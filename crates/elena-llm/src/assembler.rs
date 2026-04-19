//! Stream assembler — consumes raw Anthropic SSE events and produces
//! Elena's outward-facing [`StreamEvent`]s plus the final assembled
//! [`Message`].
//!
//! The assembler is a pure state machine; it does no I/O. Callers feed it
//! one [`AnthropicEvent`] at a time via [`StreamAssembler::handle`] and
//! collect emitted stream events. On `message_stop`, [`StreamAssembler::finish`]
//! returns the final [`Message`], accumulated [`Usage`], and any stop reason.
//!
//! Per the reference TS source (`services/api/claude.ts`):
//! - Content blocks accumulate per `index`; deltas never reset across blocks.
//! - Tool-use input is concatenated as raw JSON; parsed only after the
//!   block's `content_block_stop` event.
//! - Usage is cumulative (not delta). Input-related fields use a `>0` guard:
//!   an explicit `0` in `message_delta` is "unset", not "zero tokens."

// Methods and some block-accumulator variants are consumed in Step 7 (the
// Anthropic client module) and exercised in tests now. Dead-code warnings
// are noise until those modules land.
#![allow(dead_code)]

use std::collections::HashMap;

use elena_types::{
    CacheControl, ContentBlock, ImageSource, Message, MessageId, MessageKind, Role, StopReason,
    StreamEvent, TenantId, ThreadId, ToolCallId, ToolResultContent, Usage,
};
use smallvec::{SmallVec, smallvec};

use crate::events::{AnthropicEvent, BlockDelta, BlockStart, MessageDeltaPayload};

/// Result of finishing a stream — the fully-assembled message plus
/// cumulative usage and stop reason (if any).
#[derive(Debug, Clone)]
pub struct FinishedStream {
    /// The assembled assistant message. Content blocks are ordered by
    /// block index.
    pub message: Message,
    /// Cumulative usage from `message_start` + `message_delta` events,
    /// with the `>0` guard applied to input fields.
    pub usage: Usage,
    /// Why the model stopped, if reported.
    pub stop_reason: Option<StopReason>,
}

/// Stream-assembly state machine.
///
/// Construct once per request with `new(tenant_id, thread_id)`; the
/// `message_id` is learned from `message_start`. Call [`Self::handle`] on
/// each [`AnthropicEvent`] and collect the returned [`StreamEvent`]s until
/// `message_stop` arrives, then call [`Self::finish`].
#[derive(Debug)]
pub struct StreamAssembler {
    tenant_id: TenantId,
    thread_id: ThreadId,
    message_id: Option<MessageId>,
    blocks: HashMap<u32, BlockAccumulator>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    stopped: bool,
}

/// Per-block working state.
#[derive(Debug, Clone)]
enum BlockAccumulator {
    Text {
        text: String,
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        signature: String,
        cache_control: Option<CacheControl>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        /// Raw JSON input, accumulated across `input_json_delta` events.
        /// Parsed only when the block closes.
        input_raw: String,
        cache_control: Option<CacheControl>,
    },
    Image {
        media_type: String,
        source: ImageSource,
    },
}

impl StreamAssembler {
    /// Build a fresh assembler. `message_id` is filled by the first
    /// `message_start` event.
    #[must_use]
    pub fn new(tenant_id: TenantId, thread_id: ThreadId) -> Self {
        Self {
            tenant_id,
            thread_id,
            message_id: None,
            blocks: HashMap::new(),
            usage: Usage::default(),
            stop_reason: None,
            stopped: false,
        }
    }

