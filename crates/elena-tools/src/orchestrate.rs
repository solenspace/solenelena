//! Batch execution of tool calls with concurrent-vs-serial partitioning.
//!
//! The rule, mirroring the reference TS implementation (see
//! `services/tools/toolOrchestration.ts`):
//!
//! - Consecutive calls whose tools return `is_concurrent_safe(input) == true`
//!   group into a batch and run in parallel under a semaphore (default 10).
//! - Any call whose tool is *not* concurrent-safe flushes the current batch
//!   and then runs alone before the next batch begins.
//! - Result order matches invocation order regardless of the underlying
//!   schedule.
//!
//! Progress events emitted by tools (via [`ToolContext::progress_tx`]) are
//! interleaved on the shared channel as they happen; the loop driver fans
//! them out to the client stream.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use elena_types::{StreamEvent, TenantContext, ThreadId, ToolCallId, ToolError};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    context::ToolContext,
    registry::ToolRegistry,
    tool::{Tool, ToolInvocation, ToolOutput},
};

/// Per-call credential map: `tool_call_id` → `{key: value}`.
///
/// The loop driver pre-resolves each plugin tool's credentials from
/// `tenant_credentials` (decrypting in-memory) before calling
/// [`execute_batch`]; the orchestrator copies the resolved map into each
/// [`ToolContext`] without itself touching the store. Built-in tools
/// receive an empty map.
pub type PerCallCredentials = HashMap<ToolCallId, BTreeMap<String, String>>;

/// One classified call waiting to run.
struct Classified {
    idx: usize,
    call: ToolInvocation,
    tool: Option<Arc<dyn Tool>>,
    concurrent: bool,
}

/// The default concurrency cap — matches the reference TS default of
/// `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`.
pub const DEFAULT_MAX_CONCURRENT: usize = 10;

/// Options for [`execute_batch`].
#[derive(Debug, Clone)]
pub struct ExecuteBatchOptions {
    /// Maximum number of concurrent-safe tools that may run in parallel.
    pub max_concurrent: usize,
    /// Per-tool-call decrypted credentials. Wrapped in `Arc` so the map
    /// is shared zero-cost across spawned tasks. Default: empty.
    pub credentials: Arc<PerCallCredentials>,
}

impl Default for ExecuteBatchOptions {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            credentials: Arc::new(PerCallCredentials::new()),
        }
    }
}

/// Run a batch of tool invocations, preserving invocation order in the
/// output. See the module docs for the scheduling rule.
///
/// Returns a vector of `(tool_call_id, result)` pairs in the same order as
/// `calls`. `NotFound` is returned for any invocation whose name is not in
/// `registry`; a `Deny` permission result is mapped to
/// [`ToolError::Execution`].
///
/// Cancellation: if `cancel` fires, any in-flight tools see the same token
/// via their [`ToolContext`] and can abort; results already produced are
/// returned, pending invocations receive [`ToolError::Execution`] with an
/// "aborted" message.
pub async fn execute_batch(
    calls: Vec<ToolInvocation>,
    registry: &ToolRegistry,
    tenant: &TenantContext,
    thread_id: ThreadId,
    cancel: CancellationToken,
    progress_tx: mpsc::Sender<StreamEvent>,
    options: ExecuteBatchOptions,
) -> Vec<(ToolCallId, Result<ToolOutput, ToolError>)> {
    let n = calls.len();
    let mut results: Vec<Option<Result<ToolOutput, ToolError>>> = (0..n).map(|_| None).collect();
    let mut ids: Vec<ToolCallId> = calls.iter().map(|c| c.id).collect();
    // `ids` is immutable — we fill `results` by index and zip with `ids` at the end.
    let _ = &mut ids;

    // Classify each call: either look up the tool and its concurrency hint, or
    // mark `None` if the tool is missing.
    let mut classified: Vec<Classified> = Vec::with_capacity(n);
    for (idx, call) in calls.into_iter().enumerate() {
        let tool = registry.get(&call.name);
        let concurrent = tool.as_ref().is_some_and(|t| t.is_concurrent_safe(&call.input));
        classified.push(Classified { idx, call, tool, concurrent });
    }

    let semaphore = Arc::new(Semaphore::new(options.max_concurrent.max(1)));

    while !classified.is_empty() {
        if cancel.is_cancelled() {
            for item in classified.drain(..) {
                results[item.idx] = Some(Err(ToolError::Execution {
                    message: format!("aborted before {} ran", item.call.name),
                }));
            }
            break;
        }

        // Gather a greedy run of concurrent-safe tools from the front of the
        // queue. `drain`/`remove` below shift the tail down, so we always
        // work at index 0.
        let mut batch_end = 0;
        while batch_end < classified.len() && classified[batch_end].concurrent {
            batch_end += 1;
        }

        if batch_end > 0 {
            let mut joinset: tokio::task::JoinSet<(usize, Result<ToolOutput, ToolError>)> =
                tokio::task::JoinSet::new();
            for item in classified.drain(..batch_end) {
                let sem = Arc::clone(&semaphore);
                let tenant = tenant.clone();
                let cancel = cancel.clone();
                let progress_tx = progress_tx.clone();
                let creds = options.credentials.get(&item.call.id).cloned().unwrap_or_default();
                joinset.spawn(async move {
                    let Ok(_permit) = sem.acquire_owned().await else {
                        return (
                            item.idx,
                            Err(ToolError::Execution { message: "semaphore closed".to_owned() }),
                        );
                    };
                    let ctx = ToolContext {
                        tenant,
                        thread_id,
                        tool_call_id: item.call.id,
                        cancel,
                        progress_tx,
                        credentials: creds,
                    };
                    let outcome = match item.tool {
                        Some(t) => run_single(&*t, item.call, ctx).await,
                        None => Err(ToolError::NotFound { name: item.call.name }),
                    };
                    (item.idx, outcome)
                });
            }
            while let Some(join) = joinset.join_next().await {
                match join {
                    Ok((idx, outcome)) => results[idx] = Some(outcome),
                    Err(e) => {
                        warn!("tool batch task panicked: {e}");
                    }
                }
            }
        } else {
            let item = classified.remove(0);
            let creds = options.credentials.get(&item.call.id).cloned().unwrap_or_default();
            let ctx = ToolContext {
                tenant: tenant.clone(),
                thread_id,
                tool_call_id: item.call.id,
                cancel: cancel.clone(),
                progress_tx: progress_tx.clone(),
                credentials: creds,
            };
            let outcome = match item.tool {
                Some(t) => run_single(&*t, item.call, ctx).await,
                None => Err(ToolError::NotFound { name: item.call.name }),
            };
            results[item.idx] = Some(outcome);
        }
    }

    ids.into_iter()
        .zip(results)
        .map(|(id, slot)| {
            let outcome = slot.unwrap_or_else(|| {
                Err(ToolError::Execution { message: "tool did not run".to_owned() })
            });
            (id, outcome)
        })
        .collect()
}

