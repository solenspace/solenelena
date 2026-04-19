//! [`run_loop`] — the outer driver around [`step`].
//!
//! Responsibilities:
//!
//! - Claim the thread before the first step (atomic against other workers).
//! - Resume from a Redis checkpoint if one exists.
//! - Advance [`LoopState`] one phase at a time via [`step`] until it
//!   returns `Terminal`.
//! - Save a checkpoint after each successful step.
//! - Release the claim and drop the checkpoint on clean termination.
//! - Surface unrecoverable errors via `StreamEvent::Error(..)` + a matching
//!   `Done(Terminal)`.

use std::sync::Arc;

use elena_types::{ElenaError, LlmApiError, LlmApiErrorKind, StreamEvent, Terminal};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    checkpoint::{drop_loop_state, load_loop_state, save_loop_state},
    deps::LoopDeps,
    state::LoopState,
    step::{StepOutcome, step},
};

/// Buffer size for the outbound [`StreamEvent`] channel.
const EVENT_CHANNEL_CAP: usize = 128;

/// Distinguishes a terminal exit (loop done — emit Done, drop checkpoint)
/// from a paused exit (client approval pending — keep checkpoint, no
/// Done event, release claim so a resuming worker can pick it up).
enum Exit {
    Terminal(Terminal),
    Paused { deadline_ms: i64 },
}

/// Drive a loop from `initial` to a terminal condition.
///
/// Returns `(handle, stream)`:
/// - `handle` resolves to the final [`Terminal`] once the loop exits.
/// - `stream` emits [`StreamEvent`]s for the caller (gateway / smoke binary)
///   to fan out. Includes the final `Done(terminal)` event.
///
/// Cancellation: firing `cancel` aborts the loop at the next await point
/// and produces `Terminal::AbortedStreaming`.
#[must_use]
pub fn run_loop(
    initial: LoopState,
    deps: Arc<LoopDeps>,
    worker_id: String,
    cancel: CancellationToken,
) -> (JoinHandle<Terminal>, ReceiverStream<StreamEvent>) {
    let (tx, rx) = mpsc::channel::<StreamEvent>(EVENT_CHANNEL_CAP);
    let handle = tokio::spawn(drive_loop(initial, deps, worker_id, cancel, tx));
    (handle, ReceiverStream::new(rx))
}

