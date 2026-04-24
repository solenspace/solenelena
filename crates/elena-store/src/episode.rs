//! [`EpisodeStore`] — per-workspace memory backed by the `episodes` table
//! with pgvector similarity search.
//!
//! The table + HNSW index is provisioned by
//! `20260417000006_episodes.sql`. `insert_episode` writes a row +
//! embedding atomically, and `similar_episodes` returns the
//! cosine-nearest rows within a workspace.

use chrono::{DateTime, Utc};
use elena_types::{EpisodeId, Outcome, StoreError, TenantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::{PgPool, postgres::PgRow};

use crate::sql_error::{classify_serde, classify_sqlx};

/// One stored episodic-memory record.
///
/// This type lives in `elena-store` rather than `elena-memory` so the store
/// can own the SQL roundtrip without forcing a dep cycle. `elena-memory`
/// re-exports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    /// Unique episode id.
    pub id: EpisodeId,
    /// Tenant that owns the workspace this episode belongs to.
    pub tenant_id: TenantId,
    /// Workspace — the recall scope.
    pub workspace_id: WorkspaceId,
    /// Human-readable summary of the task the thread solved.
    pub task_summary: String,
    /// Tool names invoked during the thread, in order.
    pub actions: Vec<String>,
    /// How the thread ended.
    pub outcome: Outcome,
    /// When the episode was recorded.
    pub created_at: DateTime<Utc>,
}

/// Postgres-backed episode persistence.
#[derive(Debug, Clone)]
pub struct EpisodeStore {
    pool: PgPool,
}

impl EpisodeStore {
    pub(crate) const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a new episode row. `embedding` is stored verbatim — empty slice
    /// inserts a `NULL` (row exists but is excluded from similarity search).
    pub async fn insert_episode(
        &self,
        episode: &Episode,
        embedding: &[f32],
    ) -> Result<(), StoreError> {
        let actions_json =
            serde_json::to_value(&episode.actions).map_err(|e| classify_serde(&e))?;
        let outcome_json =
            serde_json::to_value(&episode.outcome).map_err(|e| classify_serde(&e))?;
        let embedding_opt: Option<pgvector::Vector> = if embedding.is_empty() {
            None
        } else {
            Some(pgvector::Vector::from(embedding.to_vec()))
        };

        sqlx::query(
            "INSERT INTO episodes (id, tenant_id, workspace_id, task_summary, actions, outcome, embedding, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(episode.id.as_uuid())
        .bind(episode.tenant_id.as_uuid())
        .bind(episode.workspace_id.as_uuid())
        .bind(&episode.task_summary)
        .bind(actions_json)
        .bind(outcome_json)
        .bind(embedding_opt)
        .bind(episode.created_at)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        Ok(())
    }

    /// Return the `top_k` most cosine-similar episodes within a workspace.
    ///
    /// Rows with `NULL` embedding are skipped. Empty embedding → empty result.
    pub async fn similar_episodes(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        embedding: &[f32],
        top_k: u32,
    ) -> Result<Vec<Episode>, StoreError> {
        if embedding.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let vec = pgvector::Vector::from(embedding.to_vec());
        let capped = i64::from(top_k.min(50));
        let rows = sqlx::query(
            "SELECT id, tenant_id, workspace_id, task_summary, actions, outcome, created_at
             FROM episodes
             WHERE tenant_id = $1 AND workspace_id = $2 AND embedding IS NOT NULL
             ORDER BY embedding <=> $3
             LIMIT $4",
        )
        .bind(tenant_id.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(vec)
        .bind(capped)
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(row_to_episode).collect()
    }

    /// Return the `limit` most recently inserted episodes for a workspace.
    /// Intended for operator tooling / debugging; production recall goes
    /// through [`Self::similar_episodes`].
    pub async fn recent_episodes(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        limit: u32,
    ) -> Result<Vec<Episode>, StoreError> {
        let capped = i64::from(limit.min(200));
        let rows = sqlx::query(
            "SELECT id, tenant_id, workspace_id, task_summary, actions, outcome, created_at
             FROM episodes
             WHERE tenant_id = $1 AND workspace_id = $2
             ORDER BY created_at DESC
             LIMIT $3",
        )
        .bind(tenant_id.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(capped)
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(row_to_episode).collect()
    }
}

fn row_to_episode(row: &PgRow) -> Result<Episode, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let tenant_id: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let workspace_id: uuid::Uuid = row.try_get("workspace_id").map_err(classify_sqlx)?;
    let task_summary: String = row.try_get("task_summary").map_err(classify_sqlx)?;
    let actions_json: serde_json::Value = row.try_get("actions").map_err(classify_sqlx)?;
    let outcome_json: serde_json::Value = row.try_get("outcome").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;

    let actions: Vec<String> =
        serde_json::from_value(actions_json).map_err(|e| classify_serde(&e))?;
    let outcome: Outcome = serde_json::from_value(outcome_json).map_err(|e| classify_serde(&e))?;

    Ok(Episode {
        id: EpisodeId::from_uuid(id),
        tenant_id: TenantId::from_uuid(tenant_id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        task_summary,
        actions,
        outcome,
        created_at,
    })
}
