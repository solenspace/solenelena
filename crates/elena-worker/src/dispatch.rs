//! Per-`WorkRequest` handler.
//!
//! Steps:
//! 1. Claim the thread (Redis CAS). Already-claimed → ack + skip.
//! 2. Idempotently persist the user message.
//! 3. Build a `LoopState` and call `run_loop`.
//! 4. Pipe every emitted `StreamEvent` to NATS.
//! 5. Subscribe in parallel to `elena.thread.{id}.abort` so the gateway
//!    can cancel a turn mid-flight.
//! 6. Ack the `JetStream` message after `run_loop` finishes and the
//!    publisher has flushed.

use std::sync::Arc;

use elena_core::{LoopState, run_loop};
use elena_store::RateLimiter;
use elena_types::{ModelId, WorkRequest, WorkRequestKind, subjects};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::publisher::pump_events;

/// S5 — TTL on the per-`request_id` idempotency key. Must outlive the
/// longest plausible loop run so a `JetStream` redelivery during a
/// long-running turn still hits the dedup guard. 24h is generous
/// against `max_turns × per-turn deadline` defaults.
const REQUEST_ID_TTL_SECONDS: u64 = 24 * 3600;

/// C8 — RAII guard around an acquired inflight slot. The slot is
/// released exactly once: either by `release()` (the happy path) or by
/// `Drop` (a panic, an early `return`, or a future being dropped before
/// completion). The drop path spawns the async release onto the
/// runtime via the captured handle; if the runtime is shutting down
/// the spawn fails and the slot is left for the next `acquire`'s
/// natural clamp-on-overflow to self-heal.
struct InflightGuard {
    key: Option<String>,
    rate_limits: RateLimiter,
    runtime: tokio::runtime::Handle,
}

impl InflightGuard {
    fn new(key: String, rate_limits: RateLimiter, runtime: tokio::runtime::Handle) -> Self {
        Self { key: Some(key), rate_limits, runtime }
    }

    /// Awaitable release that consumes the guard. Errors are logged
    /// rather than returned — a release failure shouldn't propagate
    /// up through the dispatch path.
    async fn release(mut self) {
        if let Some(key) = self.key.take() {
            if let Err(e) = self.rate_limits.release_inflight(&key).await {
                warn!(?e, key, "release_inflight failed");
            }
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let rate_limits = self.rate_limits.clone();
            // Best-effort fire-and-forget. Drop the JoinHandle so the
            // task runs detached. If the runtime is shutting down the
            // spawn fails silently; the next acquire's overflow clamp
            // eventually reconciles the count.
            drop(self.runtime.spawn(async move {
                if let Err(e) = rate_limits.release_inflight(&key).await {
                    warn!(?e, key, "release_inflight (Drop) failed");
                }
            }));
        }
    }
}

