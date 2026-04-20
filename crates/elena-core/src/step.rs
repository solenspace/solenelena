//! One-step phase dispatcher for the agentic loop.
//!
//! [`step`] consumes the current [`LoopState::phase`], performs one phase
//! transition (streaming an LLM turn, running tool calls, or
//! post-processing bookkeeping), and updates [`LoopState`] in place. The
//! outer driver ([`crate::loop_driver::run_loop`]) calls it in a loop until
//! it returns [`StepOutcome::Terminal`].
//!
//! Error handling policy: store / LLM / tool infrastructure failures
//! propagate as `Err(ElenaError)`. Logical failures inside a tool are
//! captured as `is_error: true` on the tool-result message and the loop
//! continues. Budget exhaustion and max-turns tripping return
//! [`StepOutcome::Terminal`] cleanly.

use chrono::Utc;
use elena_router::{CascadeDecision, CascadeInputs, RoutingContext};
use elena_tools::{ExecuteBatchOptions, ToolInvocation, execute_batch};
use elena_types::{
    ApprovalDecision, ApprovalVerdict, ContentBlock, ElenaError, Message, MessageId, MessageKind,
    PendingApproval, Role, StreamEvent, Terminal, ToolCallId, ToolResultContent,
};
use std::collections::HashSet;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{
    deps::LoopDeps,
    dispatch_decision::requires_approval,
    request_builder::build_llm_request,
    state::{LoopPhase, LoopState},
    stream_consumer::{ConsumedStream, consume_stream},
};

/// How long the loop will wait for client approval before timing out.
/// Matches the value documented in the Phase 7 plan (Cautious mode).
const APPROVAL_DEADLINE_SECS: i64 = 300;

/// F9 — Approval-deadline check.
///
/// Returns `true` when the deadline has lapsed and the
/// [`LoopPhase::AwaitingApproval`] phase should transition to
/// [`Terminal::BlockingLimit`]. Pure function so the contract can be
/// unit-tested without standing up a full [`LoopDeps`] mock.
#[must_use]
pub fn approval_deadline_lapsed(now_ms: i64, deadline_ms: i64) -> bool {
    now_ms >= deadline_ms
}

/// The outcome of one [`step`] call.
#[derive(Debug)]
pub enum StepOutcome {
    /// The loop should advance to the next step.
    Continue,
    /// The loop finished; `Terminal` is the reason.
    Terminal(Terminal),
    /// The loop is waiting for external input (a client approval posted to
    /// the gateway). The driver should release the thread claim and keep
    /// the checkpoint intact so a resuming worker can pick up after the
    /// client responds.
    Paused {
        /// Unix epoch millis after which the pause times out. Resuming
        /// workers compare this against the current time; if the deadline
        /// has passed, the loop transitions to `Terminal::BlockingLimit`.
        deadline_ms: i64,
    },
}

/// Advance the loop one phase.
pub async fn step(
    state: &mut LoopState,
    deps: &LoopDeps,
    cancel: &CancellationToken,
    event_tx: &mpsc::Sender<StreamEvent>,
) -> Result<StepOutcome, ElenaError> {
    if cancel.is_cancelled() {
        return Ok(StepOutcome::Terminal(Terminal::AbortedStreaming));
    }

    let phase_tag = state.phase.tag();
    debug!(phase = phase_tag, thread = %state.thread_id, "step");

    match state.phase.clone() {
        LoopPhase::Received => handle_received(state, deps, event_tx).await,
        LoopPhase::Streaming => handle_streaming(state, deps, cancel, event_tx).await,
        LoopPhase::ExecutingTools { remaining } => {
            handle_executing_tools(state, deps, cancel, event_tx, remaining).await
        }
        LoopPhase::AwaitingApproval { invocations, deadline_ms, .. } => {
            handle_awaiting_approval(state, deps, event_tx, invocations, deadline_ms).await
        }
        LoopPhase::PostProcessing => handle_post_processing(state, deps, event_tx).await,
        LoopPhase::Completed => Ok(StepOutcome::Terminal(Terminal::Completed)),
        LoopPhase::Failed { reason } => Ok(StepOutcome::Terminal(reason)),
    }
}