async fn run_single(
    tool: &dyn Tool,
    call: ToolInvocation,
    ctx: ToolContext,
) -> Result<ToolOutput, ToolError> {
    // Permission check. Deny → infra error. Ask → allow (a prompt
    // channel for routing approvals is not yet wired here; the loop's
    // autonomy modes are the current pause-for-approval surface).
    let permission = tool.check_permission(&call.input, &ctx.tenant.permissions).await;
    match permission {
        elena_types::Permission::Allow { .. } => {}
        elena_types::Permission::Deny { message, .. } => {
            return Err(ToolError::Execution { message: format!("permission denied: {message}") });
        }
        elena_types::Permission::Ask { .. } => {
            // PHASE5: route to prompt channel once the gateway lands. For now
            // we auto-allow and log at WARN so the audit trail captures it.
            warn!(tool = tool.name(), "Permission::Ask auto-allowed (no prompt channel here)");
        }
    }

    tool.execute(call.input, ctx).await
}

#[cfg(test)]
#[allow(clippy::unnecessary_literal_bound)]
mod tests {
    use std::{
        sync::atomic::{AtomicU32, Ordering},
        time::Duration,
    };

    use async_trait::async_trait;
    use elena_types::{
        BudgetLimits, PermissionSet, SessionId, StreamEvent, TenantContext, TenantId, TenantTier,
        ThreadId, ToolCallId, ToolError, UserId, WorkspaceId,
    };
    use serde_json::{Value, json};
    use tokio::{sync::mpsc, time::sleep};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{tool::Tool, tool::ToolInvocation, tool::ToolOutput};

