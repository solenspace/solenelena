//! Internal enum of Anthropic SSE events.
//!
//! This is the *raw wire shape* we deserialize from a single SSE frame's
//! `data:` payload. The `StreamAssembler` consumes these and produces
//! [`elena_types::StreamEvent`] — Elena's outward-facing wire protocol.
//!
//! Kept internal (`pub(crate)`) because its shape is an Anthropic
//! implementation detail; external consumers use `StreamEvent`.
//!
//! These types are consumed by the assembler (Step 4) and the client (Step
//! 7); they're intentionally wire-named (variants end in `Delta`) to match
//! the SSE protocol field 1:1.

// Suppress lints that don't apply: variants intentionally mirror wire names
// (all `*Delta`), and types are used by later-step modules that aren't wired
// in yet.
#![allow(dead_code, clippy::enum_variant_names)]

use elena_types::{CacheControl, ToolCallId, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One SSE event payload from the Anthropic Messages streaming endpoint.
///
/// Tagged via the `type` field on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnthropicEvent {
    /// Initial frame — metadata about the in-progress message.
    MessageStart {
        /// Partial message envelope (id, role, usage, model).
        message: MessageStartPayload,
    },
    /// A new content block begins at `index`.
    ContentBlockStart {
        /// Block index (stable across the stream).
        index: u32,
        /// Initial block shape — empty text/thinking/input string.
        content_block: BlockStart,
    },
    /// A delta to the block at `index`.
    ContentBlockDelta {
        /// Block index to append to.
        index: u32,
        /// The delta payload.
        delta: BlockDelta,
    },
    /// The block at `index` is finalized.
    ContentBlockStop {
        /// Block index that just finalized.
        index: u32,
    },
    /// Partial message update — stop reason + final usage totals.
    MessageDelta {
        /// Delta payload (stop reason etc.).
        delta: MessageDeltaPayload,
        /// Updated cumulative usage.
        #[serde(default)]
        usage: Option<Usage>,
    },
    /// Terminal marker — no more events will follow.
    MessageStop,
    /// Keep-alive.
    Ping,
    /// Server-side error event (rare; the stream typically just closes).
    Error {
        /// The error payload — opaque JSON.
        error: Value,
    },
}

/// Initial `message_start` payload subset we care about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MessageStartPayload {
    /// Message id (e.g., `"msg_01abc..."`).
    pub id: String,
    /// Role (always `"assistant"` from Anthropic).
    #[serde(default)]
    pub role: Option<String>,
    /// Model that actually answered.
    #[serde(default)]
    pub model: Option<String>,
    /// Cumulative usage at start.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// Start-of-block payload. Covers all block variants Anthropic can emit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BlockStart {
    /// An empty-string text block about to stream.
    Text {
        /// Always `""` at start.
        #[serde(default)]
        text: String,
        /// Cache-control marker carried from the request echo, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// A thinking block about to stream.
    Thinking {
        /// Always `""` at start.
        #[serde(default)]
        thinking: String,
        /// Usually `""` at start; finalized at `content_block_stop`.
        #[serde(default)]
        signature: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Redacted thinking block (opaque).
    RedactedThinking {
        /// Opaque data blob.
        data: String,
    },
    /// A tool-use block about to stream.
    ToolUse {
        /// Tool call id (`toolu_...`).
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Initial input — server sends `{}` or `""`, client may receive either.
        #[serde(default)]
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

/// Per-block delta payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum BlockDelta {
    /// Append to a text block.
    TextDelta {
        /// Text chunk.
        text: String,
    },
    /// Append to a thinking block.
    ThinkingDelta {
        /// Thinking chunk.
        thinking: String,
    },
    /// Finalized signature for a thinking block (emitted once, near end).
    SignatureDelta {
        /// The final signature value.
        signature: String,
    },
    /// Append raw JSON to a tool-use block's input.
    ///
    /// Accumulated as a plain string until `content_block_stop`, then parsed.
    InputJsonDelta {
        /// Raw JSON fragment — may not parse on its own.
        partial_json: String,
    },
}

/// `message_delta` payload subset.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct MessageDeltaPayload {
    /// Why the model stopped, if known.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Stop sequence that matched, if any.
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_is_parseable() {
        let json = r#"{"type":"ping"}"#;
        let ev: AnthropicEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev, AnthropicEvent::Ping);
    }

    #[test]
    fn message_start_roundtrip() {
        let json = serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_01abc",
                "role": "assistant",
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 42,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0},
                    "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                    "service_tier": "standard",
                    "inference_geo": "",
                    "iterations": [],
                    "speed": "standard"
                }
            }
        });
        let ev: AnthropicEvent = serde_json::from_value(json).unwrap();
        match ev {
            AnthropicEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_01abc");
                let usage = message.usage.unwrap();
                assert_eq!(usage.input_tokens, 42);
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
    }

    #[test]
    fn content_block_delta_text() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        let ev: AnthropicEvent = serde_json::from_str(json).unwrap();
        match ev {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta, BlockDelta::TextDelta { text: "Hello".into() });
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn content_block_delta_input_json() {
        let json = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city"}}"#;
        let ev: AnthropicEvent = serde_json::from_str(json).unwrap();
        match ev {
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 1);
                assert_eq!(delta, BlockDelta::InputJsonDelta { partial_json: "{\"city".into() });
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn content_block_start_tool_use() {
        let tool_id = ToolCallId::new();
        let json = serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "tool_use",
                "id": tool_id.to_string(),
                "name": "weather",
                "input": {}
            }
        });
        let ev: AnthropicEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(
            ev,
            AnthropicEvent::ContentBlockStart {
                index: 1,
                content_block: BlockStart::ToolUse { .. }
            }
        ));
    }

    #[test]
    fn thinking_signature_delta() {
        let json = r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-xyz"}}"#;
        let ev: AnthropicEvent = serde_json::from_str(json).unwrap();
        match ev {
            AnthropicEvent::ContentBlockDelta {
                delta: BlockDelta::SignatureDelta { signature },
                ..
            } => {
                assert_eq!(signature, "sig-xyz");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn message_delta_carries_stop_reason() {
        let json = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#;
        let ev: AnthropicEvent = serde_json::from_str(json).unwrap();
        match ev {
            AnthropicEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason.as_deref(), Some("end_turn"));
                assert!(usage.is_none());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn message_stop_and_content_block_stop() {
        let stop: AnthropicEvent = serde_json::from_str(r#"{"type":"message_stop"}"#).unwrap();
        assert_eq!(stop, AnthropicEvent::MessageStop);

        let block_stop: AnthropicEvent =
            serde_json::from_str(r#"{"type":"content_block_stop","index":3}"#).unwrap();
        match block_stop {
            AnthropicEvent::ContentBlockStop { index } => assert_eq!(index, 3),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn error_event_carries_opaque_payload() {
        let json = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        let ev: AnthropicEvent = serde_json::from_str(json).unwrap();
        match ev {
            AnthropicEvent::Error { error } => {
                assert_eq!(error["type"], "overloaded_error");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unknown_event_type_fails_parse() {
        // Anthropic may add new event types; we surface the parse error to the
        // caller (retry/fallback logic decides what to do).
        let json = r#"{"type":"some_future_event"}"#;
        let result: Result<AnthropicEvent, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}