/// While in [`LoopPhase::AwaitingApproval`] the loop tries to resume from
/// the client's persisted decisions. Behaviour:
///
/// 1. Deadline lapsed → transition to `Failed { Terminal::BlockingLimit }`.
/// 2. Approvals count matches pending invocations → apply edits for
///    `Allow`, synthesise denial tool-result rows for `Deny`, clear the
///    approvals rows, transition to `ExecutingTools` with the remaining
///    allowed invocations.
/// 3. Fewer approvals than pending → park again (client still answering).
async fn handle_awaiting_approval(
    state: &mut LoopState,
    deps: &LoopDeps,
    event_tx: &mpsc::Sender<StreamEvent>,
    invocations: Vec<ToolInvocation>,
    deadline_ms: i64,
) -> Result<StepOutcome, ElenaError> {
    let now = Utc::now().timestamp_millis();
    if approval_deadline_lapsed(now, deadline_ms) {
        state.phase = LoopPhase::Failed { reason: Terminal::BlockingLimit };
        return Ok(StepOutcome::Terminal(Terminal::BlockingLimit));
    }

    // Read current approvals for this thread.
    let decisions = deps
        .store
        .approvals
        .list(state.tenant.tenant_id, state.thread_id)
        .await
        .map_err(ElenaError::Store)?;

    if decisions.len() < invocations.len() {
        // Still waiting on more. Re-park.
        return Ok(StepOutcome::Paused { deadline_ms });
    }

    // All decisions in. Pair them with invocations by tool_use_id.
    let (allowed, denied) = split_decisions(&invocations, &decisions);

    // Synthesise a denial tool_result message for every denied call so
    // the model sees a response when it next looks at history. We append
    // this *now*, under the Tool role, because the subsequent
    // ExecutingTools phase will walk through only the allowed calls.
    let parent_id = latest_assistant_message_id(deps, state).await?;
    for denial in &denied {
        let msg = Message {
            id: MessageId::new(),
            thread_id: state.thread_id,
            tenant_id: state.tenant.tenant_id,
            role: Role::Tool,
            kind: MessageKind::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: denial.tool_use_id,
                content: ToolResultContent::text("denied by user"),
                is_error: true,
                cache_control: None,
            }],
            created_at: Utc::now(),
            token_count: None,
            parent_id,
        };
        deps.store.threads.append_message(&msg).await.map_err(ElenaError::Store)?;
        let _ =
            event_tx.send(StreamEvent::ToolResult { id: denial.tool_use_id, is_error: true }).await;
        deps.store
            .audit
            .emit(
                elena_store::AuditEvent::new(
                    state.tenant.tenant_id,
                    "approval_decision",
                    serde_json::json!({
                        "tool_use_id": denial.tool_use_id,
                        "decision": "deny",
                    }),
                )
                .with_thread(state.thread_id)
                .with_workspace(state.tenant.workspace_id)
                .with_actor("user"),
            )
            .await;
    }
    for allow in &allowed {
        deps.store
            .audit
            .emit(
                elena_store::AuditEvent::new(
                    state.tenant.tenant_id,
                    "approval_decision",
                    serde_json::json!({
                        "tool_use_id": allow.id,
                        "decision": "allow",
                    }),
                )
                .with_thread(state.thread_id)
                .with_workspace(state.tenant.workspace_id)
                .with_actor("user"),
            )
            .await;
    }

    // Drop approvals — we have consumed them into the phase transition.
    let _ = deps.store.approvals.clear(state.tenant.tenant_id, state.thread_id).await;

    emit_phase_change(event_tx, "awaiting_approval", "executing_tools").await;

    if allowed.is_empty() {
        // Everything denied — skip ExecutingTools, go straight to
        // PostProcessing so the model gets another streaming turn
        // against the denials.
        state.pending_tool_calls.clear();
        state.phase = LoopPhase::PostProcessing;
    } else {
        state.pending_tool_calls.clone_from(&allowed);
        state.phase = LoopPhase::ExecutingTools { remaining: allowed };
    }
    Ok(StepOutcome::Continue)
}

/// Split the model's proposed invocations by the client's decisions.
/// Returns `(allowed, denied)`. Allowed invocations get their input
/// replaced with the client's `edits` when provided.
///
/// An invocation with no matching decision is conservatively denied —
/// this only happens if the DB row set is corrupted; the count check in
/// the caller should have prevented this branch.
fn split_decisions(
    invocations: &[ToolInvocation],
    decisions: &[ApprovalDecision],
) -> (Vec<ToolInvocation>, Vec<ApprovalDecision>) {
    let mut allowed = Vec::new();
    let mut denied = Vec::new();
    for inv in invocations {
        match decisions.iter().find(|d| d.tool_use_id == inv.id) {
            Some(decision) => match decision.decision {
                ApprovalVerdict::Allow => {
                    let mut inv = inv.clone();
                    if let Some(edits) = decision.edits.clone() {
                        inv.input = edits;
                    }
                    allowed.push(inv);
                }
                ApprovalVerdict::Deny => denied.push(decision.clone()),
            },
            None => denied.push(ApprovalDecision {
                tool_use_id: inv.id,
                decision: ApprovalVerdict::Deny,
                edits: None,
            }),
        }
    }
    (allowed, denied)
}

async fn handle_received(
    state: &mut LoopState,
    deps: &LoopDeps,
    event_tx: &mpsc::Sender<StreamEvent>,
) -> Result<StepOutcome, ElenaError> {
    // Phase 4 hook: pull any relevant episodes from the tenant's workspace
    // and prepend a "prior sessions" preamble to the system prompt. This
    // happens exactly once (on the first step) and then sticks for the rest
    // of the loop.
    inject_episodic_preamble(state, deps).await;

    emit_phase_change(event_tx, "received", "streaming").await;
    state.phase = LoopPhase::Streaming;
    Ok(StepOutcome::Continue)
}

async fn inject_episodic_preamble(state: &mut LoopState, deps: &LoopDeps) {
    if !deps.context.retrieval_enabled() {
        return;
    }
    let recent = match deps
        .store
        .threads
        .list_messages(state.tenant.tenant_id, state.thread_id, 10, None)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            warn!(?e, "failed to fetch messages for episodic recall");
            return;
        }
    };
    let Some(seed) = first_user_text(&recent) else {
        return;
    };

    let episodes = match deps
        .memory
        .recall(
            state.tenant.tenant_id,
            state.tenant.workspace_id,
            &seed,
            &*deps.context.embedder(),
            3,
        )
        .await
    {
        Ok(list) => list,
        Err(e) => {
            warn!(?e, "episodic recall failed");
            return;
        }
    };
    if episodes.is_empty() {
        return;
    }

    let mut preamble = String::from("Relevant prior sessions in this workspace:\n");
    for ep in episodes {
        preamble.push_str("- ");
        preamble.push_str(&ep.task_summary);
        preamble.push('\n');
    }
    preamble.push_str("Use these as background context only; do not assume they continue here.\n");

    // Prepend as a dedicated system block so existing prompt cache markers
    // on downstream blocks stay in place.
    let mut new_prompt: Vec<ContentBlock> = Vec::with_capacity(state.system_prompt.len() + 1);
    new_prompt.push(ContentBlock::Text { text: preamble, cache_control: None });
    new_prompt.append(&mut state.system_prompt);
    state.system_prompt = new_prompt;
}

