//! Wire types shared by [`elena-gateway`] and [`elena-worker`] for the
//! NATS-bridged distributed runtime.
//!
//! Lives in `elena-types` so neither side needs to depend on the other —
//! the gateway publishes [`WorkRequest`] to [`subjects::WORK_INCOMING`],
//! the worker subscribes (with a queue group), and events flow back over
//! the per-thread subject built by [`subjects::thread_events`].

use serde::{Deserialize, Serialize};

use crate::{
    autonomy::AutonomyMode,
    id::{RequestId, ThreadId},
    message::{ContentBlock, Message},
    model::ModelId,
    tenant::TenantContext,
};

/// One agentic-loop turn's worth of work.
///
/// Built by the gateway when a WebSocket client emits `send_message`,
/// published to [`subjects::WORK_INCOMING`] via `JetStream` so it survives a
/// worker crash. Workers ack only after the loop finishes and all
/// [`StreamEvent`](crate::stream::StreamEvent)s have been published.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkRequest {
    /// Stable per-request id used as the NATS dedup key on redelivery.
    pub request_id: RequestId,
    /// Tenant the request runs as (extracted from the JWT at the gateway).
    pub tenant: TenantContext,
    /// Thread to append to / continue.
    pub thread_id: ThreadId,
    /// The user message that kicks off this turn. Worker persists it
    /// idempotently before invoking
    /// [`run_loop`](https://docs.rs/elena-core/*/elena_core/fn.run_loop.html).
    pub message: Message,
    /// Optional model override. `None` → router picks per
    /// [`elena-router::ModelRouter::route`].
    #[serde(default)]
    pub model: Option<ModelId>,
    /// Optional system prompt to pin for this turn. Empty = no system prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_prompt: Vec<ContentBlock>,
    /// Per-turn output cap. `None` → use the worker default.
    #[serde(default)]
    pub max_tokens_per_turn: Option<u32>,
    /// Loop-level max-turn cap. `None` → use the worker default.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// W3C `traceparent` header, if the gateway had an active span when it
    /// published this request. Worker extracts it into its root span.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_parent: Option<String>,
    /// W3C `tracestate` header. Opaque; round-tripped as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_state: Option<String>,
    /// Per-thread autonomy policy pinned for this turn. Gateway loads
    /// it from the thread's stored mode; worker copies it into
    /// [`LoopState::autonomy`]. Older serialized messages missing this
    /// field default to [`AutonomyMode::Moderate`].
    #[serde(default)]
    pub autonomy: AutonomyMode,
    /// Whether this is a fresh user turn or a resume after a client
    /// posted approvals for a paused loop. Resume requests skip the
    /// idempotent user-message append.
    #[serde(default)]
    pub kind: WorkRequestKind,
}

/// Distinguishes a fresh user turn from a post-approval resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkRequestKind {
    /// The client submitted a new user message — append, claim, drive
    /// from `Received`.
    #[default]
    Turn,
    /// The loop was previously parked in `AwaitingApproval`; the client
    /// has now posted approvals via the gateway. The worker should claim,
    /// skip the user-message append, and let the checkpoint drive the
    /// resume.
    ResumeFromApproval,
}

/// Well-known NATS subject names. Centralised so both sides agree on the
/// exact strings.
pub mod subjects {
    use crate::id::ThreadId;

    /// `JetStream` subject the gateway publishes [`super::WorkRequest`]s to.
    /// Workers `queue_subscribe` on this subject under the `"workers"`
    /// queue group for at-least-once load distribution.
    pub const WORK_INCOMING: &str = "elena.work.incoming";

    /// Default `JetStream` stream name backing [`WORK_INCOMING`].
    pub const WORK_STREAM: &str = "elena-work";

    /// Default queue group for worker consumers.
    pub const WORKERS_QUEUE: &str = "workers";

    /// Per-thread subject the worker publishes
    /// [`StreamEvent`](crate::stream::StreamEvent)s to. The gateway's
    /// WebSocket handler subscribes for the lifetime of the connection.
    #[must_use]
    pub fn thread_events(thread_id: ThreadId) -> String {
        format!("elena.thread.{thread_id}.events")
    }

    /// Per-thread subject the gateway publishes an empty payload to in
    /// order to abort a turn the worker is currently running. Workers
    /// subscribe in parallel with the `JetStream` consumer; receiving any
    /// message on this subject fires the loop's
    /// [`CancellationToken`](tokio_util::sync::CancellationToken).
    #[must_use]
    pub fn thread_abort(thread_id: ThreadId) -> String {
        format!("elena.thread.{thread_id}.abort")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::Utc;

    use super::*;
    use crate::{
        id::{MessageId, SessionId, TenantId, UserId, WorkspaceId},
        message::{Message, MessageKind, Role},
        permission::PermissionSet,
        tenant::{BudgetLimits, TenantTier},
    };

    fn tenant() -> TenantContext {
        TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::DEFAULT_PRO,
            tier: TenantTier::Pro,
            plan: None,
            metadata: HashMap::new(),
        }
    }

    fn req() -> WorkRequest {
        let thread_id = ThreadId::new();
        WorkRequest {
            request_id: RequestId::new(),
            tenant: tenant(),
            thread_id,
            message: Message {
                id: MessageId::new(),
                thread_id,
                tenant_id: TenantId::new(),
                role: Role::User,
                kind: MessageKind::User,
                content: vec![ContentBlock::Text { text: "hi".into(), cache_control: None }],
                created_at: Utc::now(),
                token_count: None,
                parent_id: None,
            },
            model: Some(ModelId::new("claude-haiku-4-5")),
            system_prompt: vec![ContentBlock::Text {
                text: "You are concise.".into(),
                cache_control: None,
            }],
            max_tokens_per_turn: Some(256),
            max_turns: Some(5),
            trace_parent: None,
            trace_state: None,
            autonomy: AutonomyMode::default(),
            kind: WorkRequestKind::default(),
        }
    }

    #[test]
    fn work_request_roundtrip() {
        let r = req();
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn optional_fields_omitted_when_unset() {
        let mut r = req();
        r.model = None;
        r.max_tokens_per_turn = None;
        r.max_turns = None;
        r.system_prompt.clear();
        let json = serde_json::to_value(&r).unwrap();
        // Empty system_prompt is skipped; max_* are None and serialize as null
        // unless skipped — we don't `skip_serializing_if` them, so they appear.
        assert!(json.get("system_prompt").is_none());
        assert!(json.get("model").is_some()); // null
    }

    #[test]
    fn work_incoming_subject_is_stable() {
        assert_eq!(subjects::WORK_INCOMING, "elena.work.incoming");
        assert_eq!(subjects::WORK_STREAM, "elena-work");
        assert_eq!(subjects::WORKERS_QUEUE, "workers");
    }

    #[test]
    fn thread_event_subject_format() {
        let id = ThreadId::new();
        let s = subjects::thread_events(id);
        assert!(s.starts_with("elena.thread."));
        assert!(s.ends_with(".events"));
        assert!(s.contains(&id.to_string()));
    }

    #[test]
    fn thread_abort_subject_format() {
        let id = ThreadId::new();
        let s = subjects::thread_abort(id);
        assert!(s.starts_with("elena.thread."));
        assert!(s.split('.').next_back() == Some("abort"));
    }
}
