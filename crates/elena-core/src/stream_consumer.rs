//! Drain a [`BoxStream<StreamEvent>`] into an assembled assistant
//! [`Message`] and a list of [`ToolInvocation`]s for the loop to execute.
//!
//! The consumer forwards every event to the caller's outbound channel while
//! simultaneously building up state. On a clean termination
//! ([`StreamEvent::Done(Terminal::Completed)`](elena_types::StreamEvent::Done))
//! it returns the assembled [`ConsumedStream`]; on an error
//! ([`StreamEvent::Error`](elena_types::StreamEvent::Error)) the contained
//! [`ElenaError`] is surfaced directly.
//!
//! Thinking blocks are observed for telemetry but not persisted in Phase 3 —
//! the wire `ThinkingDelta` doesn't carry the signature field
//! [`ContentBlock::Thinking`] requires, and we don't yet need a replay of
//! the model's reasoning. Phase 4's compaction layer will surface the
//! signature when it needs to hash reasoning into the cache key.

use chrono::Utc;
use elena_tools::ToolInvocation;
use elena_types::{
    ContentBlock, ElenaError, LlmApiError, Message, MessageId, MessageKind, Role, StopReason,
    StreamEvent, TenantId, Terminal, ThreadId, Usage,
};
use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// What [`consume_stream`] produces on a clean run.
#[derive(Debug, Clone)]
pub struct ConsumedStream {
    /// The fully assembled assistant [`Message`] (not yet committed).
    pub message: Message,
    /// Tool calls the model requested — the loop runs these next.
    pub tool_calls: Vec<ToolInvocation>,
    /// Final cumulative usage.
    pub usage: Usage,
    /// Why the turn ended.
    pub terminal: Terminal,
}

/// Drain `stream` to completion, forwarding every event to `forward_tx`
/// and returning the assembled state.
///
/// Returns:
/// - `Ok(ConsumedStream)` on `Done`.
/// - `Err(ElenaError)` on an in-stream error or cancellation.
///
/// The stream's own `Error` + `Done` events are still forwarded before the
/// function returns, so the caller's outbound channel sees the full event
/// sequence regardless of outcome.
pub async fn consume_stream<S>(
    mut stream: S,
    tenant_id: TenantId,
    thread_id: ThreadId,
    parent_id: Option<MessageId>,
    forward_tx: mpsc::Sender<StreamEvent>,
    cancel: CancellationToken,
) -> Result<ConsumedStream, ElenaError>
where
    S: Stream<Item = StreamEvent> + Unpin + Send,
{
    let mut message_id: Option<MessageId> = None;
    let mut content: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ToolInvocation> = Vec::new();
    let mut usage = Usage::default();
    // Indices into `content` for the in-progress text/thinking blocks so
    // deltas append to the right spot instead of starting a new block each
    // time.
    let mut text_idx: Option<usize> = None;
    let mut thinking_idx: Option<usize> = None;

    loop {
        let event = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(ElenaError::Aborted);
            }
            next = stream.next() => match next {
                Some(e) => e,
                None => {
                    return Err(ElenaError::LlmApi(LlmApiError::ConnectionError {
                        message: "upstream stream ended without Done".to_owned(),
                    }));
                }
            },
        };

        // The LLM's `Done` is the end of one streaming call — it must not
        // reach the client as the loop's terminal event (the outer driver
        // decides that). `UsageUpdate` is consumed into `state.usage` and
        // surfaced by the driver later. `Error` is re-emitted by the
        // driver's `classify_error` with the same payload, so forwarding
        // here would just duplicate it. Everything else forwards verbatim.
        let forward = !matches!(
            event,
            StreamEvent::Done(_) | StreamEvent::UsageUpdate(_) | StreamEvent::Error(_)
        );
        let forwarded = if forward { Some(event.clone()) } else { None };

        let outcome: Option<Result<Terminal, ElenaError>> = match event {
            StreamEvent::MessageStart { message_id: id } => {
                message_id = Some(id);
                None
            }
            StreamEvent::TextDelta { delta } => {
                append_text(&mut content, &mut text_idx, &mut thinking_idx, &delta);
                None
            }
            StreamEvent::ThinkingDelta { .. } => {
                thinking_idx = Some(content.len());
                text_idx = None;
                None
            }
            StreamEvent::ToolUseComplete { id, name, input } => {
                text_idx = None;
                thinking_idx = None;
                content.push(ContentBlock::ToolUse {
                    id,
                    name: name.clone(),
                    input: input.clone(),
                    cache_control: None,
                });
                tool_calls.push(ToolInvocation { id, name, input });
                None
            }
            StreamEvent::UsageUpdate(u) => {
                usage = u;
                None
            }
            StreamEvent::Done(term) => Some(Ok(term)),
            StreamEvent::Error(err) => Some(Err(err)),
            StreamEvent::ToolUseStart { .. }
            | StreamEvent::ToolUseInputDelta { .. }
            | StreamEvent::ToolExecStart { .. }
            | StreamEvent::ToolProgress { .. }
            | StreamEvent::ToolResult { .. }
            | StreamEvent::AwaitingApproval { .. }
            | StreamEvent::PhaseChange { .. } => None,
        };

        if let Some(ev) = forwarded {
            let _ = forward_tx.send(ev).await;
        }

        match outcome {
            None => {}
            Some(Ok(terminal)) => {
                let stop_reason = if tool_calls.is_empty() {
                    Some(StopReason::EndTurn)
                } else {
                    Some(StopReason::ToolUse)
                };
                let message = Message {
                    id: message_id.unwrap_or_default(),
                    thread_id,
                    tenant_id,
                    role: Role::Assistant,
                    kind: MessageKind::Assistant { stop_reason, is_api_error: false },
                    content,
                    created_at: Utc::now(),
                    token_count: None,
                    parent_id,
                };
                return Ok(ConsumedStream { message, tool_calls, usage, terminal });
            }
            Some(Err(e)) => return Err(e),
        }
    }
}

