//! Thread + message + per-thread usage observability for the admin panel.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use elena_store::{MessageSummary, ThreadListFilter, ThreadRecord};
use elena_types::{Role, StoreError, TenantId, ThreadId, UserId, WorkspaceId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::require_tenant_scope;
use crate::state::AdminState;

const DEFAULT_THREAD_LIMIT: u32 = 50;
const MAX_THREAD_LIMIT: u32 = 200;
const DEFAULT_MESSAGE_LIMIT: u32 = 100;
const MAX_MESSAGE_LIMIT: u32 = 500;

/// Query for `GET /admin/v1/tenants/:id/threads`.
#[derive(Debug, Deserialize, Default)]
pub struct ListThreadsQuery {
    /// Restrict to a single workspace.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    /// Keyset cursor — `last_message_at` of the last row from the prior page.
    #[serde(default)]
    pub before: Option<DateTime<Utc>>,
    /// Page size.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Wire shape of a single thread.
#[derive(Debug, Serialize)]
pub struct ThreadResponse {
    /// Thread id.
    pub id: ThreadId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// User the thread belongs to.
    pub user_id: UserId,
    /// Workspace the thread runs under.
    pub workspace_id: WorkspaceId,
    /// Optional title.
    pub title: Option<String>,
    /// Opaque metadata.
    pub metadata: HashMap<String, Value>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Latest message timestamp.
    pub last_message_at: Option<DateTime<Utc>>,
}

impl From<ThreadRecord> for ThreadResponse {
    fn from(t: ThreadRecord) -> Self {
        Self {
            id: t.id,
            tenant_id: t.tenant_id,
            user_id: t.user_id,
            workspace_id: t.workspace_id,
            title: t.title,
            metadata: t.metadata,
            created_at: t.created_at,
            updated_at: t.updated_at,
            last_message_at: t.last_message_at,
        }
    }
}

/// Tenant-scoped query argument for the per-thread observability routes.
#[derive(Debug, Deserialize)]
pub struct ThreadOwnerQuery {
    /// Tenant the thread belongs to. Mismatch returns 404.
    pub tenant_id: TenantId,
}

/// Query for `GET /admin/v1/threads/:id/messages`.
#[derive(Debug, Deserialize)]
pub struct ListMessagesQuery {
    /// Tenant the thread belongs to.
    pub tenant_id: TenantId,
    /// Page size.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Lightweight message projection (no `content` payload — admins don't
/// need to inspect generated text in the inventory view).
#[derive(Debug, Serialize)]
pub struct MessageSummaryResponse {
    /// Message id.
    pub id: elena_types::MessageId,
    /// Parent message (threading).
    pub parent_id: Option<elena_types::MessageId>,
    /// Speaker role.
    pub role: Role,
    /// `MessageKind` discriminator (`user`, `assistant`, `tool_result`, ...).
    pub kind_tag: String,
    /// Created at.
    pub created_at: DateTime<Utc>,
}

impl From<MessageSummary> for MessageSummaryResponse {
    fn from(m: MessageSummary) -> Self {
        Self {
            id: m.id,
            parent_id: m.parent_id,
            role: m.role,
            kind_tag: m.kind_tag,
            created_at: m.created_at,
        }
    }
}

/// Wire shape for `GET /admin/v1/threads/:id/usage`.
#[derive(Debug, Serialize)]
pub struct ThreadUsageResponse {
    /// Tenant.
    pub tenant_id: TenantId,
    /// Thread.
    pub thread_id: ThreadId,
    /// Cumulative tokens spent on this thread (input + output + cache).
    pub tokens_used: u64,
}

/// `GET /admin/v1/tenants/:id/threads`.
pub async fn list_threads(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    headers: HeaderMap,
    Query(q): Query<ListThreadsQuery>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    let limit = q.limit.unwrap_or(DEFAULT_THREAD_LIMIT).clamp(1, MAX_THREAD_LIMIT);
    let filter = ThreadListFilter { workspace_id: q.workspace_id, before: q.before, limit };
    match state.store.threads.list_threads(tenant_id, &filter).await {
        Ok(rows) => {
            let body: Vec<ThreadResponse> = rows.into_iter().map(ThreadResponse::from).collect();
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %tenant_id, "list_threads failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/threads/:id?tenant_id=`.
pub async fn get_thread(
    State(state): State<AdminState>,
    Path(thread_id): Path<ThreadId>,
    headers: HeaderMap,
    Query(q): Query<ThreadOwnerQuery>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, q.tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.threads.get_thread(q.tenant_id, thread_id).await {
        Ok(Some(t)) => (StatusCode::OK, Json(ThreadResponse::from(t))).into_response(),
        Ok(None) | Err(StoreError::TenantMismatch { .. }) => {
            (StatusCode::NOT_FOUND, format!("thread {thread_id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %thread_id, "get_thread failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/threads/:id/messages?tenant_id=&limit=`.
pub async fn list_messages(
    State(state): State<AdminState>,
    Path(thread_id): Path<ThreadId>,
    headers: HeaderMap,
    Query(q): Query<ListMessagesQuery>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, q.tenant_id, &headers).await {
        return s.into_response();
    }

    // Cross-tenant probe: bail with 404 before we hand the (tenant, thread)
    // pair to list_message_summaries — that method trusts the caller.
    match state.store.threads.get_thread(q.tenant_id, thread_id).await {
        Ok(Some(_)) => {}
        Ok(None) | Err(StoreError::TenantMismatch { .. }) => {
            return (StatusCode::NOT_FOUND, format!("thread {thread_id} not found"))
                .into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    let limit = q.limit.unwrap_or(DEFAULT_MESSAGE_LIMIT).clamp(1, MAX_MESSAGE_LIMIT);

    match state.store.threads.list_message_summaries(q.tenant_id, thread_id, limit).await {
        Ok(rows) => {
            let body: Vec<MessageSummaryResponse> =
                rows.into_iter().map(MessageSummaryResponse::from).collect();
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %thread_id, "list_messages failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/threads/:id/usage?tenant_id=`.
pub async fn get_thread_usage(
    State(state): State<AdminState>,
    Path(thread_id): Path<ThreadId>,
    headers: HeaderMap,
    Query(q): Query<ThreadOwnerQuery>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, q.tenant_id, &headers).await {
        return s.into_response();
    }

    match state.store.threads.get_thread(q.tenant_id, thread_id).await {
        Ok(Some(_)) => {}
        Ok(None) | Err(StoreError::TenantMismatch { .. }) => {
            return (StatusCode::NOT_FOUND, format!("thread {thread_id} not found"))
                .into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    }

    match state.store.tenants.get_thread_usage(q.tenant_id, thread_id).await {
        Ok(tokens_used) => (
            StatusCode::OK,
            Json(ThreadUsageResponse { tenant_id: q.tenant_id, thread_id, tokens_used }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(?e, %thread_id, "get_thread_usage failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
