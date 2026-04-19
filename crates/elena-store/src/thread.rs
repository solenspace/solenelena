//! Thread and message persistence.
//!
//! Every method takes a [`TenantId`] parameter explicitly — multi-tenancy
//! is part of the type signature of each query, not an ambient state. Queries
//! filter on `tenant_id` at the SQL layer; a mismatch between the row's
//! owner and the requested tenant surfaces as
//! [`StoreError::TenantMismatch`].

use chrono::{DateTime, Utc};
use elena_types::{
    ContentBlock, Message, MessageId, MessageKind, Role, StoreError, TenantId, ThreadId, UserId,
    WorkspaceId,
};
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::sql_error::{classify_serde, classify_sqlx};

/// Handle to the thread/message persistence layer.
#[derive(Debug, Clone)]
pub struct ThreadStore {
    pool: PgPool,
}

impl ThreadStore {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Access the raw pg pool. Exposed only for integration tests that
    /// need to run ad-hoc queries against the same database (audit-row
    /// assertions, count-by-tenant checks).
    #[doc(hidden)]
    #[must_use]
    pub fn pool_for_test(&self) -> &PgPool {
        &self.pool
    }

    /// Create a new thread for a tenant/user/workspace triple.
    pub async fn create_thread(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        workspace_id: WorkspaceId,
        title: Option<&str>,
    ) -> Result<ThreadId, StoreError> {
        let thread_id = ThreadId::new();
        sqlx::query(
            "INSERT INTO threads (id, tenant_id, user_id, workspace_id, title)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(thread_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .bind(user_id.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(title)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        Ok(thread_id)
    }

    /// Append a message to a thread.
    ///
    /// The message's `tenant_id` field is the authoritative tenant; callers
    /// must set it before invoking.
    pub async fn append_message(&self, msg: &Message) -> Result<(), StoreError> {
        let kind_json = serde_json::to_value(&msg.kind).map_err(|e| classify_serde(&e))?;
        let content_json = serde_json::to_value(&msg.content).map_err(|e| classify_serde(&e))?;
        let role = role_str(msg.role);

        sqlx::query(
            "INSERT INTO messages (id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(msg.id.as_uuid())
        .bind(msg.thread_id.as_uuid())
        .bind(msg.tenant_id.as_uuid())
        .bind(msg.parent_id.map(|id| id.as_uuid()))
        .bind(role)
        .bind(kind_json)
        .bind(content_json)
        .bind(msg.token_count.map(|c| i32::try_from(c).unwrap_or(i32::MAX)))
        .bind(msg.created_at)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        // Touch the parent thread's last_message_at so recency views stay fresh.
        sqlx::query("UPDATE threads SET last_message_at = $1 WHERE id = $2 AND tenant_id = $3")
            .bind(msg.created_at)
            .bind(msg.thread_id.as_uuid())
            .bind(msg.tenant_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(classify_sqlx)?;

        Ok(())
    }

    /// Fetch a single message by ID, enforcing tenant isolation.
    ///
    /// Returns [`StoreError::MessageNotFound`] if the row doesn't exist, or
    /// [`StoreError::TenantMismatch`] if the row is owned by a different
    /// tenant.
    pub async fn get_message(
        &self,
        tenant_id: TenantId,
        id: MessageId,
    ) -> Result<Message, StoreError> {
        // Fetch without tenant filter first so we can distinguish "not found"
        // from "wrong tenant" — important diagnostic.
        let row = sqlx::query(
            "SELECT id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at
             FROM messages WHERE id = $1",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else { return Err(StoreError::MessageNotFound(id)) };
        let msg = row_to_message(&row)?;
        if msg.tenant_id != tenant_id {
            return Err(StoreError::TenantMismatch { owner: msg.tenant_id, requested: tenant_id });
        }
        Ok(msg)
    }

    /// List messages in a thread, oldest first.
    ///
    /// - `limit`: maximum rows to return (hard-capped at `1_000`).
    /// - `before`: optional message ID; returns only messages strictly older.
    ///
    /// Tenant isolation: rows from other tenants are excluded (not returned
    /// as an error — a tenant querying a thread it doesn't own gets an empty
    /// list, which matches "thread doesn't exist" behavior).
    pub async fn list_messages(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
        limit: u32,
        before: Option<MessageId>,
    ) -> Result<Vec<Message>, StoreError> {
        let capped_limit = i64::from(limit.min(1_000));
        let rows = if let Some(cursor) = before {
            sqlx::query(
                "SELECT id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at
                 FROM messages
                 WHERE tenant_id = $1 AND thread_id = $2
                   AND created_at < (SELECT created_at FROM messages WHERE id = $3)
                 ORDER BY created_at ASC
                 LIMIT $4",
            )
            .bind(tenant_id.as_uuid())
            .bind(thread_id.as_uuid())
            .bind(cursor.as_uuid())
            .bind(capped_limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at
                 FROM messages
                 WHERE tenant_id = $1 AND thread_id = $2
                 ORDER BY created_at ASC
                 LIMIT $3",
            )
            .bind(tenant_id.as_uuid())
            .bind(thread_id.as_uuid())
            .bind(capped_limit)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(classify_sqlx)?;

        rows.iter().map(row_to_message).collect()
    }

    /// Append a message, silently ignoring a primary-key conflict.
    ///
    /// Phase 5 uses this from the worker: NATS `JetStream` has at-least-once
    /// semantics, so the same [`WorkRequest`](elena_types::WorkRequest) may
    /// be redelivered after a worker crash. Stamping the message with a
    /// client-supplied ID + inserting idempotently makes the retry safe.
    ///
    /// Returns `true` if the row was newly inserted, `false` if a row with
    /// the same ID already existed.
    pub async fn append_message_idempotent(&self, msg: &Message) -> Result<bool, StoreError> {
        let kind_json = serde_json::to_value(&msg.kind).map_err(|e| classify_serde(&e))?;
        let content_json = serde_json::to_value(&msg.content).map_err(|e| classify_serde(&e))?;
        let role = role_str(msg.role);

        let result = sqlx::query(
            "INSERT INTO messages (id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(msg.id.as_uuid())
        .bind(msg.thread_id.as_uuid())
        .bind(msg.tenant_id.as_uuid())
        .bind(msg.parent_id.map(|id| id.as_uuid()))
        .bind(role)
        .bind(kind_json)
        .bind(content_json)
        .bind(msg.token_count.map(|c| i32::try_from(c).unwrap_or(i32::MAX)))
        .bind(msg.created_at)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let inserted = result.rows_affected() > 0;
        if inserted {
            // Only touch the thread's recency pointer when we actually wrote.
            sqlx::query("UPDATE threads SET last_message_at = $1 WHERE id = $2 AND tenant_id = $3")
                .bind(msg.created_at)
                .bind(msg.thread_id.as_uuid())
                .bind(msg.tenant_id.as_uuid())
                .execute(&self.pool)
                .await
                .map_err(classify_sqlx)?;
        }
        Ok(inserted)
    }

    /// Write (or overwrite) the embedding vector for a single message.
    ///
    /// Enforces tenant isolation — a write against a message owned by a
    /// different tenant is a silent no-op (0 rows updated). Callers that need
    /// to detect that condition should compare `list_messages` / `get_message`
    /// ownership before invoking.
    pub async fn set_embedding(
        &self,
        tenant_id: TenantId,
        message_id: MessageId,
        embedding: &[f32],
    ) -> Result<(), StoreError> {
        if embedding.is_empty() {
            return Ok(());
        }
        let vec = pgvector::Vector::from(embedding.to_vec());
        sqlx::query("UPDATE messages SET embedding = $1 WHERE id = $2 AND tenant_id = $3")
            .bind(vec)
            .bind(message_id.as_uuid())
            .bind(tenant_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(classify_sqlx)?;
        Ok(())
    }

    /// Find messages in a thread similar to the given embedding.
    ///
    /// Phase 1 always returns an empty vector — embeddings are written as
    /// `NULL` until Phase 4 (`elena-memory`) populates them. The API is
    /// exposed now so callers can integrate against the stable signature.
    pub async fn similar_messages(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
        embedding: &[f32],
        top_k: u32,
    ) -> Result<Vec<Message>, StoreError> {
        if embedding.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let vec = pgvector::Vector::from(embedding.to_vec());
        let capped_k = i64::from(top_k.min(100));
        let rows = sqlx::query(
            "SELECT id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at
             FROM messages
             WHERE tenant_id = $1 AND thread_id = $2 AND embedding IS NOT NULL
             ORDER BY embedding <=> $3
             LIMIT $4",
        )
        .bind(tenant_id.as_uuid())
        .bind(thread_id.as_uuid())
        .bind(vec)
        .bind(capped_k)
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(row_to_message).collect()
    }
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

fn role_from_str(s: &str) -> Result<Role, StoreError> {
    match s {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "system" => Ok(Role::System),
        "tool" => Ok(Role::Tool),
        other => Err(StoreError::Serialization(format!("unknown role: {other}"))),
    }
}

fn row_to_message(row: &PgRow) -> Result<Message, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let thread_id: uuid::Uuid = row.try_get("thread_id").map_err(classify_sqlx)?;
    let tenant_id: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let parent_id: Option<uuid::Uuid> = row.try_get("parent_id").map_err(classify_sqlx)?;
    let role_str_val: String = row.try_get("role").map_err(classify_sqlx)?;
    let kind_json: serde_json::Value = row.try_get("kind").map_err(classify_sqlx)?;
    let content_json: serde_json::Value = row.try_get("content").map_err(classify_sqlx)?;
    let token_count: Option<i32> = row.try_get("token_count").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;

    let kind: MessageKind = serde_json::from_value(kind_json).map_err(|e| classify_serde(&e))?;
    let content: Vec<ContentBlock> =
        serde_json::from_value(content_json).map_err(|e| classify_serde(&e))?;

    Ok(Message {
        id: MessageId::from_uuid(id),
        thread_id: ThreadId::from_uuid(thread_id),
        tenant_id: TenantId::from_uuid(tenant_id),
        parent_id: parent_id.map(MessageId::from_uuid),
        role: role_from_str(&role_str_val)?,
        kind,
        content,
        token_count: token_count.and_then(|c| u32::try_from(c).ok()),
        created_at,
    })
}