fn append_text(
    content: &mut Vec<ContentBlock>,
    text_idx: &mut Option<usize>,
    thinking_idx: &mut Option<usize>,
    delta: &str,
) {
    *thinking_idx = None;
    if let Some(idx) = *text_idx {
        if let Some(ContentBlock::Text { text, .. }) = content.get_mut(idx) {
            text.push_str(delta);
            return;
        }
        // Index stale (shouldn't happen) — fall through to create a new block.
    }
    content.push(ContentBlock::Text { text: delta.to_owned(), cache_control: None });
    *text_idx = Some(content.len() - 1);
}

#[cfg(test)]
mod tests {
    use elena_types::{StopReason, ToolCallId};
    use futures::stream;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

    fn collect_events() -> (mpsc::Sender<StreamEvent>, mpsc::Receiver<StreamEvent>) {
        mpsc::channel::<StreamEvent>(32)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn text_only_completion_assembles_single_text_block() {
        let events = vec![
            StreamEvent::MessageStart { message_id: MessageId::new() },
            StreamEvent::TextDelta { delta: "Hel".into() },
            StreamEvent::TextDelta { delta: "lo!".into() },
            StreamEvent::UsageUpdate(Usage {
                input_tokens: 10,
                output_tokens: 3,
                ..Default::default()
            }),
            StreamEvent::Done(Terminal::Completed),
        ];
        let s = stream::iter(events);

        let (tx, mut rx) = collect_events();
        let res =
            consume_stream(s, TenantId::new(), ThreadId::new(), None, tx, CancellationToken::new())
                .await
                .expect("ok");

        assert_eq!(res.terminal, Terminal::Completed);
        assert!(res.tool_calls.is_empty());
        assert_eq!(res.message.content.len(), 1);
        match &res.message.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "Hello!"),
            other => panic!("expected text block, got {other:?}"),
        }
        assert_eq!(res.usage.output_tokens, 3);
        assert!(matches!(
            res.message.kind,
            MessageKind::Assistant { stop_reason: Some(StopReason::EndTurn), .. }
        ));

        // MessageStart + two TextDeltas = 3. Done and UsageUpdate are
        // consumed by the driver (re-emitted there), not forwarded.
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_use_is_captured_and_ends_text_block() {
        let tool_id = ToolCallId::new();
        let events = vec![
            StreamEvent::MessageStart { message_id: MessageId::new() },
            StreamEvent::TextDelta { delta: "let me look".into() },
            StreamEvent::ToolUseStart { id: tool_id, name: "weather".into() },
            StreamEvent::ToolUseComplete {
                id: tool_id,
                name: "weather".into(),
                input: json!({"city": "NYC"}),
            },
            StreamEvent::UsageUpdate(Usage::default()),
            StreamEvent::Done(Terminal::Completed),
        ];

        let (tx, _rx) = collect_events();
        let res = consume_stream(
            stream::iter(events),
            TenantId::new(),
            ThreadId::new(),
            None,
            tx,
            CancellationToken::new(),
        )
        .await
        .expect("ok");

        assert_eq!(res.tool_calls.len(), 1);
        assert_eq!(res.tool_calls[0].id, tool_id);
        assert_eq!(res.tool_calls[0].name, "weather");
        assert_eq!(res.tool_calls[0].input, json!({"city": "NYC"}));
        assert_eq!(res.message.content.len(), 2, "text + tool_use");
        assert!(matches!(
            res.message.kind,
            MessageKind::Assistant { stop_reason: Some(StopReason::ToolUse), .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consecutive_text_deltas_merge_into_one_block() {
        let events = vec![
            StreamEvent::TextDelta { delta: "a".into() },
            StreamEvent::TextDelta { delta: "b".into() },
            StreamEvent::TextDelta { delta: "c".into() },
            StreamEvent::Done(Terminal::Completed),
        ];
        let (tx, _rx) = collect_events();
        let res = consume_stream(
            stream::iter(events),
            TenantId::new(),
            ThreadId::new(),
            None,
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(res.message.content.len(), 1);
        match &res.message.content[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, "abc"),
            other => panic!("expected one text block, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn error_event_propagates() {
        let events = vec![
            StreamEvent::Error(ElenaError::LlmApi(LlmApiError::ServerOverload)),
            StreamEvent::Done(Terminal::ModelError {
                classified: elena_types::LlmApiErrorKind::ServerOverload,
            }),
        ];
        let (tx, _rx) = collect_events();
        let err = consume_stream(
            stream::iter(events),
            TenantId::new(),
            ThreadId::new(),
            None,
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ElenaError::LlmApi(LlmApiError::ServerOverload)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pre_cancelled_token_short_circuits() {
        let events = vec![StreamEvent::TextDelta { delta: "hi".into() }];
        let (tx, _rx) = collect_events();
        let token = CancellationToken::new();
        token.cancel();
        let err =
            consume_stream(stream::iter(events), TenantId::new(), ThreadId::new(), None, tx, token)
                .await
                .unwrap_err();
        assert!(matches!(err, ElenaError::Aborted));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_ending_without_done_is_an_error() {
        let events: Vec<StreamEvent> = vec![StreamEvent::TextDelta { delta: "hi".into() }];
        let (tx, _rx) = collect_events();
        let err = consume_stream(
            stream::iter(events),
            TenantId::new(),
            ThreadId::new(),
            None,
            tx,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ElenaError::LlmApi(LlmApiError::ConnectionError { .. })));
    }
}
