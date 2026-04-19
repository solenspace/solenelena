//! On-demand conversational summarizer.
//!
//! Fallback for the 200+ turn tail: when retrieval can't cram the essentials
//! into the token budget, the loop can invoke [`Summarizer::summarize`] to
//! collapse an old span into a single text paragraph. The summary is stored
//! alongside the thread as a [`MessageKind::System`](elena_types::MessageKind::System)
//! row (compact-boundary) so it survives checkpoint restarts.
//!
//! Phase 4 ships the building block. Wiring into [`ContextManager`] is opt-in
//! — tests + smoke don't need it, and the trigger condition is rare enough
//! that defaulting off keeps behavior deterministic.

use std::sync::Arc;

use elena_llm::{AnthropicClient, CachePolicy, LlmRequest, RequestOptions};
use elena_types::{
    ContentBlock, ElenaError, Message, MessageId, MessageKind, ModelId, Role, StreamEvent,
    TenantContext, TenantId, Terminal, ThreadId,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::error::ContextError;

/// LLM-backed summarizer.
#[derive(Clone)]
pub struct Summarizer {
    client: Arc<AnthropicClient>,
    cache_policy: CachePolicy,
    model: ModelId,
    max_tokens: u32,
}

impl Summarizer {
    /// Build a summarizer bound to a given LLM client + cache policy.
    ///
    /// `model` should be a cheap tier (Fast) — summarization doesn't need
    /// reasoning. `max_tokens` bounds the output; 512 is a reasonable default
    /// for a 2-3 paragraph task summary.
    #[must_use]
    pub fn new(
        client: Arc<AnthropicClient>,
        cache_policy: CachePolicy,
        model: ModelId,
        max_tokens: u32,
    ) -> Self {
        Self { client, cache_policy, model, max_tokens }
    }

    /// Summarize `history` into a single plain-text paragraph.
    ///
    /// `tenant` is stamped into the outgoing `metadata.user_id`. `cancel`
    /// lets the caller abort a slow summarization without leaking the HTTP
    /// request.
    pub async fn summarize(
        &self,
        tenant: &TenantContext,
        history: &[Message],
        cancel: CancellationToken,
    ) -> Result<String, ContextError> {
        let system_text = "You are Elena's context summarizer. Summarize the following \
            conversation into a tight factual paragraph (\u{2264} 400 words). Preserve \
            user intent, decisions, tool results, and open questions. Omit pleasantries \
            and exact wording. Write in the third person.";

        let req = LlmRequest {
            provider: "anthropic".to_owned(),
            model: self.model.clone(),
            tenant: tenant.clone(),
            messages: history.to_vec(),
            system: vec![ContentBlock::Text { text: system_text.to_owned(), cache_control: None }],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: self.max_tokens,
            thinking: None,
            temperature: Some(0.1),
            top_p: None,
            stop_sequences: Vec::new(),
            options: RequestOptions {
                query_source: Some("elena-context-summarizer".into()),
                enable_prompt_caching: true,
                ..RequestOptions::default()
            },
        };

        let mut stream = self.client.stream(req, self.cache_policy.clone(), cancel);
        let mut out = String::new();
        let mut terminal: Option<Terminal> = None;
        while let Some(event) = stream.next().await {
            match event {
                StreamEvent::TextDelta { delta } => out.push_str(&delta),
                StreamEvent::Error(err) => return Err(err.into()),
                StreamEvent::Done(t) => {
                    terminal = Some(t);
                    break;
                }
                _ => {}
            }
        }
        match terminal {
            Some(Terminal::Completed) if !out.is_empty() => Ok(out),
            Some(Terminal::Completed) | None => Err(empty_summary_error()),
            Some(t) => Err(summarizer_terminal_error(&t)),
        }
    }
}

impl std::fmt::Debug for Summarizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Summarizer")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

/// Build a [`MessageKind::System`] row representing a summary boundary —
/// used by callers that want to persist the summarizer's output into the
/// thread history so it survives checkpoint restarts.
#[must_use]
pub fn summary_message(
    tenant_id: TenantId,
    thread_id: ThreadId,
    parent_id: Option<MessageId>,
    summary_text: String,
) -> Message {
    Message {
        id: MessageId::new(),
        thread_id,
        tenant_id,
        role: Role::System,
        kind: MessageKind::System(elena_types::SystemMessageKind::CompactBoundary),
        content: vec![ContentBlock::Text { text: summary_text, cache_control: None }],
        created_at: chrono::Utc::now(),
        token_count: None,
        parent_id,
    }
}

fn empty_summary_error() -> ContextError {
    ContextError::LlmApi(elena_types::LlmApiError::Unknown {
        message: "summarizer produced an empty response".to_owned(),
    })
}

fn summarizer_terminal_error(t: &Terminal) -> ContextError {
    ContextError::LlmApi(elena_types::LlmApiError::Unknown {
        message: format!("summarizer terminal: {}", t.tag()),
    })
}

impl From<ElenaError> for ContextError {
    fn from(err: ElenaError) -> Self {
        match err {
            ElenaError::LlmApi(e) => Self::LlmApi(e),
            ElenaError::Store(e) => Self::Store(e),
            ElenaError::Tool(_)
            | ElenaError::Config(_)
            | ElenaError::ContextOverflow { .. }
            | ElenaError::BudgetExceeded { .. }
            | ElenaError::PermissionDenied { .. }
            | ElenaError::Aborted => {
                Self::LlmApi(elena_types::LlmApiError::Unknown { message: err.to_string() })
            }
        }
    }
}