fn first_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| {
        if !matches!(m.role, Role::User) {
            return None;
        }
        let mut out = String::new();
        for block in &m.content {
            if let ContentBlock::Text { text, .. } = block {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(text);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    })
}

async fn handle_streaming(
    state: &mut LoopState,
    deps: &LoopDeps,
    cancel: &CancellationToken,
    event_tx: &mpsc::Sender<StreamEvent>,
) -> Result<StepOutcome, ElenaError> {
    // Phase 4: the query used for retrieval is the most recent user text.
    let current_query = latest_user_text(deps, state).await.unwrap_or_default();

    // Pick a tier if the caller hasn't yet dictated a specific escalation
    // via `state.model_tier`; on cascade re-entries, `model_tier` is already
    // set and the router's `route()` is skipped so escalations stick.
    let pinned_tier = state.model_tier;
    if state.recovery.model_escalations == 0 {
        let recent_tools = recent_tool_names_vec(deps, state).await;
        let ctx = RoutingContext {
            user_message: &current_query,
            conversation_depth: state.turn_count,
            tools_available: u32::try_from(deps.tools.len()).unwrap_or(u32::MAX),
            recent_tool_names: &recent_tools,
            tenant_tier: state.tenant.tier,
            error_recovery_count: state.recovery.consecutive_errors,
        };
        let tier = deps.router.route(&ctx);
        state.model_tier = tier;
        let entry = deps.router.resolve(tier);
        state.model = entry.model;
        state.provider = entry.provider;
    } else {
        // Cascade already chose — make sure model_id reflects the tier.
        let entry = deps.router.resolve(pinned_tier);
        state.model = entry.model;
        state.provider = entry.provider;
    }

    // Build the context window. `build_context` falls back to a recency
    // window when the embedder is Null.
    let messages = deps
        .context
        .build_context(
            state.tenant.tenant_id,
            state.thread_id,
            &current_query,
            &deps.store.threads,
            state.max_tokens_per_turn * 8, // rough budget: ~8× max_tokens room for input
        )
        .await
        .map_err(|e| to_elena_error(&e))?;

    // Phase 7 · A4/A5: resolve the tenant's visible plugin set and
    // filter the schema list before the model sees it. Built-ins (tools
    // whose name does not match any registered plugin prefix) always
    // pass through. Backwards compat: empty allow-lists + no ownership
    // rows degrades to "every registered tool visible".
    let schemas = filter_schemas_for_tenant(deps, state).await;
    let req = build_llm_request(state, messages, schemas);

    // Stream from the LLM.
    let stream = deps.llm.stream(req, deps.cache_policy.clone(), cancel.clone());
    let consumed = consume_stream(
        stream,
        state.tenant.tenant_id,
        state.thread_id,
        None,
        event_tx.clone(),
        cancel.clone(),
    )
    .await?;

    // Reactive cascade: decide now whether this output is acceptable. If
    // not, escalate the tier and re-enter Streaming without persisting.
    if maybe_escalate(state, deps, &consumed) {
        tracing::info!(
            thread_id = %state.thread_id,
            from = ?pinned_tier,
            to = ?state.model_tier,
            escalations = state.recovery.model_escalations,
            "cascade escalation",
        );
        // Phase stays Streaming; loop driver will save checkpoint + call
        // step() again with the new tier.
        return Ok(StepOutcome::Continue);
    }

    // Accept the turn. Commit the assistant message + embed its text so
    // it's retrievable next turn.
    deps.store.threads.append_message(&consumed.message).await?;
    if let Some(text) = extract_message_text(&consumed.message) {
        if let Err(e) = deps
            .context
            .embed_and_store(
                state.tenant.tenant_id,
                consumed.message.id,
                &text,
                &deps.store.threads,
            )
            .await
        {
            warn!(?e, message_id = %consumed.message.id, "embed_and_store failed");
        }
    }
    state.usage = std::mem::take(&mut state.usage).combine(consumed.usage);

    if consumed.tool_calls.is_empty() {
        state.last_turn_had_tools = false;
        state.phase = LoopPhase::PostProcessing;
    } else if requires_approval(state.autonomy, &consumed.tool_calls) {
        // Park the loop: publish the pending approvals, snapshot them
        // into the phase, and let the driver checkpoint + release. The
        // gateway's approvals endpoint republishes a fresh WorkRequest
        // once the client responds; a resuming worker hydrates
        // `invocations` from the checkpoint and runs the filtered batch.
        let pending = pending_from_invocations(&consumed.tool_calls);
        let deadline_ms = Utc::now().timestamp_millis() + APPROVAL_DEADLINE_SECS * 1_000;

        let _ = event_tx
            .send(StreamEvent::AwaitingApproval { pending: pending.clone(), deadline_ms })
            .await;

        state.last_turn_had_tools = true;
        state.pending_tool_calls.clone_from(&consumed.tool_calls);
        state.phase =
            LoopPhase::AwaitingApproval { invocations: consumed.tool_calls, pending, deadline_ms };
        emit_phase_change(event_tx, "streaming", "awaiting_approval").await;
    } else {
        state.last_turn_had_tools = true;
        state.pending_tool_calls.clone_from(&consumed.tool_calls);
        state.phase = LoopPhase::ExecutingTools { remaining: consumed.tool_calls };
        emit_phase_change(event_tx, "streaming", "executing_tools").await;
    }
    Ok(StepOutcome::Continue)
}

/// Build the human-friendly [`PendingApproval`] envelopes from the raw
/// tool invocations the model produced.
///
/// The summary is a best-effort one-liner derived from the input JSON;
/// clients are free to render the full payload from `input` instead.
fn pending_from_invocations(invocations: &[ToolInvocation]) -> Vec<PendingApproval> {
    invocations
        .iter()
        .map(|inv| PendingApproval {
            tool_use_id: inv.id,
            tool_name: inv.name.clone(),
            summary: summarize_input(&inv.name, &inv.input),
            input: inv.input.clone(),
        })
        .collect()
}