    /// True once `message_stop` has been observed.
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.stopped
    }

    /// Handle one Anthropic event, returning zero or more Elena stream
    /// events to emit downstream.
    pub(crate) fn handle(&mut self, event: AnthropicEvent) -> SmallVec<[StreamEvent; 2]> {
        match event {
            AnthropicEvent::MessageStart { message } => self.on_message_start(message),
            AnthropicEvent::ContentBlockStart { index, content_block } => {
                self.on_block_start(index, content_block)
            }
            AnthropicEvent::ContentBlockDelta { index, delta } => self.on_block_delta(index, delta),
            AnthropicEvent::ContentBlockStop { index } => self.on_block_stop(index),
            AnthropicEvent::MessageDelta { delta, usage } => self.on_message_delta(delta, usage),
            AnthropicEvent::MessageStop => {
                self.stopped = true;
                smallvec![]
            }
            AnthropicEvent::Ping => smallvec![],
            AnthropicEvent::Error { error } => {
                // Surface as a non-fatal event; the HTTP layer decides whether
                // to retry. We don't classify here because we don't have the
                // status code context.
                smallvec![StreamEvent::Error(elena_types::ElenaError::LlmApi(
                    elena_types::LlmApiError::Unknown {
                        message: format!("server error event: {error}"),
                    }
                ))]
            }
        }
    }

    /// Finalize and return the assembled message, usage, and stop reason.
    ///
    /// Callable whether or not `message_stop` was seen — useful for
    /// partial-flush on aborted streams.
    #[must_use]
    pub fn finish(mut self) -> FinishedStream {
        // Reassemble blocks in index order.
        let mut indices: Vec<u32> = self.blocks.keys().copied().collect();
        indices.sort_unstable();
        let content: Vec<ContentBlock> = indices
            .into_iter()
            .filter_map(|i| self.blocks.remove(&i).map(accumulator_to_block))
            .collect();

        let message_id = self.message_id.unwrap_or_default();
        let message = Message {
            id: message_id,
            thread_id: self.thread_id,
            tenant_id: self.tenant_id,
            role: Role::Assistant,
            kind: MessageKind::Assistant { stop_reason: self.stop_reason, is_api_error: false },
            content,
            created_at: chrono::Utc::now(),
            token_count: None,
            parent_id: None,
        };

        FinishedStream { message, usage: self.usage, stop_reason: self.stop_reason }
    }

    // ---- per-event handlers ----

    fn on_message_start(
        &mut self,
        message: crate::events::MessageStartPayload,
    ) -> SmallVec<[StreamEvent; 2]> {
        // The wire `id` is Anthropic-native (`msg_...`); we mint our own
        // `MessageId` for the Elena side so it's a valid ULID.
        let mid = MessageId::new();
        self.message_id = Some(mid);

        if let Some(usage) = message.usage {
            apply_usage_cumulative(&mut self.usage, &usage);
        }
        smallvec![StreamEvent::MessageStart { message_id: mid }]
    }

    fn on_block_start(&mut self, index: u32, block: BlockStart) -> SmallVec<[StreamEvent; 2]> {
        match block {
            BlockStart::Text { text, cache_control } => {
                self.blocks.insert(index, BlockAccumulator::Text { text, cache_control });
                smallvec![]
            }
            BlockStart::Thinking { thinking, signature, cache_control } => {
                self.blocks.insert(
                    index,
                    BlockAccumulator::Thinking { thinking, signature, cache_control },
                );
                smallvec![]
            }
            BlockStart::RedactedThinking { data } => {
                self.blocks.insert(index, BlockAccumulator::RedactedThinking { data });
                smallvec![]
            }
            BlockStart::ToolUse { id, name, input, cache_control } => {
                // `input` at block start is usually {}; we start the raw
                // accumulator empty, not from the start-value, so partial
                // JSON deltas concatenate cleanly.
                let _ = input;
                let starter = StreamEvent::ToolUseStart { id, name: name.clone() };
                self.blocks.insert(
                    index,
                    BlockAccumulator::ToolUse { id, name, input_raw: String::new(), cache_control },
                );
                smallvec![starter]
            }
        }
    }

    fn on_block_delta(&mut self, index: u32, delta: BlockDelta) -> SmallVec<[StreamEvent; 2]> {
        match (self.blocks.get_mut(&index), delta) {
            (Some(BlockAccumulator::Text { text, .. }), BlockDelta::TextDelta { text: chunk }) => {
                text.push_str(&chunk);
                smallvec![StreamEvent::TextDelta { delta: chunk }]
            }
            (
                Some(BlockAccumulator::Thinking { thinking, .. }),
                BlockDelta::ThinkingDelta { thinking: chunk },
            ) => {
                thinking.push_str(&chunk);
                smallvec![StreamEvent::ThinkingDelta { delta: chunk }]
            }
            (
                Some(BlockAccumulator::Thinking { signature, .. }),
                BlockDelta::SignatureDelta { signature: sig },
            ) => {
                *signature = sig;
                smallvec![]
            }
            (
                Some(BlockAccumulator::ToolUse { id, input_raw, .. }),
                BlockDelta::InputJsonDelta { partial_json },
            ) => {
                input_raw.push_str(&partial_json);
                smallvec![StreamEvent::ToolUseInputDelta { id: *id, partial_json }]
            }
            // Any mismatch (delta shape that doesn't match the block kind at
            // the same index) is silently tolerated — Anthropic won't send
            // these, but we'd rather drop the event than crash the stream.
            _ => smallvec![],
        }
    }

    fn on_block_stop(&mut self, index: u32) -> SmallVec<[StreamEvent; 2]> {
        match self.blocks.get(&index) {
            Some(BlockAccumulator::ToolUse { id, name, input_raw, .. }) => {
                // Parse accumulated JSON. An empty `input_raw` is valid and
                // represents `{}` on the wire (no parameters).
                let parsed = if input_raw.is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(input_raw).unwrap_or_else(|_| {
                        // Fallback: surface as raw string so the downstream
                        // loop can report the malformed tool input rather
                        // than silently dropping it.
                        serde_json::Value::String(input_raw.clone())
                    })
                };
                smallvec![StreamEvent::ToolUseComplete {
                    id: *id,
                    name: name.clone(),
                    input: parsed,
                }]
            }
            // Text/thinking/image/redacted blocks emit no explicit end event;
            // downstream code can observe block finalization by watching for
            // the next block's start or `MessageStop`.
            Some(_) | None => smallvec![],
        }
    }

    fn on_message_delta(
        &mut self,
        delta: MessageDeltaPayload,
        usage: Option<Usage>,
    ) -> SmallVec<[StreamEvent; 2]> {
        let mut events: SmallVec<[StreamEvent; 2]> = smallvec![];

        if let Some(usage) = usage {
            apply_usage_cumulative(&mut self.usage, &usage);
            events.push(StreamEvent::UsageUpdate(self.usage.clone()));
        }

        if let Some(stop) = delta.stop_reason {
            self.stop_reason = parse_stop_reason(&stop);
        }

        events
    }
}

