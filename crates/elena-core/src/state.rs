//! [`LoopState`] — the serializable agentic-loop state machine.
//!
//! A `LoopState` is everything a worker needs to resume a thread from a
//! crash. It roundtrips through JSON so it can sleep in Redis between
//! turns. Committed messages live in Postgres (authoritative record);
//! `LoopState` is a fast-path hint that lets the loop skip re-planning the
//! current phase when a worker restarts.

use elena_tools::ToolInvocation;
use elena_types::{
    AutonomyMode, ContentBlock, ModelId, ModelTier, PendingApproval, TenantContext, Terminal,
    ThreadId, Usage,
};
use serde::{Deserialize, Serialize};

/// The full agentic-loop state.
///
/// Cloneable so phase transitions can work against an owned snapshot
/// without locking. Serializable so the state can be checkpointed to Redis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopState {
    /// The thread this loop belongs to.
    pub thread_id: ThreadId,
    /// The initiating tenant's context.
    pub tenant: TenantContext,
    /// The model this loop is pinned to for the current turn.
    ///
    /// Phase 4 populates this from
    /// [`ModelRouter::resolve`](elena_router::ModelRouter::resolve) at the
    /// start of each `Streaming` phase based on the current
    /// [`model_tier`](Self::model_tier); cascade escalation updates both.
    pub model: ModelId,

    /// Which LLM provider serves the current turn.
    ///
    /// Phase 6.5 addition: router picks `(provider, model)` per tier, so
    /// state carries both. Defaults to `"anthropic"` for pre-Phase-6.5
    /// checkpoints that only have `model`.
    #[serde(default = "default_provider")]
    pub provider: String,

    /// The currently selected tier. Defaults to
    /// [`ModelTier::Fast`](elena_types::ModelTier::Fast) on fresh state;
    /// the router can override, and a cascade check can escalate.
    #[serde(default = "default_model_tier")]
    pub model_tier: ModelTier,
    /// System prompt for this loop. Empty = no system prompt.
    #[serde(default)]
    pub system_prompt: Vec<ContentBlock>,
    /// Max output tokens per LLM call.
    pub max_tokens_per_turn: u32,
    /// The loop phase.
    pub phase: LoopPhase,
    /// How many user→assistant turns have completed.
    pub turn_count: u32,
    /// Terminating condition's max-turn cap.
    pub max_turns: u32,
    /// Cumulative usage for this loop run.
    pub usage: Usage,
    /// Recovery counters (error retries, escalations, etc.).
    pub recovery: RecoveryState,
    /// Tool calls extracted from the last assistant turn, waiting to run.
    #[serde(default)]
    pub pending_tool_calls: Vec<ToolInvocation>,
    /// True after a turn that emitted tool calls — signals
    /// [`LoopPhase::PostProcessing`] to loop back to
    /// [`LoopPhase::Streaming`] so the model can react to tool results.
    #[serde(default)]
    pub last_turn_had_tools: bool,
    /// Per-thread autonomy policy. Governs whether the loop pauses between
    /// the model proposing tool calls and the worker executing them.
    ///
    /// Pre-Phase-7 checkpoints won't carry this field; `#[serde(default)]`
    /// gives them [`AutonomyMode::Moderate`] — the safe middle ground.
    #[serde(default)]
    pub autonomy: AutonomyMode,
}

const fn default_model_tier() -> ModelTier {
    ModelTier::Fast
}

fn default_provider() -> String {
    "anthropic".to_owned()
}

/// The current step in the loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum LoopPhase {
    /// A fresh request arrived — claim the thread and move to Streaming.
    Received,
    /// An LLM streaming call is in flight (or about to start).
    Streaming,
    /// Tool calls are being executed.
    ExecutingTools {
        /// Invocations that still need to run.
        remaining: Vec<ToolInvocation>,
    },
    /// The model has proposed tool calls and the loop is waiting for the
    /// client to approve/deny them before executing. Reached when
    /// [`AutonomyMode`] says "pause" for the current call set.
    ///
    /// While in this phase the worker releases the thread claim. A fresh
    /// `WorkRequest` republished by the gateway after the client posts
    /// approvals re-enters the loop; the resuming worker reads the
    /// persisted approvals, filters `invocations`, and transitions to
    /// [`Self::ExecutingTools`].
    AwaitingApproval {
        /// Tool calls the model requested. One-to-one with the pending
        /// approvals surfaced to the client via
        /// [`elena_types::StreamEvent::AwaitingApproval`].
        invocations: Vec<ToolInvocation>,
        /// Display copies for the client. Re-derived by the worker on
        /// resume from the plugin manifests if missing, but carrying
        /// them here keeps the re-emit path trivial.
        pending: Vec<PendingApproval>,
        /// Unix epoch millis after which the pause times out.
        deadline_ms: i64,
    },
    /// The turn completed — bookkeeping before looping or terminating.
    PostProcessing,
    /// The loop finished cleanly.
    Completed,
    /// The loop ended unsuccessfully.
    Failed {
        /// Why it ended.
        reason: Terminal,
    },
}