fn summarize_input(tool_name: &str, input: &serde_json::Value) -> String {
    match input {
        serde_json::Value::Object(map) => {
            let mut pairs: Vec<(String, String)> =
                map.iter().take(3).map(|(k, v)| (k.clone(), brief_value(v))).collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let rendered: Vec<String> =
                pairs.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{tool_name}({})", rendered.join(", "))
        }
        serde_json::Value::Null => format!("{tool_name}()"),
        other => format!("{tool_name}({})", brief_value(other)),
    }
}

fn brief_value(v: &serde_json::Value) -> String {
    const MAX_LEN: usize = 48;
    let raw = match v {
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    };
    if raw.len() > MAX_LEN {
        let mut truncated: String = raw.chars().take(MAX_LEN).collect();
        truncated.push('…');
        truncated
    } else {
        raw
    }
}

fn maybe_escalate(state: &mut LoopState, deps: &LoopDeps, consumed: &ConsumedStream) -> bool {
    let assistant_text = extract_message_text(&consumed.message).unwrap_or_default();
    let registered_owned: Vec<String> = deps
        .tools
        .schemas()
        .iter()
        .filter_map(|s| s.as_value().get("name").and_then(|v| v.as_str()).map(ToOwned::to_owned))
        .collect();
    let registered: Vec<&str> = registered_owned.iter().map(String::as_str).collect();
    let tools_present = !registered.is_empty();

    let decision = deps.router.cascade_check(CascadeInputs {
        assistant_text: &assistant_text,
        tool_calls: &consumed.tool_calls,
        registered_tool_names: &registered,
        tools_present,
        current_tier: state.model_tier,
        escalations_so_far: state.recovery.model_escalations,
        max_escalations: 2,
    });

    match decision {
        CascadeDecision::Accept => false,
        CascadeDecision::Escalate(next) => {
            state.model_tier = next;
            state.recovery.model_escalations = state.recovery.model_escalations.saturating_add(1);
            true
        }
    }
}

fn extract_message_text(message: &Message) -> Option<String> {
    let mut out = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text, .. } = block {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

async fn latest_user_text(deps: &LoopDeps, state: &LoopState) -> Option<String> {
    let msgs = deps
        .store
        .threads
        .list_messages(state.tenant.tenant_id, state.thread_id, 20, None)
        .await
        .ok()?;
    msgs.iter().rev().find_map(|m| {
        if !matches!(m.role, Role::User) {
            return None;
        }
        extract_message_text(m)
    })
}

async fn recent_tool_names_vec(deps: &LoopDeps, state: &LoopState) -> Vec<String> {
    let msgs = deps
        .store
        .threads
        .list_messages(state.tenant.tenant_id, state.thread_id, 10, None)
        .await
        .unwrap_or_default();
    let mut names = Vec::new();
    for m in msgs.iter().rev().take(3) {
        for block in &m.content {
            if let ContentBlock::ToolUse { name, .. } = block {
                names.push(name.clone());
            }
        }
    }
    names
}

fn to_elena_error(err: &elena_context::ContextError) -> ElenaError {
    match err {
        elena_context::ContextError::Store(e) => ElenaError::Store(e.clone()),
        elena_context::ContextError::LlmApi(e) => ElenaError::LlmApi(e.clone()),
        elena_context::ContextError::Embed(e) => {
            ElenaError::LlmApi(elena_types::LlmApiError::Unknown { message: e.to_string() })
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "SCRUM-103 added an idempotency guard + early-exit path; splitting the helper off would fragment a straight-line transition"
)]
async fn handle_executing_tools(
    state: &mut LoopState,
    deps: &LoopDeps,
    cancel: &CancellationToken,
    event_tx: &mpsc::Sender<StreamEvent>,
    remaining: Vec<elena_tools::ToolInvocation>,
) -> Result<StepOutcome, ElenaError> {
    // Idempotency guard. The LLM can re-propose the same `tool_use_id`
    // after seeing its own `tool_result` — llama-3.3's tool-use
    // protocol handling is not always clean. Without this filter we
    // would execute the same Slack post twice (and rack up duplicate
    // side-effects on every integration).
    //
    // Two passes:
    //   1. Skip any call whose id already has a persisted tool_result
    //      row on this thread (exact idempotency).
    //   2. Skip any call whose (tool_name, canonical_input) matches a
    //      prior *successful* tool result on this thread. This catches
    //      the moderate-mode loop where the LLM re-proposes the same
    //      logical action under a fresh `tool_use_id` after approval.
    let thread_exec =
        prior_tool_execution_index(deps, state.tenant.tenant_id, state.thread_id).await?;
    let (dupes, fresh): (Vec<_>, Vec<_>) = remaining.into_iter().partition(|c| {
        if thread_exec.ids.contains(&c.id) {
            return true;
        }
        let fp = tool_call_fingerprint(&c.name, &c.input);
        thread_exec.succeeded_fingerprints.contains(&fp)
    });
    for dup in &dupes {
        warn!(
            tool_call_id = %dup.id,
            tool_name = %dup.name,
            "skipping duplicate tool invocation — result already persisted for same id or (name,input)",
        );
    }
    if fresh.is_empty() {
        // Every call in this batch was a dupe. Force loop termination —
        // *not* a re-entry into Streaming. If we set
        // `last_turn_had_tools = true`, PostProcessing would loop back
        // to Streaming, the LLM would propose the same tool again, the
        // guard would block it again, and we'd cycle until max_turns.
        // Setting `last_turn_had_tools = false` instead routes
        // PostProcessing straight to `Terminal::Completed`. The user
        // sees the original `tool_use_start` + `tool_result` + `done` —
        // the UI's tool pill carries the success story without a
        // contradictory text response from the LLM.
        state.pending_tool_calls.clear();
        state.last_turn_had_tools = false;
        state.phase = LoopPhase::PostProcessing;
        emit_phase_change(event_tx, "executing_tools", "post_processing").await;
        return Ok(StepOutcome::Continue);
    }
    let remaining = fresh;

    // Parent ID threads tool-result rows off the assistant that requested them.
    let parent_id = latest_assistant_message_id(deps, state).await?;

    // Pre-resolve per-tenant credentials so the orchestrator can hand
    // them to the connector via gRPC metadata without itself touching
    // the credentials store. We snapshot the registered plugin prefixes
    // once so the per-call lookup is O(prefixes) and not O(SQL).
    let registered_plugin_ids: Vec<String> =
        deps.plugins.manifests().iter().map(|m| m.id.as_str().to_owned()).collect();
    let mut creds_map: elena_tools::PerCallCredentials = elena_tools::PerCallCredentials::new();
    for call in &remaining {
        let _ = event_tx.send(StreamEvent::ToolExecStart { id: call.id }).await;
        if let Some(plugin_id) = plugin_id_from_tool_name(&call.name, &registered_plugin_ids)
            && let Some(store) = &deps.store.tenant_credentials
            && let Ok(creds) = store.get_decrypted(state.tenant.tenant_id, &plugin_id).await
            && !creds.is_empty()
        {
            creds_map.insert(call.id, creds);
        }
        deps.store
            .audit
            .emit(
                elena_store::AuditEvent::new(
                    state.tenant.tenant_id,
                    "tool_use",
                    serde_json::json!({
                        "tool_use_id": call.id,
                        "tool_name": call.name,
                        "input": call.input,
                    }),
                )
                .with_thread(state.thread_id)
                .with_workspace(state.tenant.workspace_id),
            )
            .await;
    }

    let results = execute_batch(
        remaining.clone(),
        &deps.tools,
        &state.tenant,
        state.thread_id,
        cancel.clone(),
        event_tx.clone(),
        ExecuteBatchOptions {
            max_concurrent: deps.defaults.max_concurrent_tools,
            credentials: std::sync::Arc::new(creds_map),
        },
    )
    .await;

    for (tool_call_id, outcome) in results {
        let (content, is_error) = match outcome {
            Ok(output) => (output.content, output.is_error),
            Err(err) => {
                warn!(tool_call_id = %tool_call_id, ?err, "tool execution failed");
                (ToolResultContent::text(err.to_string()), true)
            }
        };
        let msg = Message {
            id: MessageId::new(),
            thread_id: state.thread_id,
            tenant_id: state.tenant.tenant_id,
            role: Role::Tool,
            kind: MessageKind::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_call_id,
                content,
                is_error,
                cache_control: None,
            }],
            created_at: Utc::now(),
            token_count: None,
            parent_id,
        };
        deps.store.threads.append_message(&msg).await?;
        let _ = event_tx.send(StreamEvent::ToolResult { id: tool_call_id, is_error }).await;
        deps.store
            .audit
            .emit(
                elena_store::AuditEvent::new(
                    state.tenant.tenant_id,
                    "tool_result",
                    serde_json::json!({
                        "tool_use_id": tool_call_id,
                        "is_error": is_error,
                    }),
                )
                .with_thread(state.thread_id)
                .with_workspace(state.tenant.workspace_id),
            )
            .await;
    }

    state.pending_tool_calls.clear();
    state.turn_count = state.turn_count.saturating_add(1);
    state.phase = LoopPhase::PostProcessing;
    emit_phase_change(event_tx, "executing_tools", "post_processing").await;
    Ok(StepOutcome::Continue)
}