fn accumulator_to_block(acc: BlockAccumulator) -> ContentBlock {
    match acc {
        BlockAccumulator::Text { text, cache_control } => {
            ContentBlock::Text { text, cache_control }
        }
        BlockAccumulator::Thinking { thinking, signature, cache_control } => {
            ContentBlock::Thinking { thinking, signature, cache_control }
        }
        BlockAccumulator::RedactedThinking { data } => ContentBlock::RedactedThinking { data },
        BlockAccumulator::ToolUse { id, name, input_raw, cache_control } => {
            let input = if input_raw.is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&input_raw).unwrap_or(serde_json::Value::String(input_raw))
            };
            ContentBlock::ToolUse { id, name, input, cache_control }
        }
        BlockAccumulator::Image { media_type, source } => {
            ContentBlock::Image { media_type, source }
        }
    }
}

/// Apply an incoming cumulative usage sample to the running total.
///
/// Implements the `>0` guard on input-related fields that the TS source
/// uses: an explicit 0 in a later event means "unset in this frame", not
/// "zero tokens used" — so we only overwrite when the incoming value is
/// nonzero.
fn apply_usage_cumulative(running: &mut Usage, incoming: &Usage) {
    if incoming.input_tokens > 0 {
        running.input_tokens = incoming.input_tokens;
    }
    if incoming.cache_creation_input_tokens > 0 {
        running.cache_creation_input_tokens = incoming.cache_creation_input_tokens;
    }
    if incoming.cache_read_input_tokens > 0 {
        running.cache_read_input_tokens = incoming.cache_read_input_tokens;
    }
    if incoming.cache_creation.ephemeral_1h_input_tokens > 0 {
        running.cache_creation.ephemeral_1h_input_tokens =
            incoming.cache_creation.ephemeral_1h_input_tokens;
    }
    if incoming.cache_creation.ephemeral_5m_input_tokens > 0 {
        running.cache_creation.ephemeral_5m_input_tokens =
            incoming.cache_creation.ephemeral_5m_input_tokens;
    }

    // Output tokens are always latest-wins — they only grow as the stream
    // progresses, and zero is a legitimate starting state.
    running.output_tokens = incoming.output_tokens;

    // Server tool counts latest-wins.
    running.server_tool_use = incoming.server_tool_use;

    // Tier / geo / speed — latest-wins, overwriting with the most recent
    // provider report.
    running.service_tier = incoming.service_tier;
    running.speed = incoming.speed;
    if !incoming.inference_geo.is_empty() {
        running.inference_geo.clone_from(&incoming.inference_geo);
    }
}

