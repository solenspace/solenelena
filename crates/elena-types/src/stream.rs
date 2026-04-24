//! Stream events — the wire protocol between worker, gateway, and client.
//!
//! Every event is small and self-contained. Large payloads (tool results,
//! assistant messages) are committed to Postgres first; the stream carries
//! only pointers so a reconnecting client can replay from the DB. This keeps
//! the NATS/WebSocket hot path cheap and makes crash recovery a matter of
//! resuming from the last committed row.

use serde::{Deserialize, Serialize};

use crate::{
    autonomy::PendingApproval,
    error::ElenaError,
    id::{MessageId, ToolCallId},
    terminal::Terminal,
    usage::Usage,
};

/// One event in the output stream.
///
/// Variants are tagged via an `event` field so consumers can match on the
/// type and route to the right renderer. Gateways multiplex these to
/// WebSocket or SSE clients with no transformation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A new assistant message is about to stream.
    MessageStart {
        /// ID of the in-progress message.
        message_id: MessageId,
    },
    /// A piece of assistant text. Concatenate deltas in order to reconstruct.
    TextDelta {
        /// Token chunk.
        delta: String,
    },
    /// A piece of visible model reasoning.
    ThinkingDelta {
        /// Reasoning chunk.
        delta: String,
    },
    /// The model started emitting a tool-use block.
    ToolUseStart {
        /// Tool-call ID.
        id: ToolCallId,
        /// Tool name.
        name: String,
    },
    /// A partial JSON input for the in-progress tool-use block.
    ToolUseInputDelta {
        /// Tool-call ID.
        id: ToolCallId,
        /// Fragment of the input JSON (may not parse on its own).
        partial_json: String,
    },
    /// The tool-use block finished — parsed input is attached.
    ToolUseComplete {
        /// Tool-call ID.
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Parsed input value.
        input: serde_json::Value,
    },
    /// Tool execution has begun on the worker side.
    ToolExecStart {
        /// Tool-call ID being executed.
        id: ToolCallId,
    },
    /// The tool emitted a progress update while running.
    ToolProgress {
        /// Tool-call ID.
        id: ToolCallId,
        /// Human-readable progress message.
        message: String,
        /// Structured data (tool-specific shape).
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        data: serde_json::Value,
    },
    /// Tool execution finished. Full body is committed to Postgres; this
    /// event is a pointer.
    ToolResult {
        /// Tool-call ID.
        id: ToolCallId,
        /// True if the tool call errored.
        is_error: bool,
    },
    /// Usage counters updated (cumulative for this turn).
    UsageUpdate(Usage),
    /// The loop transitioned between phases (diagnostic/UX only).
    PhaseChange {
        /// Previous phase name.
        from: String,
        /// New phase name.
        to: String,
    },
    /// An error occurred. May be terminal or intermediate (loop continues).
    Error(ElenaError),
    /// The loop has paused before executing tool calls and is waiting for
    /// the client to post approvals. The stream does not emit further
    /// events on this channel until the client responds via the gateway's
    /// approvals endpoint — at which point a fresh `WorkRequest` resumes
    /// the thread, streaming on a new subject.
    AwaitingApproval {
        /// Tool calls the loop wants to run. Each carries enough context
        /// for a client UI to render an approve/deny prompt.
        pending: Vec<PendingApproval>,
        /// Unix epoch millis after which the pause will time out and the
        /// loop will terminate with a `Terminal::BlockingLimit`. Clients
        /// should surface this so users know how long they have.
        deadline_ms: i64,
    },
    /// The turn has completed — no further events follow.
    Done(Terminal),
}

/// X4 — Wire-level wrapper that stamps a per-thread monotonic offset
/// onto every event the worker publishes.
///
/// The offset is generated via Redis INCR per `(thread_id, event)` so
/// it survives worker crashes and gateway restarts. Clients track the
/// last `offset` they saw; on reconnect they pass `?since=<offset>` to
/// the WS upgrade and the gateway prepends a Postgres-replay of any
/// persisted messages newer than the last seen offset before joining
/// the live stream.
///
/// The flat-tagged shape (`{"offset": 42, "event": {...}}`) keeps the
/// wire compatible with consumers that just want the inner `event` —
/// they peel one layer and recover the pre-X4 `StreamEvent` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEnvelope {
    /// Per-thread monotonic counter. Strictly increasing across the
    /// thread's lifetime. `0` is reserved for replay events
    /// reconstructed from persisted messages (which never had a live
    /// offset assigned).
    pub offset: u64,
    /// The wrapped event.
    pub event: StreamEvent,
}

impl StreamEnvelope {
    /// Wrap an event with an explicit offset.
    #[must_use]
    pub const fn new(offset: u64, event: StreamEvent) -> Self {
        Self { offset, event }
    }

    /// Sentinel offset for events synthesized during replay (i.e.
    /// derived from persisted messages, not from a live stream).
    pub const REPLAY_OFFSET: u64 = 0;

    /// Build a replay envelope (offset = `REPLAY_OFFSET`).
    #[must_use]
    pub const fn replay(event: StreamEvent) -> Self {
        Self { offset: Self::REPLAY_OFFSET, event }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_delta_wire_shape() {
        let ev = StreamEvent::TextDelta { delta: "Hello".into() };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json, serde_json::json!({"event": "text_delta", "delta": "Hello"}));
    }

    #[test]
    fn done_terminates_the_stream() {
        let ev = StreamEvent::Done(Terminal::Completed);
        let json = serde_json::to_value(&ev).unwrap();
        // `event: "done"` then the flattened Terminal tag, but since Terminal
        // uses `tag = "reason"`, the two serde attributes conflict — match
        // exactly.
        assert_eq!(json, serde_json::json!({"event": "done", "reason": "completed"}));
    }

    #[test]
    fn usage_update_roundtrip() {
        let ev = StreamEvent::UsageUpdate(Usage { input_tokens: 42, ..Default::default() });
        let json = serde_json::to_string(&ev).unwrap();
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn tool_use_complete_carries_parsed_input() {
        let ev = StreamEvent::ToolUseComplete {
            id: ToolCallId::new(),
            name: "search".into(),
            input: serde_json::json!({"q": "rust"}),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn error_event_carries_full_error() {
        let ev = StreamEvent::Error(ElenaError::PermissionDenied { reason: "no".into() });
        let json = serde_json::to_string(&ev).unwrap();
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn awaiting_approval_wire_shape() {
        let tool_use_id = ToolCallId::new();
        let ev = StreamEvent::AwaitingApproval {
            pending: vec![crate::autonomy::PendingApproval {
                tool_use_id,
                tool_name: "slack.post_message".into(),
                summary: "Post to #sales".into(),
                input: serde_json::json!({"channel": "#sales"}),
            }],
            deadline_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["event"], "awaiting_approval");
        assert_eq!(json["deadline_ms"], 1_700_000_000_000_i64);
        let back: StreamEvent = serde_json::from_value(json).unwrap();
        assert_eq!(ev, back);
    }
}
