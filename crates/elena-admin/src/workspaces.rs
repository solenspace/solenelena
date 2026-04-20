//! Workspace CRUD — `/admin/v1/workspaces`.
//!
//! Wraps [`elena_store::WorkspaceStore`]. Four routes:
//!
//! - `POST   /admin/v1/workspaces`                      — upsert
//! - `GET    /admin/v1/workspaces/:id`                  — fetch
//! - `PATCH  /admin/v1/workspaces/:id/instructions`     — edit the
//!   global-instructions fragment (Solen guardrail).
//! - `PATCH  /admin/v1/workspaces/:id/allowed-plugins`  — replace the
//!   plugin allow-list (A4).
//!
//! Every route is idempotent — re-posting the same body returns the same
//! state. Operators re-run bootstrap scripts and expect retries to work.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use elena_types::{TenantId, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::state::AdminState;

/// Request body for `POST /admin/v1/workspaces`.
#[derive(Debug, Deserialize)]
pub struct UpsertWorkspaceRequest {
    /// Stable workspace identifier.
    pub id: WorkspaceId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Empty string = no guardrail.
    #[serde(default)]
    pub global_instructions: String,
    /// Empty list = defer to the tenant's allow-list.
    #[serde(default)]
    pub allowed_plugin_ids: Vec<String>,
}

/// Response shape for workspace reads.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceResponse {
    /// Stable workspace id.
    pub id: WorkspaceId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Display name.
    pub name: Option<String>,
    /// Current system-prompt fragment.
    pub global_instructions: String,
    /// Current allow-list.
    pub allowed_plugin_ids: Vec<String>,
}

/// Request body for `PATCH .../instructions`.
#[derive(Debug, Deserialize)]
pub struct UpdateInstructionsRequest {
    /// Replacement fragment. Empty string clears it.
    pub global_instructions: String,
}

/// Request body for `PATCH .../allowed-plugins`.
#[derive(Debug, Deserialize)]
pub struct UpdateAllowedPluginsRequest {
    /// Replacement allow-list.
    pub allowed_plugin_ids: Vec<String>,
}

/// `POST /admin/v1/workspaces` — upsert a workspace.
pub async fn upsert_workspace(
    State(state): State<AdminState>,
    Json(req): Json<UpsertWorkspaceRequest>,
) -> impl IntoResponse {
    match state
        .store
        .workspaces
        .upsert(
            req.id,
            req.tenant_id,
            req.name.as_deref(),
            &req.global_instructions,
            &req.allowed_plugin_ids,
        )
        .await
    {
        Ok(record) => (
            StatusCode::CREATED,
            Json(WorkspaceResponse {
                id: record.id,
                tenant_id: record.tenant_id,
                name: record.name,
                global_instructions: record.global_instructions,
                allowed_plugin_ids: record.allowed_plugin_ids,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(?e, "upsert_workspace failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/workspaces/:id` — fetch a workspace.
///
/// Takes `?tenant_id=<uuid>` as a query parameter — the admin surface is
/// pre-authenticated, so we don't carry tenant-from-JWT yet. Operators
/// pass the tenant_id explicitly.
pub async fn get_workspace(
    State(state): State<AdminState>,
    Path(id): Path<WorkspaceId>,
    axum::extract::Query(q): axum::extract::Query<TenantQuery>,
) -> impl IntoResponse {
    match state.store.workspaces.get(q.tenant_id, id).await {
        Ok(Some(record)) => (
            StatusCode::OK,
            Json(WorkspaceResponse {
                id: record.id,
                tenant_id: record.tenant_id,
                name: record.name,
                global_instructions: record.global_instructions,
                allowed_plugin_ids: record.allowed_plugin_ids,
            }),
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, format!("workspace {id} not found")).into_response(),
        Err(elena_types::StoreError::TenantMismatch { .. }) => {
            // Caller asked for a workspace under a tenant that doesn't
            // own it. Surface as 404 (not 500) so the admin endpoint
            // doesn't leak which workspaces exist under other tenants.
            (StatusCode::NOT_FOUND, format!("workspace {id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "get_workspace failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `PATCH /admin/v1/workspaces/:id/instructions` — replace the guardrail.
pub async fn update_instructions(
    State(state): State<AdminState>,
    Path(id): Path<WorkspaceId>,
    axum::extract::Query(q): axum::extract::Query<TenantQuery>,
    Json(req): Json<UpdateInstructionsRequest>,
) -> impl IntoResponse {
    match state
        .store
        .workspaces
        .update_instructions(q.tenant_id, id, &req.global_instructions)
        .await
    {
        Ok(()) => (StatusCode::OK, "updated").into_response(),
        Err(e) => {
            tracing::error!(?e, "update_instructions failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `PATCH /admin/v1/workspaces/:id/allowed-plugins` — replace allow-list.
pub async fn update_allowed_plugins(
    State(state): State<AdminState>,
    Path(id): Path<WorkspaceId>,
    axum::extract::Query(q): axum::extract::Query<TenantQuery>,
    Json(req): Json<UpdateAllowedPluginsRequest>,
) -> impl IntoResponse {
    match state
        .store
        .workspaces
        .update_allowed_plugins(q.tenant_id, id, &req.allowed_plugin_ids)
        .await
    {
        Ok(()) => (StatusCode::OK, "updated").into_response(),
        Err(e) => {
            tracing::error!(?e, "update_allowed_plugins failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Query-string tenant gate. Shared by the `:id`-scoped routes.
#[derive(Debug, Deserialize)]
pub struct TenantQuery {
    /// Owning tenant — enforced at the store layer via `tenant_id` filter.
    pub tenant_id: TenantId,
}