async fn handle_post_processing(
    state: &mut LoopState,
    deps: &LoopDeps,
    event_tx: &mpsc::Sender<StreamEvent>,
) -> Result<StepOutcome, ElenaError> {
    // Record cumulative usage for the tenant (best-effort — store errors are
    // logged, not propagated, since we don't want a metrics blip to kill a
    // perfectly good turn).
    if let Err(e) =
        deps.store.tenants.record_usage(state.tenant.tenant_id, state.usage.clone()).await
    {
        warn!(?e, tenant = %state.tenant.tenant_id, "record_usage failed");
    }

    // Budget check.
    let used = state.usage.total();
    if used > state.tenant.budget.max_tokens_per_thread {
        let _ = event_tx
            .send(StreamEvent::Error(ElenaError::BudgetExceeded {
                tenant_id: state.tenant.tenant_id,
            }))
            .await;
        state.phase = LoopPhase::Failed { reason: Terminal::BlockingLimit };
        return Ok(StepOutcome::Terminal(Terminal::BlockingLimit));
    }

    // Max-turns check.
    if state.turn_count >= state.max_turns {
        state.phase =
            LoopPhase::Failed { reason: Terminal::MaxTurns { turn_count: state.turn_count } };
        return Ok(StepOutcome::Terminal(Terminal::MaxTurns { turn_count: state.turn_count }));
    }

    state.recovery.consecutive_errors = 0;

    if state.last_turn_had_tools {
        // Model needs to see tool results — loop back.
        state.last_turn_had_tools = false;
        state.phase = LoopPhase::Streaming;
        emit_phase_change(event_tx, "post_processing", "streaming").await;
        Ok(StepOutcome::Continue)
    } else {
        // Pure text turn — we're done.
        state.phase = LoopPhase::Completed;
        Ok(StepOutcome::Terminal(Terminal::Completed))
    }
}

async fn latest_assistant_message_id(
    deps: &LoopDeps,
    state: &LoopState,
) -> Result<Option<MessageId>, ElenaError> {
    let msgs =
        deps.store.threads.list_messages(state.tenant.tenant_id, state.thread_id, 50, None).await?;
    Ok(msgs.iter().rev().find(|m| matches!(m.kind, MessageKind::Assistant { .. })).map(|m| m.id))
}