impl LoopPhase {
    /// Short stable tag for metrics/logs.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Streaming => "streaming",
            Self::ExecutingTools { .. } => "executing_tools",
            Self::AwaitingApproval { .. } => "awaiting_approval",
            Self::PostProcessing => "post_processing",
            Self::Completed => "completed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Recovery counters. Used by the loop driver to decide when to give up
/// retrying vs escalate to a terminal failure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryState {
    /// Consecutive errors since the last successful turn.
    pub consecutive_errors: u32,
    /// How many times we've reshaped the context to dodge a reshape error.
    pub context_rebuilds: u32,
    /// How many times we've retried after max-output-tokens.
    pub max_output_retries: u32,
    /// Phase 4: how many cascade-driven tier escalations this turn.
    #[serde(default)]
    pub model_escalations: u32,
}

impl LoopState {
    /// Build a fresh `LoopState` at the start of a request.
    #[must_use]
    pub fn new(
        thread_id: ThreadId,
        tenant: TenantContext,
        model: ModelId,
        max_turns: u32,
        max_tokens_per_turn: u32,
    ) -> Self {
        Self {
            thread_id,
            tenant,
            model,
            provider: "anthropic".to_owned(),
            model_tier: ModelTier::Fast,
            system_prompt: Vec::new(),
            max_tokens_per_turn,
            phase: LoopPhase::Received,
            turn_count: 0,
            max_turns,
            usage: Usage::default(),
            recovery: RecoveryState::default(),
            pending_tool_calls: Vec::new(),
            last_turn_had_tools: false,
            autonomy: AutonomyMode::default(),
        }
    }

    /// Override the autonomy mode for this loop. Typically set once at
    /// thread-creation time from the `threads.autonomy_mode` column.
    #[must_use]
    pub const fn with_autonomy(mut self, autonomy: AutonomyMode) -> Self {
        self.autonomy = autonomy;
        self
    }
}

#[cfg(test)]
mod tests {
    use elena_types::{
        BudgetLimits, LlmApiErrorKind, PermissionSet, SessionId, TenantId, TenantTier, UserId,
        WorkspaceId,
    };

    use super::*;

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
            metadata: std::collections::HashMap::new(),
        }
    }

    fn base_state() -> LoopState {
        LoopState::new(
            ThreadId::new(),
            tenant(),
            ModelId::new("claude-haiku-4-5-20251001"),
            20,
            2_048,
        )
    }

    #[test]
    fn new_state_starts_at_received() {
        let s = base_state();
        assert_eq!(s.phase, LoopPhase::Received);
        assert_eq!(s.turn_count, 0);
        assert_eq!(s.max_turns, 20);
        assert!(s.pending_tool_calls.is_empty());
    }

    #[test]
    fn every_phase_roundtrips() {
        let phases = [
            LoopPhase::Received,
            LoopPhase::Streaming,
            LoopPhase::ExecutingTools { remaining: vec![] },
            LoopPhase::AwaitingApproval {
                invocations: vec![],
                pending: vec![],
                deadline_ms: 1_700_000_000_000,
            },
            LoopPhase::PostProcessing,
            LoopPhase::Completed,
            LoopPhase::Failed { reason: Terminal::Completed },
            LoopPhase::Failed {
                reason: Terminal::ModelError { classified: LlmApiErrorKind::RateLimit },
            },
        ];
        for p in phases {
            let json = serde_json::to_string(&p).unwrap();
            let back: LoopPhase = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn pre_phase7_checkpoint_defaults_autonomy_to_moderate() {
        // Serialize a fresh state, strip the `autonomy` key to simulate a
        // pre-Phase-7 checkpoint, and confirm the field defaults to
        // Moderate on deserialize. This is the serde-default contract we
        // depend on for the rolling Phase-7 deploy.
        let fresh = base_state();
        let mut as_value = serde_json::to_value(&fresh).unwrap();
        as_value.as_object_mut().unwrap().remove("autonomy");
        let loaded: LoopState = serde_json::from_value(as_value).unwrap();
        assert_eq!(loaded.autonomy, elena_types::AutonomyMode::Moderate);
    }

    #[test]
    fn autonomy_field_roundtrips() {
        let s = base_state().with_autonomy(elena_types::AutonomyMode::Cautious);
        let json = serde_json::to_string(&s).unwrap();
        let back: LoopState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.autonomy, elena_types::AutonomyMode::Cautious);
    }

    #[test]
    fn loop_state_roundtrips() {
        let state = base_state();
        let json = serde_json::to_string(&state).unwrap();
        let back: LoopState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.thread_id, state.thread_id);
        assert_eq!(back.phase, state.phase);
        assert_eq!(back.turn_count, state.turn_count);
        assert_eq!(back.recovery, state.recovery);
    }

    #[test]
    fn phase_tags_are_snake_case() {
        assert_eq!(LoopPhase::Received.tag(), "received");
        assert_eq!(LoopPhase::ExecutingTools { remaining: vec![] }.tag(), "executing_tools");
        assert_eq!(LoopPhase::Failed { reason: Terminal::Completed }.tag(), "failed");
    }
}
