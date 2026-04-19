//! Plugin CRUD — `/admin/v1/plugins`.
//!
//! Phase 7 ships the ownership-management subset: creators register
//! plugins (via the plugin gRPC sidecar at startup) and operators
//! declare which tenants see them via this endpoint. Re-registration
//! itself still flows through gateway boot, not the admin API; that
//! lands in a v1.0.x patch.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use elena_types::TenantId;
use serde::Deserialize;

use crate::state::AdminState;

/// Request body for `PUT /admin/v1/plugins/:plugin_id/owners`.
#[derive(Debug, Deserialize)]
pub struct SetOwnersRequest {
    /// New set of tenant owners. Empty = global plugin (visible to all
    /// tenants).
    pub owners: Vec<TenantId>,
}

/// `PUT /admin/v1/plugins/:plugin_id/owners` — replace the plugin's
/// ownership set. Idempotent; empty `owners` publishes the plugin as
/// global.
pub async fn set_owners(
    State(state): State<AdminState>,
    Path(plugin_id): Path<String>,
    Json(req): Json<SetOwnersRequest>,
) -> impl IntoResponse {
    match state.store.plugin_ownerships.set_owners(&plugin_id, &req.owners).await {
        Ok(()) => (StatusCode::OK, "updated").into_response(),
        Err(e) => {
            tracing::error!(?e, %plugin_id, "set_owners failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
