//! `/admin/v1` router composition.

use axum::{
    Router,
    routing::{get, patch, post, put},
};

use crate::{health, plugins, state::AdminState, tenant_credentials, tenants, workspaces};

/// Build the admin router. Caller mounts it at `/admin/v1` on the
/// gateway's main [`axum::Router`].
pub fn admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/tenants", post(tenants::create_tenant))
        .route("/tenants/{id}", get(tenants::get_tenant))
        .route("/tenants/{id}/budget", patch(tenants::update_budget))
        .route("/tenants/{id}/allowed-plugins", patch(tenants::update_allowed_plugins))
        .route(
            "/tenants/{id}/credentials/{plugin_id}",
            put(tenant_credentials::put_credentials).delete(tenant_credentials::delete_credentials),
        )
        .route("/workspaces", post(workspaces::upsert_workspace))
        .route("/workspaces/{id}", get(workspaces::get_workspace))
        .route("/workspaces/{id}/instructions", patch(workspaces::update_instructions))
        .route("/workspaces/{id}/allowed-plugins", patch(workspaces::update_allowed_plugins))
        .route("/plugins/{plugin_id}/owners", put(plugins::set_owners))
        .route("/health/deep", get(health::deep_health))
        .with_state(state)
}
