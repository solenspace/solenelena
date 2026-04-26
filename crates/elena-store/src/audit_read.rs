//! Audit-event read path.
//!
//! Kept separate from [`crate::audit`] so future reader optimisations (read
//! replica, partitioning) don't churn the hot-path writer. The writer is
//! latency-sensitive (bounded mpsc, batched insert); the reader is rare,
//! operator-driven, and tolerates table scans within the keyset window.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use elena_types::{StoreError, TenantId, ThreadId, WorkspaceId};
use sqlx::{PgPool, QueryBuilder, Row};
use tracing::instrument;
use uuid::Uuid;

use crate::audit::AuditEvent;
use crate::sql_error::classify_sqlx;

/// Filter passed to [`AuditReadStore::query`].
///
/// `tenant_id` is **not** part of this struct — it's a separate argument to
/// the query method so handlers cannot accidentally read it from a request
/// body and bypass the URL-derived scope.
#[derive(Debug, Clone, Default)]
pub struct AuditQueryFilter {
    /// Inclusive lower bound on `created_at`.
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `created_at`.
    pub until: Option<DateTime<Utc>>,
    /// Exact match on `kind` (e.g. `"tool_use"`, `"approval_decision"`).
    pub kind: Option<String>,
    /// Restrict to a single thread.
    pub thread_id: Option<ThreadId>,
    /// Restrict to a single workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Page size. Callers should clamp this (e.g. min(query, 200)) before
    /// passing it through; the store applies no implicit cap.
    pub limit: u32,
    /// Pagination cursor — the last `(created_at, id)` of the previous page.
    /// Inclusive of equal `created_at` rows but strictly less than `id`.
    pub before_cursor: Option<AuditCursor>,
}

/// Keyset-pagination cursor: encodes the `(created_at, id)` of the last row
/// returned in the prior page so the next page picks up exactly after it.
///
/// Wire format is `<rfc3339-timestamp>|<uuid>` then base64(url-safe, no pad)
/// to keep cursors opaque to clients but trivially decodable for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCursor {
    /// `created_at` of the last row in the prior page.
    pub created_at: DateTime<Utc>,
    /// `id` of the last row in the prior page.
    pub id: Uuid,
}

impl AuditCursor {
    /// Encode as a base64 string for HTTP query-string transport.
    #[must_use]
    pub fn encode(&self) -> String {
        let raw = format!("{}|{}", self.created_at.to_rfc3339(), self.id);
        URL_SAFE_NO_PAD.encode(raw.as_bytes())
    }

    /// Decode an HTTP cursor string. Returns a [`StoreError::Serialization`]
    /// for malformed input — this is operator-supplied, so a hard error is
    /// the right surface (callers should clear the cursor on retry).
    pub fn decode(s: &str) -> Result<Self, StoreError> {
        let bytes =
            URL_SAFE_NO_PAD.decode(s.as_bytes()).map_err(|e| StoreError::Serialization(e.to_string()))?;
        let raw = std::str::from_utf8(&bytes).map_err(|e| StoreError::Serialization(e.to_string()))?;
        let (ts, id) = raw
            .split_once('|')
            .ok_or_else(|| StoreError::Serialization("cursor missing separator".to_owned()))?;
        let created_at = DateTime::parse_from_rfc3339(ts)
            .map_err(|e| StoreError::Serialization(e.to_string()))?
            .with_timezone(&Utc);
        let id = Uuid::parse_str(id).map_err(|e| StoreError::Serialization(e.to_string()))?;
        Ok(Self { created_at, id })
    }
}

/// Read-only handle to the `audit_events` table.
#[derive(Debug, Clone)]
pub struct AuditReadStore {
    pool: PgPool,
}

impl AuditReadStore {
    /// Build a reader from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Page through a tenant's audit history newest-first.
    ///
    /// Returns the rows plus an optional next-page cursor. The cursor is
    /// `Some` iff `rows.len() == filter.limit` (we returned a full page);
    /// callers stop paginating once they get `None` back.
    #[instrument(skip(self, filter), fields(tenant_id = %tenant_id, limit = filter.limit))]
    pub async fn query(
        &self,
        tenant_id: TenantId,
        filter: &AuditQueryFilter,
    ) -> Result<(Vec<AuditEvent>, Option<AuditCursor>), StoreError> {
        let mut qb = QueryBuilder::<sqlx::Postgres>::new(
            "SELECT id, tenant_id, workspace_id, thread_id, actor, kind, payload, created_at
             FROM audit_events WHERE tenant_id = ",
        );
        qb.push_bind(tenant_id.as_uuid());

        if let Some(since) = filter.since {
            qb.push(" AND created_at >= ").push_bind(since);
        }
        if let Some(until) = filter.until {
            qb.push(" AND created_at < ").push_bind(until);
        }
        if let Some(ref kind) = filter.kind {
            qb.push(" AND kind = ").push_bind(kind);
        }
        if let Some(thread_id) = filter.thread_id {
            qb.push(" AND thread_id = ").push_bind(thread_id.as_uuid());
        }
        if let Some(workspace_id) = filter.workspace_id {
            qb.push(" AND workspace_id = ").push_bind(workspace_id.as_uuid());
        }
        if let Some(cursor) = filter.before_cursor {
            qb.push(" AND (created_at, id) < (")
                .push_bind(cursor.created_at)
                .push(", ")
                .push_bind(cursor.id)
                .push(")");
        }
        qb.push(" ORDER BY created_at DESC, id DESC LIMIT ");
        qb.push_bind(i64::from(filter.limit));

        let rows = qb.build().fetch_all(&self.pool).await.map_err(classify_sqlx)?;

        let events: Vec<AuditEvent> = rows.iter().map(decode_audit_row).collect::<Result<_, _>>()?;

        let next = if events.len() == filter.limit as usize {
            events.last().map(|ev| AuditCursor { created_at: ev.created_at, id: ev.id })
        } else {
            None
        };

        Ok((events, next))
    }
}

fn decode_audit_row(row: &sqlx::postgres::PgRow) -> Result<AuditEvent, StoreError> {
    let id: Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let tenant_id: Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let workspace_id: Option<Uuid> = row.try_get("workspace_id").map_err(classify_sqlx)?;
    let thread_id: Option<Uuid> = row.try_get("thread_id").map_err(classify_sqlx)?;
    let actor: String = row.try_get("actor").map_err(classify_sqlx)?;
    let kind: String = row.try_get("kind").map_err(classify_sqlx)?;
    let payload: serde_json::Value = row.try_get("payload").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;

    Ok(AuditEvent {
        id,
        tenant_id: TenantId::from_uuid(tenant_id),
        workspace_id: workspace_id.map(WorkspaceId::from_uuid),
        thread_id: thread_id.map(ThreadId::from_uuid),
        actor,
        kind,
        payload,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip_is_lossless() {
        let cur = AuditCursor { created_at: Utc::now(), id: Uuid::new_v4() };
        let encoded = cur.encode();
        let back = AuditCursor::decode(&encoded).expect("decode");
        assert_eq!(cur.created_at.timestamp_micros(), back.created_at.timestamp_micros());
        assert_eq!(cur.id, back.id);
    }

    #[test]
    fn cursor_decode_rejects_garbage() {
        assert!(AuditCursor::decode("not-base64!@#").is_err());
        assert!(AuditCursor::decode("aGVsbG8").is_err());
    }
}