/// Parse the Anthropic wire stop-reason string into an Elena [`StopReason`].
fn parse_stop_reason(s: &str) -> Option<StopReason> {
    match s {
        "end_turn" => Some(StopReason::EndTurn),
        "max_tokens" => Some(StopReason::MaxTokens),
        "tool_use" => Some(StopReason::ToolUse),
        "stop_sequence" => Some(StopReason::StopSequence),
        "refusal" => Some(StopReason::Refusal),
        "pause_turn" => Some(StopReason::PauseTurn),
        _ => None,
    }
}

// Keep the ToolResultContent import referenced so future expansions that
// handle it don't trip `unused_imports` on the fresh `use` line.
#[allow(dead_code)]
fn _reserved(_: ToolResultContent) {}

#[cfg(test)]
mod tests {
    use elena_types::{
        CacheControl, CacheTtl, ContentBlock, StopReason, StreamEvent, TenantId, ThreadId,
        ToolCallId,
    };

    use super::*;
    use crate::events::{
        AnthropicEvent, BlockDelta, BlockStart, MessageDeltaPayload, MessageStartPayload,
    };

    fn assembler() -> StreamAssembler {
        StreamAssembler::new(TenantId::new(), ThreadId::new())
    }

    fn start_usage(input: u64) -> Usage {
        Usage { input_tokens: input, ..Default::default() }
    }