    fn tenant() -> TenantContext {
        TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::default(),
            tier: TenantTier::Pro,
            plan: None,
            metadata: std::collections::HashMap::new(),
        }
    }

    /// Tool whose `execute` awaits a delay and tracks peak concurrency via a
    /// shared atomic. Tests assert on that atomic to decide if calls ran in
    /// parallel.
    #[derive(Clone)]
    struct TrackingTool {
        name: &'static str,
        concurrent: bool,
        delay_ms: u64,
        in_flight: Arc<AtomicU32>,
        peak: Arc<AtomicU32>,
        schema: Value,
    }

    impl TrackingTool {
        fn new(name: &'static str, concurrent: bool, peak: Arc<AtomicU32>) -> Self {
            Self {
                name,
                concurrent,
                delay_ms: 40,
                in_flight: Arc::new(AtomicU32::new(0)),
                peak,
                schema: json!({"type": "object"}),
            }
        }
    }

    #[async_trait]
    impl Tool for TrackingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "tracking test tool"
        }
        fn input_schema(&self) -> &Value {
            &self.schema
        }
        fn is_read_only(&self, _: &Value) -> bool {
            self.concurrent
        }
        async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            let mut peak = self.peak.load(Ordering::SeqCst);
            while now > peak {
                match self.peak.compare_exchange(peak, now, Ordering::SeqCst, Ordering::SeqCst) {
                    Ok(_) => break,
                    Err(cur) => peak = cur,
                }
            }
            sleep(Duration::from_millis(self.delay_ms)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolOutput::text(self.name))
        }
    }

    fn invocation(name: &str) -> ToolInvocation {
        ToolInvocation { id: ToolCallId::new(), name: name.to_owned(), input: json!({}) }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_tools_run_in_parallel() {
        let reg = ToolRegistry::new();
        let peak = Arc::new(AtomicU32::new(0));
        reg.register(TrackingTool::new("read", true, Arc::clone(&peak)));

        let calls = (0..6).map(|_| invocation("read")).collect();
        let (tx, _rx) = mpsc::channel::<StreamEvent>(16);

        let results = execute_batch(
            calls,
            &reg,
            &tenant(),
            ThreadId::new(),
            CancellationToken::new(),
            tx,
            ExecuteBatchOptions { max_concurrent: 3, ..Default::default() },
        )
        .await;

        assert_eq!(results.len(), 6);
        for (_, outcome) in &results {
            assert!(outcome.is_ok());
        }
        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(observed_peak >= 2, "expected parallel execution, observed peak {observed_peak}");
        assert!(observed_peak <= 3, "semaphore should cap at 3, observed peak {observed_peak}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn write_tools_run_serially_and_flush_batch() {
        let reg = ToolRegistry::new();
        let peak = Arc::new(AtomicU32::new(0));
        reg.register(TrackingTool::new("read", true, Arc::clone(&peak)));
        reg.register(TrackingTool::new("write", false, Arc::clone(&peak)));

        // Sequence: read, read, write, read — write should flush the first
        // batch and force serialization.
        let calls =
            vec![invocation("read"), invocation("read"), invocation("write"), invocation("read")];

        let (tx, _rx) = mpsc::channel::<StreamEvent>(16);
        let results = execute_batch(
            calls,
            &reg,
            &tenant(),
            ThreadId::new(),
            CancellationToken::new(),
            tx,
            ExecuteBatchOptions::default(),
        )
        .await;

        assert_eq!(results.len(), 4);
        for (_, outcome) in &results {
            assert!(outcome.is_ok(), "unexpected error: {outcome:?}");
        }
        // Peak should be <= 2 because the `write` tool can never run
        // alongside anything else.
        let observed_peak = peak.load(Ordering::SeqCst);
        assert!(observed_peak <= 2, "write tool must be serial, observed peak {observed_peak}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_tool_returns_not_found() {
        let reg = ToolRegistry::new();
        let calls = vec![invocation("ghost")];
        let (tx, _rx) = mpsc::channel::<StreamEvent>(4);

        let results = execute_batch(
            calls,
            &reg,
            &tenant(),
            ThreadId::new(),
            CancellationToken::new(),
            tx,
            ExecuteBatchOptions::default(),
        )
        .await;

        assert_eq!(results.len(), 1);
        let err = results.into_iter().next().unwrap().1.unwrap_err();
        assert!(matches!(err, ToolError::NotFound { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn results_preserve_invocation_order() {
        let reg = ToolRegistry::new();
        let peak = Arc::new(AtomicU32::new(0));
        reg.register(TrackingTool::new("a", true, Arc::clone(&peak)));
        reg.register(TrackingTool::new("b", true, Arc::clone(&peak)));
        reg.register(TrackingTool::new("c", false, Arc::clone(&peak)));

        let calls = vec![
            invocation("a"),
            invocation("b"),
            invocation("c"),
            invocation("a"),
            invocation("b"),
        ];
        let expected_ids: Vec<ToolCallId> = calls.iter().map(|c| c.id).collect();

        let (tx, _rx) = mpsc::channel::<StreamEvent>(16);
        let results = execute_batch(
            calls,
            &reg,
            &tenant(),
            ThreadId::new(),
            CancellationToken::new(),
            tx,
            ExecuteBatchOptions::default(),
        )
        .await;

        let got_ids: Vec<ToolCallId> = results.iter().map(|(id, _)| *id).collect();
        assert_eq!(got_ids, expected_ids);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pre_cancelled_batch_returns_aborted_for_all() {
        let reg = ToolRegistry::new();
        let peak = Arc::new(AtomicU32::new(0));
        reg.register(TrackingTool::new("read", true, Arc::clone(&peak)));

        let calls = (0..3).map(|_| invocation("read")).collect();
        let (tx, _rx) = mpsc::channel::<StreamEvent>(4);
        let token = CancellationToken::new();
        token.cancel();

        let results = execute_batch(
            calls,
            &reg,
            &tenant(),
            ThreadId::new(),
            token,
            tx,
            ExecuteBatchOptions::default(),
        )
        .await;

        assert_eq!(results.len(), 3);
        for (_, outcome) in &results {
            match outcome {
                Err(ToolError::Execution { message }) => {
                    assert!(message.contains("aborted"));
                }
                other => panic!("expected Execution err, got {other:?}"),
            }
        }
    }
}