/// Snapshot of tool-call state for idempotency checks.
struct PriorToolExecIndex {
    /// Every `tool_use_id` that already has a `ToolResult` content block.
    ids: HashSet<ToolCallId>,
    /// `tool_name + canonical_input` fingerprints for *successful* prior
    /// results. Lets us detect the "LLM re-proposes the same logical
    /// action under a new id" case, which is how the moderate-mode
    /// approval loop manifests.
    succeeded_fingerprints: HashSet<String>,
}

/// Build [`PriorToolExecIndex`] by walking the thread: assistant messages
/// carry the `tool_use` blocks that produced a given call, tool-result
/// rows carry the `is_error` flag per id. We cross-reference the two to
/// fingerprint only the successful calls.
async fn prior_tool_execution_index(
    deps: &LoopDeps,
    tenant_id: elena_types::TenantId,
    thread_id: elena_types::ThreadId,
) -> Result<PriorToolExecIndex, ElenaError> {
    let msgs = deps.store.threads.list_messages(tenant_id, thread_id, 1_000, None).await?;
    let mut ids: HashSet<ToolCallId> = HashSet::new();
    let mut succeeded_ids: HashSet<ToolCallId> = HashSet::new();
    let mut tool_use_by_id: std::collections::HashMap<ToolCallId, (String, serde_json::Value)> =
        std::collections::HashMap::new();
    for m in &msgs {
        for b in &m.content {
            match b {
                ContentBlock::ToolUse { id, name, input, .. } => {
                    tool_use_by_id.insert(*id, (name.clone(), input.clone()));
                }
                ContentBlock::ToolResult { tool_use_id, is_error, .. } => {
                    ids.insert(*tool_use_id);
                    if !is_error {
                        succeeded_ids.insert(*tool_use_id);
                    }
                }
                _ => {}
            }
        }
    }
    let succeeded_fingerprints: HashSet<String> = succeeded_ids
        .iter()
        .filter_map(|id| tool_use_by_id.get(id).map(|(name, input)| tool_call_fingerprint(name, input)))
        .collect();
    Ok(PriorToolExecIndex { ids, succeeded_fingerprints })
}

/// Stable fingerprint for "same tool + same input". Produces the same
/// string for `{a:1,b:2}` and `{b:2,a:1}` by sorting object keys
/// recursively.
fn tool_call_fingerprint(name: &str, input: &serde_json::Value) -> String {
    format!("{name}:{}", canonicalize(input))
}

