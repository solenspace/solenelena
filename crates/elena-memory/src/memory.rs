//! [`EpisodicMemory`] — the public record/recall API Elena's agentic loop
//! consults for cross-session workspace memory.

use std::sync::Arc;

use elena_context::Embedder;
use elena_store::{Episode, EpisodeStore, ThreadStore};
use elena_types::{EpisodeId, Outcome, StoreError, TenantId, ThreadId, WorkspaceId};
use tracing::warn;

use crate::summary::extract_summary;

/// Per-workspace episodic memory.
///
/// Cheap to clone; the `Arc<EpisodeStore>` is shared behind.
#[derive(Debug, Clone)]
pub struct EpisodicMemory {
    store: Arc<EpisodeStore>,
}

impl EpisodicMemory {
    /// Build a memory handle around an [`EpisodeStore`].
    #[must_use]
    pub fn new(store: Arc<EpisodeStore>) -> Self {
        Self { store }
    }

    /// Extract a summary from a completed thread and persist it as an
    /// [`Episode`]. Fire-and-forget semantics: errors are logged, not
    /// returned — a failing recorder should never fail a user-visible turn.
    pub async fn record_episode(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        thread_id: ThreadId,
        threads: &ThreadStore,
        embedder: &dyn Embedder,
        outcome: Outcome,
    ) {
        if let Err(e) = self
            .record_episode_inner(tenant_id, workspace_id, thread_id, threads, embedder, outcome)
            .await
        {
            warn!(
                ?e,
                tenant_id = %tenant_id,
                workspace_id = %workspace_id,
                thread_id = %thread_id,
                "record_episode failed (swallowed)",
            );
        }
    }

    async fn record_episode_inner(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        thread_id: ThreadId,
        threads: &ThreadStore,
        embedder: &dyn Embedder,
        outcome: Outcome,
    ) -> Result<(), StoreError> {
        // 1_000 caps: the store's list_messages itself caps at 1_000.
        let messages = threads.list_messages(tenant_id, thread_id, 1_000, None).await?;
        let summary = extract_summary(&messages);
        let embedding = embedder.embed(&summary.task_summary).await.unwrap_or_default();

        let episode = Episode {
            id: EpisodeId::new(),
            tenant_id,
            workspace_id,
            task_summary: summary.task_summary,
            actions: summary.actions,
            outcome,
            created_at: chrono::Utc::now(),
        };
        self.store.insert_episode(&episode, &embedding).await
    }

    /// Return up to `top_k` episodes in `workspace_id` most similar to
    /// `current_task`. Returns an empty vec when no episodes are present or
    /// the embedder is disabled.
    pub async fn recall(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        current_task: &str,
        embedder: &dyn Embedder,
        top_k: u32,
    ) -> Result<Vec<Episode>, StoreError> {
        if embedder.dimension() == 0 {
            return Ok(Vec::new());
        }
        let embedding = embedder.embed(current_task).await.unwrap_or_default();
        if embedding.is_empty() {
            return Ok(Vec::new());
        }
        self.store.similar_episodes(tenant_id, workspace_id, &embedding, top_k).await
    }
}
