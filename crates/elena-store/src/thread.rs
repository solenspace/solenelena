//! Thread and message persistence.
//!
//! Every method takes a [`TenantId`] parameter explicitly — multi-tenancy
//! is part of the type signature of each query, not an ambient state. Queries
//! filter on `tenant_id` at the SQL layer; a mismatch between the row's
//! owner and the requested tenant surfaces as
//! [`StoreError::TenantMismatch`].

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use elena_types::{
    ContentBlock, Message, MessageId, MessageKind, Role, StoreError, TenantId, ThreadId, UserId,
    WorkspaceId,
};
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder, Row, postgres::PgRow};
use tracing::instrument;

use crate::sql_error::{classify_serde, classify_sqlx};

/// One thread row, surfaced to the admin API.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRecord {
    /// Thread identifier.
    pub id: ThreadId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// User this thread belongs to.
    pub user_id: UserId,
    /// Workspace the thread runs under.
    pub workspace_id: WorkspaceId,
    /// Optional human-readable title.
    pub title: Option<String>,
    /// Opaque per-thread metadata (caller-defined).
    pub metadata: HashMap<String, Value>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last update (bumped by `threads_updated_at`).
    pub updated_at: DateTime<Utc>,
    /// Latest message timestamp; `None` for empty threads.
    pub last_message_at: Option<DateTime<Utc>>,
}

/// Filter passed to [`ThreadStore::list_threads`].
#[derive(Debug, Clone, Default)]
pub struct ThreadListFilter {
    /// Restrict to a single workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Keyset cursor — return threads with `last_message_at < before`.
    /// Threads whose `last_message_at` is NULL are ordered last and are
    /// excluded once a cursor is set.
    pub before: Option<DateTime<Utc>>,
    /// Page size.
    pub limit: u32,
}

