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
use elena_plugins::PluginManifest;
use elena_types::TenantId;
use serde::{Deserialize, Serialize};

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

/// One row in the response from `GET /admin/v1/plugins`.
#[derive(Debug, Serialize)]
pub struct RegisteredPluginView {
    /// Plugin id (e.g. `"slack"`).
    pub plugin_id: String,
    /// Free-form name from the manifest.
    pub name: String,
    /// Plugin version string.
    pub version: String,
    /// Action names exposed by this plugin (just the names, not the
    /// schemas — the LLM-facing schemas are large and not useful here).
    pub actions: Vec<String>,
    /// Number of actions advertised by the manifest. Convenient for
    /// dashboards that don't want to count.
    pub action_count: usize,
}

impl From<PluginManifest> for RegisteredPluginView {
    fn from(m: PluginManifest) -> Self {
        let actions: Vec<String> = m.actions.iter().map(|a| a.name.clone()).collect();
        let action_count = actions.len();
        Self {
            plugin_id: m.id.into_inner(),
            name: m.name,
            version: m.version,
            actions,
            action_count,
        }
    }
}

/// `GET /admin/v1/plugins` — list every plugin whose sidecar answered
/// `GetManifest()` at gateway boot. The BFF uses this to detect "Slack
/// configured but plugin not loaded" and to surface a banner instead of
/// letting the LLM hallucinate when the tool schema is empty.
///
/// Returns 503 if the registry was never attached (test / smoke build).
pub async fn list_registered(State(state): State<AdminState>) -> impl IntoResponse {
    let Some(plugins) = state.plugins else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "plugin registry not attached to AdminState",
        )
            .into_response();
    };
    let view: Vec<RegisteredPluginView> =
        plugins.manifests().into_iter().map(RegisteredPluginView::from).collect();
    (StatusCode::OK, Json(view)).into_response()
}
