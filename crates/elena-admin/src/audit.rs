//! Audit-event reader — `GET /admin/v1/tenants/:id/audit-events`.
//!
//! `tenant_id` is taken from the URL path only — never from a header — so
//! handlers cannot accidentally honour a client-supplied tenant scope.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use elena_store::{AuditCursor, AuditEvent, AuditQueryFilter};
use elena_types::{TenantId, ThreadId, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::require_tenant_scope;
use crate::state::AdminState;

const DEFAULT_AUDIT_LIMIT: u32 = 100;
const MAX_AUDIT_LIMIT: u32 = 500;

/// Query string for `GET /admin/v1/tenants/:id/audit-events`.
#[derive(Debug, Deserialize, Default)]
pub struct AuditQuery {
    /// Inclusive lower bound on `created_at`.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `created_at`.
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Exact match on `kind`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Restrict to one thread.
    #[serde(default)]
    pub thread_id: Option<ThreadId>,
    /// Restrict to one workspace.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    /// Page size (default 100, max 500).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque next-page cursor returned by the prior request.
    #[serde(default)]
    pub before_cursor: Option<String>,
}

/// One audit row in the response.
#[derive(Debug, Serialize)]
pub struct AuditEventResponse {
    /// Event id.
    pub id: uuid::Uuid,
    /// Tenant scope.
    pub tenant_id: TenantId,
    /// Workspace scope (if any).
    pub workspace_id: Option<WorkspaceId>,
    /// Thread scope (if any).
    pub thread_id: Option<ThreadId>,
    /// `system` or `user`.
    pub actor: String,
    /// Short kebab-case kind (`tool_use`, `approval_decision`, ...).
    pub kind: String,
    /// Structured payload, kind-dependent.
    pub payload: Value,
    /// Event timestamp.
    pub created_at: DateTime<Utc>,
}

impl From<AuditEvent> for AuditEventResponse {
    fn from(e: AuditEvent) -> Self {
        Self {
            id: e.id,
            tenant_id: e.tenant_id,
            workspace_id: e.workspace_id,
            thread_id: e.thread_id,
            actor: e.actor,
            kind: e.kind,
            payload: e.payload,
            created_at: e.created_at,
        }
    }
}

/// Page of audit events plus an opaque next-page cursor.
#[derive(Debug, Serialize)]
pub struct AuditPage {
    /// Events for this page, newest-first.
    pub events: Vec<AuditEventResponse>,
    /// Cursor for the next page; absent on the final page.
    pub next_cursor: Option<String>,
}

/// `GET /admin/v1/tenants/:id/audit-events` — paginated tenant audit history.
pub async fn list_audit_events(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    headers: HeaderMap,
    Query(q): Query<AuditQuery>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }

    let before_cursor = match q.before_cursor.as_deref() {
        Some(s) => match AuditCursor::decode(s) {
            Ok(c) => Some(c),
            Err(e) => {
                return (StatusCode::BAD_REQUEST, format!("invalid cursor: {e}")).into_response();
            }
        },
        None => None,
    };

    let limit = q.limit.unwrap_or(DEFAULT_AUDIT_LIMIT).clamp(1, MAX_AUDIT_LIMIT);

    let filter = AuditQueryFilter {
        since: q.since,
        until: q.until,
        kind: q.kind,
        thread_id: q.thread_id,
        workspace_id: q.workspace_id,
        limit,
        before_cursor,
    };

    match state.store.audit_reads.query(tenant_id, &filter).await {
        Ok((events, next)) => {
            let body = AuditPage {
                events: events.into_iter().map(AuditEventResponse::from).collect(),
                next_cursor: next.map(|c| c.encode()),
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %tenant_id, "list_audit_events failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
