//! Plan-assignment CRUD —
//! `/admin/v1/tenants/{tenant_id}/assignments`.
//!
//! The (tenant, user?, workspace?) triple is the resolution scope. Upsert
//! is by-scope (idempotent on shape); the assignment `id` stays stable
//! across replaces. Delete is by-scope via query parameters.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use elena_types::{
    PlanAssignment, PlanAssignmentId, PlanId, StoreError, TenantId, UserId, WorkspaceId,
};
use serde::{Deserialize, Serialize};

use crate::auth::require_tenant_scope;
use crate::state::AdminState;

/// Request body for `PUT /admin/v1/tenants/:tenant_id/assignments`.
#[derive(Debug, Deserialize)]
pub struct UpsertAssignmentRequest {
    /// User scope. Null means "applies to every user in the workspace".
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// Workspace scope. Null means "applies to the user across workspaces".
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    /// Plan to assign.
    pub plan_id: PlanId,
}

/// Query string for `DELETE /admin/v1/tenants/:tenant_id/assignments`.
///
/// Both fields are optional; omitting one matches the NULL scope (e.g.
/// `?user_id=01...` deletes the user-only assignment). The all-omit case
/// is rejected so admins can't accidentally clear the tenant default
/// (which doesn't live here anyway, but the user might think it does).
#[derive(Debug, Deserialize)]
pub struct AssignmentScopeQuery {
    /// User scope to match.
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// Workspace scope to match.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
}

/// Response shape for assignment reads.
#[derive(Debug, Serialize, Deserialize)]
pub struct AssignmentResponse {
    /// Stable assignment id.
    pub id: PlanAssignmentId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// User scope.
    pub user_id: Option<UserId>,
    /// Workspace scope.
    pub workspace_id: Option<WorkspaceId>,
    /// Selected plan.
    pub plan_id: PlanId,
    /// When the assignment becomes effective.
    pub effective_at: DateTime<Utc>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
}

impl From<PlanAssignment> for AssignmentResponse {
    fn from(a: PlanAssignment) -> Self {
        Self {
            id: a.id,
            tenant_id: a.tenant_id,
            user_id: a.user_id,
            workspace_id: a.workspace_id,
            plan_id: a.plan_id,
            effective_at: a.effective_at,
            created_at: a.created_at,
            updated_at: a.updated_at,
        }
    }
}

/// `PUT /admin/v1/tenants/:tenant_id/assignments` — upsert by scope.
pub async fn upsert_assignment(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    headers: HeaderMap,
    Json(req): Json<UpsertAssignmentRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state
        .store
        .plan_assignments
        .upsert(tenant_id, req.user_id, req.workspace_id, req.plan_id)
        .await
    {
        Ok(assignment) => {
            (StatusCode::OK, Json(AssignmentResponse::from(assignment))).into_response()
        }
        Err(StoreError::Conflict(msg)) => (StatusCode::BAD_REQUEST, msg).into_response(),
        Err(e) => {
            tracing::error!(?e, "upsert_assignment failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/tenants/:tenant_id/assignments` — list assignments.
pub async fn list_assignments(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.plan_assignments.list_by_tenant(tenant_id).await {
        Ok(rows) => {
            let resp: Vec<AssignmentResponse> =
                rows.into_iter().map(AssignmentResponse::from).collect();
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "list_assignments failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `DELETE /admin/v1/tenants/:tenant_id/assignments?user_id=&workspace_id=`.
///
/// Returns 204 on a found-and-deleted match, 404 when no row matched.
/// The all-omit case (no scope) is rejected with 400 to avoid confusing
/// admins who think the tenant default lives in this table.
pub async fn delete_assignment(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    Query(scope): Query<AssignmentScopeQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    if scope.user_id.is_none() && scope.workspace_id.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "delete requires at least one of ?user_id= or ?workspace_id=",
        )
            .into_response();
    }
    match state
        .store
        .plan_assignments
        .delete_for_scope(tenant_id, scope.user_id, scope.workspace_id)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no matching assignment").into_response(),
        Err(e) => {
            tracing::error!(?e, "delete_assignment failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