/// Entry point. Cancellable via the worker-level token; the per-thread
/// `abort` subject hooks an inner token that's cancelled on receipt.
#[allow(clippy::too_many_lines)]
pub async fn handle(
    nats: async_nats::Client,
    deps: Arc<elena_core::LoopDeps>,
    worker_id: String,
    request: WorkRequest,
    parent_cancel: CancellationToken,
) {
    let thread_id = request.thread_id;

    // S5 — JetStream idempotency guard. A worker that crashes after
    // starting handle() but before ack will see this same request
    // redelivered; the SETNX guard short-circuits the redelivery so
    // we don't burn a rate-limit token, re-claim the thread, or
    // double-process work. Redis errors fall open.
    match deps.store.cache.try_claim_request(request.request_id, REQUEST_ID_TTL_SECONDS).await {
        Ok(true) => {}
        Ok(false) => {
            info!(
                request_id = %request.request_id,
                %thread_id,
                "duplicate WorkRequest delivery suppressed"
            );
            return;
        }
        Err(e) => warn!(?e, "request_id idempotency guard failed; allowing through"),
    }

    // 0. Rate-limit gate. Check per-tenant RPM before touching the DB.
    //    Skipped entirely when the operator left limits unset (u32::MAX).
    if deps.rate_limits.tenant_rpm != u32::MAX {
        let key = elena_store::tenant_rpm_key(request.tenant.tenant_id);
        let rate_per_sec = (deps.rate_limits.tenant_rpm / 60).max(1);
        let burst = deps.rate_limits.effective_tenant_burst();
        match deps.store.rate_limits.try_take(&key, rate_per_sec, burst).await {
            Ok(elena_store::RateDecision::Allow { .. }) => {}
            Ok(elena_store::RateDecision::Deny { retry_after_ms }) => {
                deps.metrics.rate_limit_rejections_total.with_label_values(&["tenant_rpm"]).inc();
                warn!(
                    %thread_id,
                    tenant = %request.tenant.tenant_id,
                    retry_after_ms,
                    "rate-limited; skipping turn"
                );
                return;
            }
            Err(e) => warn!(?e, "rate-limit check failed; allowing through"),
        }
    }

    // D5 — per-tenant inflight cap. Counts concurrent in-flight turns
    // for the tenant; the third concurrent device-session is rejected
    // when `tenant_inflight_max` is set. C8 — release runs through an
    // RAII guard so a panic / early `return` / dropped JoinHandle
    // can't leak a slot.
    let inflight_guard = if deps.rate_limits.tenant_inflight_max < u32::MAX {
        let key = elena_store::tenant_inflight_key(request.tenant.tenant_id);
        match deps
            .store
            .rate_limits
            .acquire_inflight(&key, deps.rate_limits.tenant_inflight_max)
            .await
        {
            Ok(elena_store::RateDecision::Allow { .. }) => Some(InflightGuard::new(
                key,
                deps.store.rate_limits.clone(),
                tokio::runtime::Handle::current(),
            )),
            Ok(elena_store::RateDecision::Deny { .. }) => {
                deps.metrics
                    .rate_limit_rejections_total
                    .with_label_values(&["tenant_inflight"])
                    .inc();
                warn!(
                    %thread_id,
                    tenant = %request.tenant.tenant_id,
                    cap = deps.rate_limits.tenant_inflight_max,
                    "tenant inflight cap hit; skipping turn"
                );
                return;
            }
            Err(e) => {
                warn!(?e, "inflight acquire failed; allowing through");
                None
            }
        }
    } else {
        None
    };

    // 1. Claim. If lost, another worker has it — bail (still OK to ack).
    match deps.store.cache.claim_thread(thread_id, &worker_id).await {
        Ok(true) => {}
        Ok(false) => {
            info!(%thread_id, "thread already claimed by another worker; skipping");
            return;
        }
        Err(e) => {
            warn!(?e, %thread_id, "claim_thread failed; dropping request");
            return;
        }
    }

    // 2. Idempotent user-message append. Skipped on an approval resume:
    //    the user message already exists from the original turn and the
    //    `request.message` placeholder should not be persisted again.
    if matches!(request.kind, WorkRequestKind::Turn) {
        if let Err(e) = deps.store.threads.append_message_idempotent(&request.message).await {
            warn!(?e, %thread_id, "append_message_idempotent failed; aborting");
            let _ = deps.store.cache.release_thread(thread_id, &worker_id).await;
            return;
        }
    } else {
        info!(%thread_id, "resume-from-approval — skipping user-message append");
    }

    // 3. Build LoopState. Caller-supplied overrides win; otherwise sensible
    //    defaults. The checkpoint loaded inside `run_loop` overrides this
    //    fresh state for resumes.
    let max_turns = request.max_turns.unwrap_or(deps.defaults.default_max_turns);
    let max_tokens = request.max_tokens_per_turn.unwrap_or(2_048);
    let model = request.model.clone().unwrap_or_else(|| ModelId::new("placeholder"));
    // C2 — pre-load cumulative per-thread usage so the budget check at
    // step.rs:784 evaluates against the real per-thread total, not just
    // this turn's delta. Skipped on `ResumeFromApproval` because the
    // checkpoint already carries the correct cumulative figure.
    let prior_thread_tokens = if matches!(request.kind, WorkRequestKind::Turn) {
        deps.store.tenants.get_thread_usage(request.tenant.tenant_id, thread_id).await.unwrap_or(0)
    } else {
        0
    };
    let mut state = LoopState::new(thread_id, request.tenant.clone(), model, max_turns, max_tokens)
        .with_autonomy(request.autonomy);
    state.system_prompt = request.system_prompt;
    state.prior_thread_tokens = prior_thread_tokens;

    // 4. Start the abort listener.
    let inner_cancel = parent_cancel.child_token();
    let abort_subject = subjects::thread_abort(thread_id);
    let abort_token = inner_cancel.clone();
    let abort_nats = nats.clone();
    let abort_task = tokio::spawn(async move {
        match abort_nats.subscribe(abort_subject.clone()).await {
            Ok(mut sub) => {
                if sub.next().await.is_some() {
                    info!(subject = %abort_subject, "abort signal received");
                    abort_token.cancel();
                }
            }
            Err(e) => warn!(?e, "abort subscribe failed"),
        }
    });

    // 5. Drive the loop and pipe events to NATS.
    let (handle, stream) = run_loop(state, Arc::clone(&deps), worker_id.clone(), inner_cancel);
    pump_events(&nats, &deps.store.cache, thread_id, stream).await;
    if let Err(e) = handle.await {
        warn!(?e, %thread_id, "run_loop join failed");
    }

    // 6. Tear down the abort listener.
    abort_task.abort();

    // 7. D5 — release the inflight slot via the RAII guard. The
    //    happy-path explicit release is async and joins cleanly; the
    //    Drop fallback covers any panic / early-return upstream of
    //    here.
    if let Some(guard) = inflight_guard {
        guard.release().await;
    }
}
