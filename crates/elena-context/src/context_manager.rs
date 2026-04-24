//! [`ContextManager`] — the high-level API the loop calls to build a context
//! window.
//!
//! Responsibilities:
//!
//! - If the embedder is available (`dimension() > 0`), embed the query and
//!   retrieve the top-k semantically similar messages + the last-N recent
//!   messages, union/dedupe, and pack under a token budget.
//! - Otherwise (`NullEmbedder`), fall back to a pure recency window.
//! - Expose [`embed_and_store`](Self::embed_and_store) so the loop can
//!   persist embeddings as messages land — retrieval's correctness depends
//!   on that happening.
//!
//! Summarization for 200+ turn histories is a separate step (see
//! `summarize::Summarizer`) that the manager optionally invokes. The
//! hook lives here; the default manager leaves it as a no-op summarizer
//! so behavior is deterministic in tests.

use std::collections::HashSet;
use std::sync::Arc;

use elena_store::ThreadStore;
use elena_types::{Message, MessageId, TenantId, ThreadId};
use tracing::warn;

use crate::embedder::Embedder;
use crate::error::ContextError;
use crate::packer::pack;
use crate::tokens::TokenCounter;

/// Defaults controlling retrieval + packing behavior.
#[derive(Debug, Clone, Copy)]
pub struct ContextManagerOptions {
    /// How many recent messages to always include when available.
    pub recency_window: u32,
    /// Top-k neighbors returned by vector search.
    pub retrieval_top_k: u32,
    /// Token budget ceiling; callers can override per call.
    pub default_budget_tokens: u32,
}

impl Default for ContextManagerOptions {
    fn default() -> Self {
        Self { recency_window: 100, retrieval_top_k: 20, default_budget_tokens: 60_000 }
    }
}

/// Assembles the per-turn context window.
#[derive(Clone)]
pub struct ContextManager {
    embedder: Arc<dyn Embedder>,
    counter: TokenCounter,
    options: ContextManagerOptions,
}

impl std::fmt::Debug for ContextManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextManager")
            .field("retrieval_enabled", &(self.embedder.dimension() > 0))
            .field("dim", &self.embedder.dimension())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl ContextManager {
    /// Build a manager from a shared embedder and defaults.
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder>, options: ContextManagerOptions) -> Self {
        Self { embedder, counter: TokenCounter::new(), options }
    }

    /// The embedder this manager delegates to.
    #[must_use]
    pub fn embedder(&self) -> Arc<dyn Embedder> {
        Arc::clone(&self.embedder)
    }

    /// True when retrieval is available.
    #[must_use]
    pub fn retrieval_enabled(&self) -> bool {
        self.embedder.dimension() > 0
    }

    /// Approximate token counter used by the packer. Public so callers can
    /// share the same cost model when they size the model's `max_tokens`.
    #[must_use]
    pub const fn counter(&self) -> TokenCounter {
        self.counter
    }

    /// Build the per-turn context window.
    pub async fn build_context(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
        current_query: &str,
        store: &ThreadStore,
        budget_tokens: u32,
    ) -> Result<Vec<Message>, ContextError> {
        // `list_messages` returns rows in chronological order (oldest first)
        // and caps at 1_000 internally. Fetch a broad slice, filter, then
        // keep only the last `recency_window` to get the newest-N tail.
        let all = store.list_messages(tenant_id, thread_id, 1_000, None).await?;
        let visible = filter_visible(all);
        let keep = self.options.recency_window as usize;
        let start = visible.len().saturating_sub(keep);
        let recent: Vec<Message> = visible.into_iter().skip(start).collect();

        if !self.retrieval_enabled() {
            return Ok(pack(recent, &self.counter, budget_tokens));
        }

        let embedding = match self.embedder.embed(current_query).await {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => return Ok(pack(recent, &self.counter, budget_tokens)),
            Err(err) => {
                warn!(?err, "embedder failed — falling back to recency window");
                return Ok(pack(recent, &self.counter, budget_tokens));
            }
        };

        let similar = store
            .similar_messages(tenant_id, thread_id, &embedding, self.options.retrieval_top_k)
            .await?;
        let similar = filter_visible(similar);

        let merged = merge_chronological(recent, similar);
        Ok(pack(merged, &self.counter, budget_tokens))
    }

    /// Embed `text` and write the resulting vector to the message row.
    ///
    /// No-op when the embedder is null, or when the embedding comes back
    /// empty. Errors propagate as [`ContextError`] so the caller (usually the
    /// loop) can decide whether to log-and-continue.
    pub async fn embed_and_store(
        &self,
        tenant_id: TenantId,
        message_id: MessageId,
        text: &str,
        store: &ThreadStore,
    ) -> Result<(), ContextError> {
        if !self.retrieval_enabled() || text.is_empty() {
            return Ok(());
        }
        let vector = self.embedder.embed(text).await?;
        if vector.is_empty() {
            return Ok(());
        }
        store.set_embedding(tenant_id, message_id, &vector).await?;
        Ok(())
    }
}

/// Filter out rows that the LLM shouldn't see (tombstones, internal
/// system notices, summaries). Mirrors the recency-window
/// `context_builder` rule so the retrieval path doesn't accidentally
/// widen what's shown.
fn filter_visible(messages: Vec<Message>) -> Vec<Message> {
    use elena_types::MessageKind;
    messages
        .into_iter()
        .filter(|m| {
            !matches!(
                m.kind,
                MessageKind::Tombstone { .. }
                    | MessageKind::System(_)
                    | MessageKind::ToolUseSummary
            )
        })
        .collect()
}

/// Merge two lists of messages by id (de-duplicating) and return in
/// chronological order.
fn merge_chronological(a: Vec<Message>, b: Vec<Message>) -> Vec<Message> {
    let mut seen: HashSet<MessageId> = HashSet::with_capacity(a.len() + b.len());
    let mut merged: Vec<Message> = Vec::with_capacity(a.len() + b.len());
    for m in a {
        if seen.insert(m.id) {
            merged.push(m);
        }
    }
    for m in b {
        if seen.insert(m.id) {
            merged.push(m);
        }
    }
    merged.sort_by_key(|m| m.created_at);
    merged
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};
    use elena_types::{ContentBlock, Message, MessageId, MessageKind, Role, TenantId, ThreadId};

    use super::*;
    use crate::embedder::{FakeEmbedder, NullEmbedder};

    fn msg_at(offset_secs: i64, text: &str) -> Message {
        Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::User,
            content: vec![ContentBlock::Text { text: text.into(), cache_control: None }],
            created_at: Utc::now() + Duration::seconds(offset_secs),
            token_count: None,
            parent_id: None,
        }
    }

    #[test]
    fn merge_preserves_uniqueness_and_order() {
        let a = msg_at(0, "oldest");
        let b = msg_at(10, "middle");
        let c = msg_at(20, "newest");
        let merged = merge_chronological(vec![c.clone(), a.clone()], vec![a.clone(), b.clone()]);
        let ids: Vec<_> = merged.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![a.id, b.id, c.id]);
    }

    #[test]
    fn retrieval_disabled_for_null_embedder() {
        let cm = ContextManager::new(
            Arc::new(NullEmbedder) as Arc<dyn crate::embedder::Embedder>,
            ContextManagerOptions::default(),
        );
        assert!(!cm.retrieval_enabled());
    }

    #[test]
    fn retrieval_enabled_for_fake_embedder() {
        let cm = ContextManager::new(
            Arc::new(FakeEmbedder) as Arc<dyn crate::embedder::Embedder>,
            ContextManagerOptions::default(),
        );
        assert!(cm.retrieval_enabled());
    }
}
