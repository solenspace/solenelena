//! `/admin/v1` router composition.

use axum::{
    Router, middleware,
    routing::{get, patch, post, put},
};

use crate::{
    apps, audit, auth::require_admin_token, budget, health, plan_assignments, plans, plugins,
    state::AdminState, tenant_credentials, tenants, threads, workspaces,
};

/// Build the admin router. Caller mounts it at `/admin/v1` on the
/// gateway's main [`axum::Router`].
///
/// When `state.admin_token.is_some()` every route is wrapped in
/// [`require_admin_token`] middleware that enforces the
/// `X-Elena-Admin-Token` header. Tests and smokes that build the
/// router with `AdminState::new(...)` (no token) are unaffected.
pub fn admin_router(state: AdminState) -> Router {
    let token_for_middleware = state.admin_token.clone();
    Router::new()
        // App registry — admin grouping above tenants.
        .route("/apps", post(apps::create_app).get(apps::list_apps))
        .route("/apps/{id}", get(apps::get_app).patch(apps::update_app).delete(apps::delete_app))
        .route("/apps/{id}/tenants", get(apps::list_app_tenants).post(apps::onboard_tenant))
        .route("/apps/{id}/usage-summary", get(budget::get_usage_summary))
        // Tenants — CRUD + list + cascading delete + soft-delete + set-app.
        .route("/tenants", post(tenants::create_tenant).get(tenants::list_tenants))
        .route("/tenants/{id}", get(tenants::get_tenant).delete(tenants::delete_tenant))
        .route("/tenants/{id}/budget", patch(tenants::update_budget))
        .route("/tenants/{id}/budget-state", get(budget::get_budget_state))
        .route("/tenants/{id}/allowed-plugins", patch(tenants::update_allowed_plugins))
        .route("/tenants/{id}/admin-scope", put(tenants::set_admin_scope))
        .route("/tenants/{id}/app", patch(tenants::set_app))
        .route("/tenants/{id}/soft-delete", post(tenants::soft_delete_tenant))
        .route("/tenants/{id}/audit-events", get(audit::list_audit_events))
        .route("/tenants/{id}/threads", get(threads::list_threads))
        .route("/tenants/{id}/credentials", get(tenant_credentials::list_credentials))
        .route(
            "/tenants/{id}/credentials/{plugin_id}",
            put(tenant_credentials::put_credentials).delete(tenant_credentials::delete_credentials),
        )
        .route("/tenants/{tenant_id}/plans", post(plans::create_plan).get(plans::list_plans))
        .route(
            "/tenants/{tenant_id}/plans/{plan_id}",
            get(plans::get_plan).patch(plans::update_plan).delete(plans::delete_plan),
        )
        .route("/tenants/{tenant_id}/default-plan", patch(plans::set_default_plan))
        .route(
            "/tenants/{tenant_id}/assignments",
            put(plan_assignments::upsert_assignment)
                .get(plan_assignments::list_assignments)
                .delete(plan_assignments::delete_assignment),
        )
        // Threads observability — keyed by thread, scoped via ?tenant_id=.
        .route("/threads/{thread_id}", get(threads::get_thread))
        .route("/threads/{thread_id}/messages", get(threads::list_messages))
        .route("/threads/{thread_id}/usage", get(threads::get_thread_usage))
        // Workspaces — existing CRUD plus list / delete.
        .route("/workspaces", post(workspaces::upsert_workspace).get(workspaces::list_workspaces))
        .route(
            "/workspaces/{id}",
            get(workspaces::get_workspace).delete(workspaces::delete_workspace),
        )
        .route("/workspaces/{id}/instructions", patch(workspaces::update_instructions))
        .route("/workspaces/{id}/allowed-plugins", patch(workspaces::update_allowed_plugins))
        .route("/plugins", get(plugins::list_registered))
        .route("/plugins/{plugin_id}/owners", put(plugins::set_owners))
        .route("/health/deep", get(health::deep_health))
        .route_layer(middleware::from_fn_with_state(token_for_middleware, require_admin_token))
        .with_state(state)
}