/// X3 — Lightweight per-message metadata projection. Skips
/// `content` and `kind`-payload deserialization, so callers that only
/// care about role / kind-tag / parent / timestamp avoid hauling the
/// full JSONB content across the wire and through serde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    /// Stable message id.
    pub id: MessageId,
    /// Optional parent message id (threading).
    pub parent_id: Option<MessageId>,
    /// Speaker role.
    pub role: Role,
    /// Discriminator tag of [`MessageKind`] (e.g. `"user"`,
    /// `"assistant"`, `"tool_result"`). Used by callers that only
    /// need to know "was this an assistant turn" without inspecting
    /// fields like `stop_reason`.
    pub kind_tag: String,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
}

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

    /// List threads for a tenant, newest-first by `last_message_at`.
    ///
    /// Threads with no messages yet (`last_message_at IS NULL`) are surfaced
    /// at the tail of the first page only — once `filter.before` is set the
    /// keyset query strictly orders by `last_message_at` and excludes them.
    #[instrument(skip(self, filter), fields(tenant_id = %tenant_id, limit = filter.limit))]
    pub async fn list_threads(
        &self,
        tenant_id: TenantId,
        filter: &ThreadListFilter,
    ) -> Result<Vec<ThreadRecord>, StoreError> {
        let mut qb = QueryBuilder::<sqlx::Postgres>::new(
            "SELECT id, tenant_id, user_id, workspace_id, title, metadata,
                    created_at, updated_at, last_message_at
             FROM threads WHERE tenant_id = ",
        );
        qb.push_bind(tenant_id.as_uuid());

        if let Some(workspace_id) = filter.workspace_id {
            qb.push(" AND workspace_id = ").push_bind(workspace_id.as_uuid());
        }
        if let Some(before) = filter.before {
            qb.push(" AND last_message_at IS NOT NULL AND last_message_at < ")
                .push_bind(before);
        }

        qb.push(" ORDER BY last_message_at DESC NULLS LAST, id DESC LIMIT ")
            .push_bind(i64::from(filter.limit));

        let rows = qb.build().fetch_all(&self.pool).await.map_err(classify_sqlx)?;
        rows.iter().map(decode_thread).collect()
    }

    /// Fetch a single thread, scoped to its owning tenant.
    ///
    /// Returns `Ok(None)` if no row exists; returns
    /// [`StoreError::TenantMismatch`] on cross-tenant access.
    #[instrument(skip(self))]
    pub async fn get_thread(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
    ) -> Result<Option<ThreadRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, tenant_id, user_id, workspace_id, title, metadata,
                    created_at, updated_at, last_message_at
             FROM threads WHERE id = $1",
        )
        .bind(thread_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else { return Ok(None) };

        let owner_uuid: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
        let owner = TenantId::from_uuid(owner_uuid);
        if owner != tenant_id {
            return Err(StoreError::TenantMismatch { owner, requested: tenant_id });
        }

        Ok(Some(decode_thread(&row)?))
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

    /// X10 — Bulk-insert several messages from the same thread/tenant.
    ///
    /// Used by the loop's tool-execution phase to commit every
    /// `tool_result` row from one batch in a single round-trip rather
    /// than N. Caller guarantees all messages share the same `tenant_id`
    /// and `thread_id`; if not, the touched-thread UPDATE may pick the
    /// wrong row's timestamp (this is a "fast path for the common
    /// shape" rather than a fully general bulk insert).
    ///
    /// No-op for an empty slice.
    pub async fn append_messages_bulk(&self, msgs: &[Message]) -> Result<(), StoreError> {
        let Some(first) = msgs.first() else { return Ok(()) };
        let mut qb = QueryBuilder::<sqlx::Postgres>::new(
            "INSERT INTO messages (id, thread_id, tenant_id, parent_id, role, kind, content, token_count, created_at) ",
        );
        // Pre-serialize each message's JSONB columns into owned values
        // to satisfy QueryBuilder's borrow shape.
        let mut prepared: Vec<(serde_json::Value, serde_json::Value, &'static str)> =
            Vec::with_capacity(msgs.len());
        for msg in msgs {
            let kind_json = serde_json::to_value(&msg.kind).map_err(|e| classify_serde(&e))?;
            let content_json =
                serde_json::to_value(&msg.content).map_err(|e| classify_serde(&e))?;
            prepared.push((kind_json, content_json, role_str(msg.role)));
        }
        qb.push_values(msgs.iter().zip(prepared.iter()), |mut b, (msg, (k, c, role))| {
            b.push_bind(msg.id.as_uuid())
                .push_bind(msg.thread_id.as_uuid())
                .push_bind(msg.tenant_id.as_uuid())
                .push_bind(msg.parent_id.map(|id| id.as_uuid()))
                .push_bind(*role)
                .push_bind(k)
                .push_bind(c)
                .push_bind(msg.token_count.map(|n| i32::try_from(n).unwrap_or(i32::MAX)))
                .push_bind(msg.created_at);
        });
        qb.build().execute(&self.pool).await.map_err(classify_sqlx)?;

        // Single thread-touch using the latest message's timestamp.
        let latest = msgs.iter().map(|m| m.created_at).max().unwrap_or(first.created_at);
        sqlx::query("UPDATE threads SET last_message_at = $1 WHERE id = $2 AND tenant_id = $3")
            .bind(latest)
            .bind(first.thread_id.as_uuid())
            .bind(first.tenant_id.as_uuid())
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

    /// X3 — Lightweight per-message metadata, no content blocks.
    ///
    /// Returns `MessageSummary` rows in created-at ascending order so
    /// callers like `latest_assistant_message_id` walk the list in
    /// reverse and pick the most recent assistant. Reads only the
    /// columns it needs (`id`, `role`, `kind`, `parent_id`, `created_at`), so a
    /// 100-turn thread costs ~10× less per-row work than the full
    /// `list_messages`.
    pub async fn list_message_summaries(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
        limit: u32,
    ) -> Result<Vec<MessageSummary>, StoreError> {
        let capped_limit = i64::from(limit.min(1_000));
        let rows = sqlx::query(
            "SELECT id, parent_id, role, kind, created_at
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
        .map_err(classify_sqlx)?;

        rows.iter().map(row_to_summary).collect()
    }

    /// Append a message, silently ignoring a primary-key conflict.
    ///
    /// Used from the worker: NATS `JetStream` has at-least-once
    /// semantics, so the same [`WorkRequest`](elena_types::WorkRequest)
    /// may be redelivered after a worker crash. Stamping the message
    /// with a client-supplied ID + inserting idempotently makes the
    /// retry safe.
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
    /// Currently always returns an empty vector — message-level
    /// embeddings are written as `NULL` at this layer; episodic recall
    /// lives in `elena-memory`. The API is exposed so callers can
    /// integrate against the stable signature.
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

fn decode_thread(row: &PgRow) -> Result<ThreadRecord, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let tenant_id: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let user_id: uuid::Uuid = row.try_get("user_id").map_err(classify_sqlx)?;
    let workspace_id: uuid::Uuid = row.try_get("workspace_id").map_err(classify_sqlx)?;
    let title: Option<String> = row.try_get("title").map_err(classify_sqlx)?;
    let metadata: Value = row.try_get("metadata").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(classify_sqlx)?;
    let last_message_at: Option<DateTime<Utc>> =
        row.try_get("last_message_at").map_err(classify_sqlx)?;

    Ok(ThreadRecord {
        id: ThreadId::from_uuid(id),
        tenant_id: TenantId::from_uuid(tenant_id),
        user_id: UserId::from_uuid(user_id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        title,
        metadata: serde_json::from_value(metadata).map_err(|e| classify_serde(&e))?,
        created_at,
        updated_at,
        last_message_at,
    })
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

/// X3 — Decode the lightweight summary projection. Only reads the
/// `kind` JSONB to extract its top-level `type` discriminator string,
/// so the heavy `MessageKind` deserialize is skipped.
fn row_to_summary(row: &PgRow) -> Result<MessageSummary, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let parent_id: Option<uuid::Uuid> = row.try_get("parent_id").map_err(classify_sqlx)?;
    let role_str_val: String = row.try_get("role").map_err(classify_sqlx)?;
    let kind_json: serde_json::Value = row.try_get("kind").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;

    let kind_tag = kind_json.get("type").and_then(|v| v.as_str()).unwrap_or("unknown").to_owned();

    Ok(MessageSummary {
        id: MessageId::from_uuid(id),
        parent_id: parent_id.map(MessageId::from_uuid),
        role: role_from_str(&role_str_val)?,
        kind_tag,
        created_at,
    })
}