async fn drive_loop(
    mut state: LoopState,
    deps: Arc<LoopDeps>,
    worker_id: String,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEvent>,
) -> Terminal {
    // Try to claim the thread. If another worker already owns it, bail
    // cleanly — the other worker will emit the Done event on that channel.
    match deps.store.cache.claim_thread(state.thread_id, &worker_id).await {
        Ok(true) => {
            // We're the owner.
        }
        Ok(false) => {
            warn!(
                thread_id = %state.thread_id,
                worker_id = %worker_id,
                "thread already claimed by another worker; bailing"
            );
            let term = Terminal::BlockingLimit;
            let _ = tx.send(StreamEvent::Done(term.clone())).await;
            return term;
        }
        Err(e) => {
            error!(?e, "claim_thread failed");
            return emit_internal_failure(&tx, LlmApiErrorKind::Unknown, e.to_string()).await;
        }
    }

    // Resume from a checkpoint if one exists (we just claimed, so the
    // checkpoint is either stale-from-this-worker or a recovery of a
    // crashed predecessor's progress).
    match load_loop_state(&deps.store.cache, state.thread_id).await {
        Ok(Some(resumed)) => {
            info!(
                thread_id = %state.thread_id,
                phase = resumed.phase.tag(),
                "resuming from checkpoint"
            );
            state = resumed;
        }
        Ok(None) => {
            // No checkpoint — start from `initial`.
        }
        Err(e) => {
            warn!(?e, "load_loop_state failed; starting fresh");
        }
    }

    // Phase 7 observability: one `turns_total` increment per run, wall-clock
    // histogram labelled by outcome.
    deps.metrics.turns_total.inc();
    deps.metrics.loops_in_flight.with_label_values(&[worker_id.as_str()]).inc();
    let turn_start = std::time::Instant::now();

    let exit = loop {
        match step(&mut state, &deps, &cancel, &tx).await {
            Ok(StepOutcome::Continue) => {
                if let Err(e) = save_loop_state(&deps.store.cache, &state).await {
                    warn!(?e, "save_loop_state failed; continuing without checkpoint");
                }
            }
            Ok(StepOutcome::Terminal(t)) => break Exit::Terminal(t),
            Ok(StepOutcome::Paused { deadline_ms }) => {
                // Persist the checkpoint one more time so the resumer
                // reads the freshest phase (AwaitingApproval with the
                // pending list populated).
                if let Err(e) = save_loop_state(&deps.store.cache, &state).await {
                    warn!(?e, "save_loop_state before pause failed");
                }
                break Exit::Paused { deadline_ms };
            }
            Err(err) => break Exit::Terminal(classify_error(&tx, &err).await),
        }
    };

    deps.metrics.loops_in_flight.with_label_values(&[worker_id.as_str()]).dec();

    match exit {
        Exit::Paused { deadline_ms } => {
            info!(
                thread_id = %state.thread_id,
                deadline_ms,
                "loop parked awaiting approval; releasing claim with checkpoint intact"
            );
            // Release the claim so the resuming worker (post-approval)
            // can pick the thread back up. Deliberately NO
            // `drop_loop_state` and NO `Done` — the stream stays
            // logically open from the client's perspective.
            let _ = deps.store.cache.release_thread(state.thread_id, &worker_id).await;
            // The JoinHandle result is internal-only; pick a neutral
            // Terminal that isn't `is_success`.
            Terminal::BlockingLimit
        }
        Exit::Terminal(terminal) => {
            let outcome_label = if terminal.is_success() { "ok" } else { "error" };
            deps.metrics
                .turn_duration_seconds
                .with_label_values(&[outcome_label])
                .observe(turn_start.elapsed().as_secs_f64());

            // Phase 4: fire-and-forget episode recording before cleanup. Errors
            // are swallowed by `EpisodicMemory::record_episode` itself.
            spawn_episode_record(&deps, &state, &terminal);

            // Best-effort cleanup. Errors don't block termination.
            let _ = drop_loop_state(&deps.store.cache, state.thread_id).await;
            let _ = deps.store.cache.release_thread(state.thread_id, &worker_id).await;

            // Always emit the final Done event — phases don't emit it themselves;
            // they just set `phase = Failed { .. }` and return the terminal value.
            let _ = tx.send(StreamEvent::Done(terminal.clone())).await;
            terminal
        }
    }
}

fn spawn_episode_record(deps: &Arc<LoopDeps>, state: &LoopState, terminal: &Terminal) {
    let outcome = if terminal.is_success() {
        elena_types::Outcome::Completed
    } else {
        elena_types::Outcome::Failed { terminal: terminal.clone() }
    };
    let deps = Arc::clone(deps);
    let tenant_id = state.tenant.tenant_id;
    let workspace_id = state.tenant.workspace_id;
    let thread_id = state.thread_id;
    tokio::spawn(async move {
        let embedder = deps.context.embedder();
        deps.memory
            .record_episode(
                tenant_id,
                workspace_id,
                thread_id,
                &deps.store.threads,
                &*embedder,
                outcome,
            )
            .await;
    });
}

/// Map an `ElenaError` raised from inside a phase into a `Terminal` and emit
/// a matching `Error` event so the caller sees the detail.
async fn classify_error(tx: &mpsc::Sender<StreamEvent>, err: &ElenaError) -> Terminal {
    // Emit the structured error first so clients can render it.
    let _ = tx.send(StreamEvent::Error(err.clone())).await;

    match err {
        ElenaError::Aborted => Terminal::AbortedStreaming,
        ElenaError::LlmApi(llm_err) => {
            if matches!(llm_err, LlmApiError::Aborted) {
                Terminal::AbortedStreaming
            } else {
                Terminal::ModelError { classified: llm_err.kind() }
            }
        }
        ElenaError::BudgetExceeded { .. } => Terminal::BlockingLimit,
        ElenaError::ContextOverflow { .. } => Terminal::PromptTooLong,
        ElenaError::PermissionDenied { .. }
        | ElenaError::Tool(_)
        | ElenaError::Store(_)
        | ElenaError::Config(_) => Terminal::ModelError { classified: LlmApiErrorKind::Unknown },
    }
}

async fn emit_internal_failure(
    tx: &mpsc::Sender<StreamEvent>,
    classified: LlmApiErrorKind,
    message: String,
) -> Terminal {
    let _ = tx.send(StreamEvent::Error(ElenaError::LlmApi(LlmApiError::Unknown { message }))).await;
    let term = Terminal::ModelError { classified };
    let _ = tx.send(StreamEvent::Done(term.clone())).await;
    term
}