fn canonicalize(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let body: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonicalize(v)))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(arr) => {
            let body: Vec<String> = arr.iter().map(canonicalize).collect();
            format!("[{}]", body.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

async fn emit_phase_change(tx: &mpsc::Sender<StreamEvent>, from: &str, to: &str) {
    let _ = tx.send(StreamEvent::PhaseChange { from: from.into(), to: to.into() }).await;
}

/// Compute the tool-schema subset this tenant is allowed to see.
///
/// Rule: for each registered plugin `P`, visible to tenant `T` iff
/// (A) `P` has no ownership rows OR `T` is one of its owners (A5)
/// AND (B) tenant allow-list empty OR `P` in tenant list (A4)
/// AND (C) workspace allow-list empty OR `P` in workspace list (A4).
///
/// Schemas whose tool name begins with a visible `{plugin_id}_` prefix
/// pass. Non-plugin built-ins always pass. Missing DB rows → treat as
/// empty (permissive), so Phase-6 deployments keep working.
async fn filter_schemas_for_tenant(
    deps: &LoopDeps,
    state: &LoopState,
) -> Vec<elena_llm::ToolSchema> {
    let all = deps.tools.schemas();
    let plugins = deps.plugins.manifests();
    if plugins.is_empty() {
        // Nothing to filter — all tools are built-ins.
        return all;
    }
    let registered: Vec<String> = plugins.iter().map(|m| m.id.as_str().to_owned()).collect();

    // Pull allow-lists and ownership-visible set. Best-effort: an error
    // on any of these degrades to "no filter" rather than failing the
    // whole turn.
    let tenant_allowed: Vec<String> = deps
        .store
        .tenants
        .get_tenant(state.tenant.tenant_id)
        .await
        .map(|t| t.allowed_plugin_ids)
        .unwrap_or_default();
    let workspace_allowed: Vec<String> = deps
        .store
        .workspaces
        .get(state.tenant.tenant_id, state.tenant.workspace_id)
        .await
        .ok()
        .flatten()
        .map(|w| w.allowed_plugin_ids)
        .unwrap_or_default();
    let visible_by_ownership: Vec<String> = deps
        .store
        .plugin_ownerships
        .visible_plugins(state.tenant.tenant_id, &registered)
        .await
        .unwrap_or_else(|_| registered.clone());

    // Intersect. `allowed` is the final set of plugin IDs to surface.
    let allowed: std::collections::HashSet<String> = visible_by_ownership
        .into_iter()
        .filter(|p| tenant_allowed.is_empty() || tenant_allowed.contains(p))
        .filter(|p| workspace_allowed.is_empty() || workspace_allowed.contains(p))
        .collect();

    // If every filter is empty the set equals the registered plugins —
    // schema list is unchanged. Short-circuit to save the name check.
    if tenant_allowed.is_empty()
        && workspace_allowed.is_empty()
        && allowed.len() == registered.len()
    {
        return all;
    }

    all.into_iter()
        .filter(|schema| {
            let Some(name) = schema.as_value().get("name").and_then(|v| v.as_str()) else {
                return true;
            };
            // Non-plugin built-ins (names that don't start with any
            // registered plugin prefix) always pass.
            let plugin_prefix = registered
                .iter()
                .find(|pid| name.starts_with(&format!("{pid}_")) || name == pid.as_str());
            match plugin_prefix {
                Some(pid) => allowed.contains(pid),
                None => true,
            }
        })
        .collect()
}

/// Extract the plugin id prefix from a tool name like
/// `"slack_post_message"` given the set of registered plugin ids
/// (`["slack", "notion", ...]`).
///
/// Returns `Some(plugin_id)` when the tool name starts with
/// `{plugin_id}_`, `None` for built-in tools or tools whose plugin is
/// not registered. Used to scope per-tenant credential lookups.
#[must_use]
pub(crate) fn plugin_id_from_tool_name(
    tool_name: &str,
    registered_plugin_ids: &[String],
) -> Option<String> {
    registered_plugin_ids.iter().find_map(|pid| {
        let prefix = format!("{pid}_");
        tool_name.starts_with(&prefix).then(|| pid.clone())
    })
}

#[cfg(test)]
mod approval_tests {
    use elena_tools::ToolInvocation;
    use elena_types::{ApprovalDecision, ApprovalVerdict, ToolCallId};

    use super::*;

    fn inv(name: &str) -> ToolInvocation {
        ToolInvocation {
            id: ToolCallId::new(),
            name: name.to_owned(),
            input: serde_json::json!({"k": "v"}),
        }
    }

    #[test]
    fn summarize_input_prints_sorted_pairs() {
        let s = summarize_input(
            "slack.post_message",
            &serde_json::json!({"channel": "#sales", "text": "hi", "extra": "x"}),
        );
        // Sorted by key — channel, extra, text.
        assert!(s.starts_with("slack.post_message("));
        assert!(s.contains("channel=#sales"));
        assert!(s.contains("text=hi"));
    }

    #[test]
    fn summarize_input_handles_null() {
        let s = summarize_input("notion.list_pages", &serde_json::Value::Null);
        assert_eq!(s, "notion.list_pages()");
    }

    #[test]
    fn brief_value_truncates_long_strings() {
        let long = "x".repeat(200);
        let b = brief_value(&serde_json::Value::String(long));
        assert!(b.ends_with('…'));
        assert!(b.chars().count() <= 50);
    }

    #[test]
    fn split_decisions_allow_all() {
        let i1 = inv("a");
        let i2 = inv("b");
        let decisions = vec![
            ApprovalDecision { tool_use_id: i1.id, decision: ApprovalVerdict::Allow, edits: None },
            ApprovalDecision { tool_use_id: i2.id, decision: ApprovalVerdict::Allow, edits: None },
        ];
        let (allowed, denied) = split_decisions(&[i1.clone(), i2.clone()], &decisions);
        assert_eq!(allowed.len(), 2);
        assert!(denied.is_empty());
    }

    #[test]
    fn split_decisions_applies_edits() {
        let i1 = inv("slack.post_message");
        let decisions = vec![ApprovalDecision {
            tool_use_id: i1.id,
            decision: ApprovalVerdict::Allow,
            edits: Some(serde_json::json!({"channel": "#override"})),
        }];
        let (allowed, _) = split_decisions(std::slice::from_ref(&i1), &decisions);
        assert_eq!(allowed[0].input, serde_json::json!({"channel": "#override"}));
    }

    #[test]
    fn split_decisions_deny_moves_to_denied() {
        let i1 = inv("stripe.payouts.create");
        let decisions = vec![ApprovalDecision {
            tool_use_id: i1.id,
            decision: ApprovalVerdict::Deny,
            edits: None,
        }];
        let (allowed, denied) = split_decisions(std::slice::from_ref(&i1), &decisions);
        assert!(allowed.is_empty());
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].tool_use_id, i1.id);
    }

    #[test]
    fn split_decisions_missing_decision_is_conservatively_denied() {
        let i1 = inv("a");
        let i2 = inv("b");
        // Only one decision for two invocations.
        let decisions = vec![ApprovalDecision {
            tool_use_id: i1.id,
            decision: ApprovalVerdict::Allow,
            edits: None,
        }];
        let (allowed, denied) = split_decisions(&[i1.clone(), i2.clone()], &decisions);
        assert_eq!(allowed.len(), 1);
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].tool_use_id, i2.id);
    }

    #[test]
    fn pending_from_invocations_preserves_ids_and_names() {
        let i1 = inv("slack.post_message");
        let i2 = inv("stripe.payouts.create");
        let pending = pending_from_invocations(&[i1.clone(), i2.clone()]);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].tool_use_id, i1.id);
        assert_eq!(pending[0].tool_name, "slack.post_message");
        assert_eq!(pending[1].tool_use_id, i2.id);
    }

    // ----- Phase 7 · F-track edge cases -----

    #[test]
    fn f1_unicode_arabic_and_spanish_preserved_through_summary() {
        // Solen-style: a Spanish prompt that asks Elena to message an
        // Arabic-named contact. Tool args carry both scripts.
        let input = serde_json::json!({
            "to": "فاطمة",
            "subject": "Resumen diario",
            "body": "Hola, ¿qué tal?",
        });
        let summary = summarize_input("gmail.send_message", &input);
        assert!(summary.contains("فاطمة"), "Arabic 'to' field lost: {summary}");
        assert!(summary.contains("Resumen diario"), "Spanish subject lost: {summary}");
    }

    #[test]
    fn f1_pending_from_invocations_preserves_unicode_bytes() {
        let mut inv = ToolInvocation {
            id: ToolCallId::new(),
            name: "notion.create_page".to_owned(),
            input: serde_json::Value::Null,
        };
        inv.input = serde_json::json!({
            "title": "Plan de entrenamiento — semana 1 💪",
            "tags": ["fuerza", "principiante"],
        });
        let pending = pending_from_invocations(std::slice::from_ref(&inv));
        assert_eq!(pending.len(), 1);
        // The structured input is bit-for-bit preserved.
        assert_eq!(pending[0].input, inv.input);
    }

    #[test]
    fn f8_zero_length_result_synthesises_well_formed_summary() {
        // Plugin returns `{}` — an empty object, not null. Summary
        // should render `tool()` shape with no key=value pairs leaked.
        let summary = summarize_input("notion.list_pages", &serde_json::json!({}));
        assert_eq!(summary, "notion.list_pages()");
    }

    #[test]
    fn f8_completely_null_input_renders_cleanly() {
        let summary = summarize_input("any.tool", &serde_json::Value::Null);
        assert_eq!(summary, "any.tool()");
    }

    #[test]
    fn f8_array_input_renders_as_brief_value() {
        // Tools whose input is a top-level array (rare but legal).
        let summary = summarize_input("foo.bar", &serde_json::json!(["a", "b", "c"]));
        // Renders as `foo.bar(<brief>)`.
        assert!(summary.starts_with("foo.bar("));
        assert!(summary.ends_with(')'));
    }

    #[test]
    fn f10_spanish_keys_round_trip_through_split_decisions() {
        // Mixed-script tool name + Spanish edits payload survive the
        // approval-allow path with edits.
        let i1 = ToolInvocation {
            id: ToolCallId::new(),
            name: "noticias.publicar".to_owned(),
            input: serde_json::json!({"título": "borrador"}),
        };
        let edits = serde_json::json!({"título": "Versión final"});
        let decisions = vec![ApprovalDecision {
            tool_use_id: i1.id,
            decision: ApprovalVerdict::Allow,
            edits: Some(edits.clone()),
        }];
        let (allowed, denied) = split_decisions(std::slice::from_ref(&i1), &decisions);
        assert_eq!(allowed.len(), 1);
        assert!(denied.is_empty());
        // Edits replace the original input verbatim — Unicode preserved.
        assert_eq!(allowed[0].input, edits);
    }

    #[test]
    fn f3_brief_value_handles_multibyte_truncation_without_panic() {
        // Build a string longer than MAX_LEN (48) where the boundary
        // falls inside a multi-byte char — naive byte-slicing would
        // panic. Our `take(MAX_LEN).chars()` impl must not.
        let long = "💪".repeat(100); // 4 bytes per emoji × 100 = 400 bytes
        let brief = brief_value(&serde_json::Value::String(long));
        assert!(brief.ends_with('…'));
        // Should produce a valid UTF-8 string (no panic, no replacement chars).
        assert!(brief.chars().all(|c| c == '💪' || c == '…'));
    }

    // ----- F9: approval deadline arithmetic -----

    #[test]
    fn f9_approval_deadline_lapsed_strictly_at_or_past_deadline() {
        // Equal-time counts as lapsed (the comparison is `>=`).
        assert!(approval_deadline_lapsed(1_000, 1_000));
        // Strictly past lapses.
        assert!(approval_deadline_lapsed(1_001, 1_000));
        // Strictly before does NOT lapse.
        assert!(!approval_deadline_lapsed(999, 1_000));
    }

    #[test]
    fn f9_approval_deadline_holds_under_extreme_inputs() {
        // i64::MAX deadline is "never expires" for any realistic clock.
        assert!(!approval_deadline_lapsed(0, i64::MAX));
        assert!(!approval_deadline_lapsed(1_700_000_000_000, i64::MAX));
        // i64::MIN deadline is "always expired".
        assert!(approval_deadline_lapsed(0, i64::MIN));
        assert!(approval_deadline_lapsed(i64::MIN + 1, i64::MIN));
    }

    #[test]
    fn f9_approval_deadline_default_is_300_seconds() {
        // Pin the contract that the default deadline matches the Phase 7
        // plan and the smoke's documented expectation.
        assert_eq!(APPROVAL_DEADLINE_SECS, 300);
    }

    #[test]
    fn f8_deeply_nested_object_summary_does_not_blow_the_stack() {
        // Five-level nesting (Phase 3 schema-extreme test). Build a
        // payload mimicking what a complex plugin schema would produce.
        let input = serde_json::json!({
            "outer": {
                "level2": {
                    "level3": {
                        "level4": {
                            "level5": "deeply nested",
                        }
                    }
                }
            }
        });
        let summary = summarize_input("plugin.deep_action", &input);
        // The outer-most 3 keys are rendered (we sort + take 3); since
        // there's only one, only `outer` shows. The nested object is
        // brief-valued inline.
        assert!(summary.starts_with("plugin.deep_action("));
        assert!(summary.contains("outer="));
    }

    #[test]
    fn fingerprint_normalizes_object_key_order() {
        let a = tool_call_fingerprint(
            "slack_post_message",
            &serde_json::json!({"channel": "#x", "text": "hi"}),
        );
        let b = tool_call_fingerprint(
            "slack_post_message",
            &serde_json::json!({"text": "hi", "channel": "#x"}),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_distinguishes_different_inputs() {
        let a = tool_call_fingerprint(
            "slack_post_message",
            &serde_json::json!({"channel": "#x", "text": "hi"}),
        );
        let b = tool_call_fingerprint(
            "slack_post_message",
            &serde_json::json!({"channel": "#x", "text": "bye"}),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_distinguishes_different_tool_names() {
        let a = tool_call_fingerprint("slack_post_message", &serde_json::json!({}));
        let b = tool_call_fingerprint("slack_list_channels", &serde_json::json!({}));
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_handles_nested_objects_and_arrays() {
        let a = tool_call_fingerprint(
            "x",
            &serde_json::json!({"a": [1, 2, {"k": "v", "j": 3}], "z": true}),
        );
        let b = tool_call_fingerprint(
            "x",
            &serde_json::json!({"z": true, "a": [1, 2, {"j": 3, "k": "v"}]}),
        );
        assert_eq!(a, b);
    }
}