    #[test]
    fn text_block_roundtrip() {
        let mut a = assembler();
        a.handle(AnthropicEvent::MessageStart {
            message: MessageStartPayload {
                id: "msg_01".into(),
                role: None,
                model: None,
                usage: Some(start_usage(10)),
            },
        });
        a.handle(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: BlockStart::Text { text: String::new(), cache_control: None },
        });
        let deltas = a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: "Hello ".into() },
        });
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], StreamEvent::TextDelta { .. }));

        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: "world".into() },
        });
        a.handle(AnthropicEvent::ContentBlockStop { index: 0 });
        a.handle(AnthropicEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some("end_turn".into()),
                stop_sequence: None,
            },
            usage: Some(Usage { output_tokens: 5, ..Default::default() }),
        });
        a.handle(AnthropicEvent::MessageStop);

        let done = a.finish();
        assert_eq!(done.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(done.message.content.len(), 1);
        match &done.message.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "Hello world"),
            other => panic!("expected text block, got {other:?}"),
        }
        assert_eq!(done.usage.input_tokens, 10);
        assert_eq!(done.usage.output_tokens, 5);
    }

    #[test]
    fn input_zero_guard_preserves_earlier_nonzero() {
        let mut a = assembler();
        a.handle(AnthropicEvent::MessageStart {
            message: MessageStartPayload {
                id: "msg".into(),
                role: None,
                model: None,
                usage: Some(Usage { input_tokens: 100, ..Default::default() }),
            },
        });
        // message_delta arrives with an explicit 0 — must NOT clobber.
        a.handle(AnthropicEvent::MessageDelta {
            delta: MessageDeltaPayload { stop_reason: None, stop_sequence: None },
            usage: Some(Usage { input_tokens: 0, output_tokens: 42, ..Default::default() }),
        });
        let done = a.finish();
        assert_eq!(done.usage.input_tokens, 100, "nonzero input preserved");
        assert_eq!(done.usage.output_tokens, 42);
    }

    #[test]
    fn tool_use_block_parses_accumulated_json() {
        let mut a = assembler();
        let tool_id = ToolCallId::new();

        a.handle(AnthropicEvent::MessageStart {
            message: MessageStartPayload { id: "msg".into(), role: None, model: None, usage: None },
        });
        let starts = a.handle(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: BlockStart::ToolUse {
                id: tool_id,
                name: "weather".into(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        });
        assert!(matches!(starts[0], StreamEvent::ToolUseStart { .. }));

        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta { partial_json: "{\"city\":".into() },
        });
        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta { partial_json: " \"NYC\"}".into() },
        });
        let completes = a.handle(AnthropicEvent::ContentBlockStop { index: 0 });
        assert_eq!(completes.len(), 1);
        match &completes[0] {
            StreamEvent::ToolUseComplete { id, name, input } => {
                assert_eq!(*id, tool_id);
                assert_eq!(name, "weather");
                assert_eq!(input, &serde_json::json!({"city": "NYC"}));
            }
            other => panic!("expected ToolUseComplete, got {other:?}"),
        }

        a.handle(AnthropicEvent::MessageStop);
        let done = a.finish();
        match &done.message.content[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "weather");
                assert_eq!(input, &serde_json::json!({"city": "NYC"}));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn thinking_block_signature_captured() {
        let mut a = assembler();
        a.handle(AnthropicEvent::MessageStart {
            message: MessageStartPayload { id: "msg".into(), role: None, model: None, usage: None },
        });
        a.handle(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: BlockStart::Thinking {
                thinking: String::new(),
                signature: String::new(),
                cache_control: Some(CacheControl::ephemeral().with_ttl(CacheTtl::FiveMin)),
            },
        });
        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::ThinkingDelta { thinking: "Let me think...".into() },
        });
        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::SignatureDelta { signature: "sig-abc".into() },
        });
        a.handle(AnthropicEvent::ContentBlockStop { index: 0 });
        a.handle(AnthropicEvent::MessageStop);

        let done = a.finish();
        match &done.message.content[0] {
            ContentBlock::Thinking { thinking, signature, cache_control } => {
                assert_eq!(thinking, "Let me think...");
                assert_eq!(signature, "sig-abc");
                assert!(cache_control.is_some());
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn mixed_blocks_preserve_index_order() {
        let mut a = assembler();
        a.handle(AnthropicEvent::MessageStart {
            message: MessageStartPayload { id: "msg".into(), role: None, model: None, usage: None },
        });

        // Block 0: text "Hello ". Block 1: tool_use. Block 2: text "!".
        a.handle(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: BlockStart::Text { text: String::new(), cache_control: None },
        });
        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta { text: "Hello ".into() },
        });
        a.handle(AnthropicEvent::ContentBlockStop { index: 0 });

        let tool_id = ToolCallId::new();
        a.handle(AnthropicEvent::ContentBlockStart {
            index: 1,
            content_block: BlockStart::ToolUse {
                id: tool_id,
                name: "ping".into(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        });
        a.handle(AnthropicEvent::ContentBlockStop { index: 1 });

        a.handle(AnthropicEvent::ContentBlockStart {
            index: 2,
            content_block: BlockStart::Text { text: String::new(), cache_control: None },
        });
        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 2,
            delta: BlockDelta::TextDelta { text: "!".into() },
        });
        a.handle(AnthropicEvent::ContentBlockStop { index: 2 });
        a.handle(AnthropicEvent::MessageStop);

        let done = a.finish();
        assert_eq!(done.message.content.len(), 3);
        assert!(
            matches!(done.message.content[0], ContentBlock::Text { ref text, .. } if text == "Hello ")
        );
        assert!(matches!(done.message.content[1], ContentBlock::ToolUse { .. }));
        assert!(
            matches!(done.message.content[2], ContentBlock::Text { ref text, .. } if text == "!")
        );
    }

    #[test]
    fn empty_tool_input_is_object() {
        let mut a = assembler();
        a.handle(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: BlockStart::ToolUse {
                id: ToolCallId::new(),
                name: "noop".into(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        });
        let evs = a.handle(AnthropicEvent::ContentBlockStop { index: 0 });
        match &evs[0] {
            StreamEvent::ToolUseComplete { input, .. } => {
                assert_eq!(input, &serde_json::json!({}));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn ping_is_no_op() {
        let mut a = assembler();
        let out = a.handle(AnthropicEvent::Ping);
        assert!(out.is_empty());
    }

    #[test]
    fn message_stop_sets_finished_flag() {
        let mut a = assembler();
        assert!(!a.is_finished());
        a.handle(AnthropicEvent::MessageStop);
        assert!(a.is_finished());
    }

    #[test]
    fn delta_for_missing_block_is_silently_dropped() {
        let mut a = assembler();
        // No block-start at index 5, but a delta arrives.
        let out = a.handle(AnthropicEvent::ContentBlockDelta {
            index: 5,
            delta: BlockDelta::TextDelta { text: "oops".into() },
        });
        assert!(out.is_empty());
    }

    #[test]
    fn malformed_tool_input_surfaces_as_string() {
        let mut a = assembler();
        let tool_id = ToolCallId::new();
        a.handle(AnthropicEvent::ContentBlockStart {
            index: 0,
            content_block: BlockStart::ToolUse {
                id: tool_id,
                name: "x".into(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        });
        a.handle(AnthropicEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta { partial_json: "not-valid-json{".into() },
        });
        let evs = a.handle(AnthropicEvent::ContentBlockStop { index: 0 });
        match &evs[0] {
            StreamEvent::ToolUseComplete { input, .. } => {
                // Malformed JSON falls through to a raw-string surface so
                // the agentic loop can classify the error downstream.
                assert_eq!(input, &serde_json::json!("not-valid-json{"));
            }
            other => panic!("got {other:?}"),
        }
    }
}
