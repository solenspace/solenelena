//! Tenant CRUD — `/admin/v1/tenants`.
//!
//! Wraps [`elena_store::TenantStore`]. Every route is idempotent; this is
//! deliberate so operator-driven retry is safe.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::DateTime;
use chrono::Utc;
use elena_store::{TenantListFilter, TenantRecord};
use elena_types::{
    AppId, BudgetLimits, PermissionSet, Plan, PlanId, PlanSlug, TenantId, TenantTier,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{hash_scope_token, require_tenant_scope};
use crate::state::AdminState;

/// Request body for `POST /admin/v1/tenants`.
#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    /// Tenant identifier. Operators supply stable external IDs.
    pub id: TenantId,
    /// Human-readable name shown in dashboards and error messages.
    pub name: String,
    /// Pricing tier. Budget defaults to
    /// [`BudgetLimits::DEFAULT_PRO`] when unset.
    #[serde(default)]
    pub tier: TenantTier,
    /// Optional budget override.
    #[serde(default)]
    pub budget: Option<BudgetLimits>,
    /// Optional permission-set override.
    #[serde(default)]
    pub permissions: Option<PermissionSet>,
    /// Opaque metadata map.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    /// Plugin allow-list. Empty = no filter.
    #[serde(default)]
    pub allowed_plugin_ids: Vec<String>,
    /// Optional owning app. `None` keeps the tenant detached from any app.
    #[serde(default)]
    pub app_id: Option<AppId>,
}

/// Response shape for tenant reads.
#[derive(Debug, Serialize, Deserialize)]
pub struct TenantResponse {
    /// Stable tenant id.
    pub id: TenantId,
    /// Human-readable name.
    pub name: String,
    /// Pricing tier.
    pub tier: TenantTier,
}

/// Request body for `PATCH /admin/v1/tenants/:id/budget`.
#[derive(Debug, Deserialize)]
pub struct UpdateBudgetRequest {
    /// New budget. Replaces the existing row atomically.
    pub budget: BudgetLimits,
}

/// Request body for `PATCH /admin/v1/tenants/:id/allowed-plugins`.
#[derive(Debug, Deserialize)]
pub struct UpdateAllowedPluginsRequest {
    /// Replacement allow-list.
    pub allowed_plugin_ids: Vec<String>,
}

/// `PATCH /admin/v1/tenants/:id/allowed-plugins` — allow-list swap.
pub async fn update_allowed_plugins(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    headers: HeaderMap,
    Json(req): Json<UpdateAllowedPluginsRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, id, &headers).await {
        return s.into_response();
    }
    match state.store.tenants.update_allowed_plugins(id, &req.allowed_plugin_ids).await {
        Ok(()) => (StatusCode::OK, "updated").into_response(),
        Err(elena_types::StoreError::TenantNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("tenant {id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "update_allowed_plugins failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `POST /admin/v1/tenants` — upsert a tenant.
pub async fn create_tenant(
    State(state): State<AdminState>,
    Json(req): Json<CreateTenantRequest>,
) -> impl IntoResponse {
    let now = Utc::now();
    let record = TenantRecord {
        id: req.id,
        name: req.name,
        tier: req.tier,
        budget: req.budget.unwrap_or(BudgetLimits::DEFAULT_PRO),
        permissions: req.permissions.unwrap_or_default(),
        metadata: req.metadata,
        allowed_plugin_ids: req.allowed_plugin_ids,
        app_id: req.app_id,
        deleted_at: None,
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = state.store.tenants.upsert_tenant(&record).await {
        tracing::error!(?e, "create_tenant failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("tenant upsert failed: {e}"))
            .into_response();
    }

    // B1 — seed a default plan on first create. The migration backfill
    // covers pre-B1 tenants; new tenants need this so the resolver
    // (which falls back to plans.is_default = true) always finds a row.
    // Re-running create-tenant on an existing tenant skips this step
    // because `default_for_tenant` returns the existing default.
    if state.store.plans.default_for_tenant(record.id).await.is_err() {
        let slug = match record.tier {
            TenantTier::Free => "free",
            TenantTier::Pro => "pro",
            TenantTier::Team => "team",
            TenantTier::Enterprise => "enterprise",
        };
        let plan = Plan {
            id: PlanId::new(),
            tenant_id: record.id,
            slug: PlanSlug::new(slug),
            display_name: slug.to_owned(),
            is_default: true,
            budget: record.budget,
            rate_limits: serde_json::json!({}),
            allowed_plugin_ids: record.allowed_plugin_ids.clone(),
            tier_models: None,
            autonomy_default: elena_types::AutonomyMode::Moderate,
            cache_policy: serde_json::json!({}),
            max_cascade_escalations: 1,
            metadata: std::collections::HashMap::new(),
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = state.store.plans.upsert(&plan).await {
            // Tenant exists; default plan failed to seed. Surface a 500
            // so admin tooling notices and either retries or seeds a
            // plan via POST /plans manually.
            tracing::error!(?e, tenant_id = %record.id, "default plan seed failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("tenant created but default plan seed failed: {e}"),
            )
                .into_response();
        }
    }

    (
        StatusCode::CREATED,
        Json(TenantResponse { id: record.id, name: record.name, tier: record.tier }),
    )
        .into_response()
}

/// `GET /admin/v1/tenants/:id` — fetch a tenant by id.
pub async fn get_tenant(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, id, &headers).await {
        return s.into_response();
    }
    match state.store.tenants.get_tenant(id).await {
        Ok(record) => (
            StatusCode::OK,
            Json(TenantResponse { id: record.id, name: record.name, tier: record.tier }),
        )
            .into_response(),
        Err(elena_types::StoreError::TenantNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("tenant {id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "get_tenant failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `PATCH /admin/v1/tenants/:id/budget` — atomic budget swap.
pub async fn update_budget(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    headers: HeaderMap,
    Json(req): Json<UpdateBudgetRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, id, &headers).await {
        return s.into_response();
    }
    // Re-read + re-upsert to preserve name/tier/metadata. `upsert_tenant`
    // is idempotent on the primary key.
    let current = match state.store.tenants.get_tenant(id).await {
        Ok(r) => r,
        Err(elena_types::StoreError::TenantNotFound(_)) => {
            return (StatusCode::NOT_FOUND, format!("tenant {id} not found")).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };
    let updated = TenantRecord { budget: req.budget, updated_at: Utc::now(), ..current };
    match state.store.tenants.upsert_tenant(&updated).await {
        Ok(()) => (
            StatusCode::OK,
            Json(TenantResponse { id: updated.id, name: updated.name, tier: updated.tier }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(?e, "update_budget failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Request body for `PUT /admin/v1/tenants/:id/admin-scope`.
///
/// Empty `token` clears the per-tenant scope (tenant inherits the
/// global admin token again). Any non-empty value provisions a new
/// scope: the server SHA-256s it on receive and never persists the
/// raw value.
#[derive(Debug, Deserialize)]
pub struct SetAdminScopeRequest {
    /// Plaintext scope token. Server hashes; raw bytes are not
    /// persisted. Empty string clears.
    pub token: String,
}

/// `PUT /admin/v1/tenants/:id/admin-scope` — provision/rotate the
/// per-tenant admin scope. Bootstrapping requires the global
/// `X-Elena-Admin-Token` plus the *current* `X-Elena-Tenant-Scope`
/// (when one is already set), so a leaked global token alone can't
/// rotate a tenant's scope without also holding the existing scope.
pub async fn set_admin_scope(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    headers: HeaderMap,
    Json(req): Json<SetAdminScopeRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, id, &headers).await {
        return s.into_response();
    }
    let new_hash = if req.token.is_empty() { None } else { Some(hash_scope_token(&req.token)) };
    match state.store.tenants.set_admin_scope_hash(id, new_hash.as_ref()).await {
        Ok(()) => {
            let body = if new_hash.is_some() { "scope set" } else { "scope cleared" };
            (StatusCode::OK, body).into_response()
        }
        Err(elena_types::StoreError::TenantNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("tenant {id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %id, "set_admin_scope failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

const DEFAULT_LIST_LIMIT: u32 = 50;
const MAX_LIST_LIMIT: u32 = 200;

/// Query for `GET /admin/v1/tenants`.
#[derive(Debug, Deserialize, Default)]
pub struct ListTenantsQuery {
    /// Filter by owning app.
    #[serde(default)]
    pub app_id: Option<AppId>,
    /// Case-sensitive name prefix.
    #[serde(default)]
    pub name: Option<String>,
    /// Page size (default 50, max 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Offset.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Include soft-deleted tenants.
    #[serde(default)]
    pub include_deleted: bool,
}

/// Listing projection — drops the heavy `metadata` map and `permissions`.
#[derive(Debug, Serialize)]
pub struct TenantSummary {
    /// Tenant id.
    pub id: TenantId,
    /// Display name.
    pub name: String,
    /// Pricing tier.
    pub tier: TenantTier,
    /// Owning app, if any.
    pub app_id: Option<AppId>,
    /// Soft-delete marker.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl From<TenantRecord> for TenantSummary {
    fn from(t: TenantRecord) -> Self {
        Self {
            id: t.id,
            name: t.name,
            tier: t.tier,
            app_id: t.app_id,
            deleted_at: t.deleted_at,
            created_at: t.created_at,
        }
    }
}

/// `GET /admin/v1/tenants` — list tenants.
pub async fn list_tenants(
    State(state): State<AdminState>,
    Query(q): Query<ListTenantsQuery>,
) -> impl IntoResponse {
    let filter = TenantListFilter {
        app_id: q.app_id,
        name_prefix: q.name,
        limit: q.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT),
        offset: q.offset.unwrap_or(0),
        include_deleted: q.include_deleted,
    };
    match state.store.tenants.list_all(&filter).await {
        Ok(rows) => {
            let body: Vec<TenantSummary> = rows.into_iter().map(TenantSummary::from).collect();
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "list_tenants failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Body for `PATCH /admin/v1/tenants/:id/app`.
#[derive(Debug, Deserialize)]
pub struct SetAppRequest {
    /// Owning app. `None` detaches the tenant.
    pub app_id: Option<AppId>,
}

/// `PATCH /admin/v1/tenants/:id/app` — attach/detach a tenant from an app.
pub async fn set_app(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    headers: HeaderMap,
    Json(req): Json<SetAppRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, id, &headers).await {
        return s.into_response();
    }
    match state.store.tenants.set_app(id, req.app_id).await {
        Ok(()) => (StatusCode::OK, "updated").into_response(),
        Err(elena_types::StoreError::TenantNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("tenant {id} not found")).into_response()
        }
        Err(elena_types::StoreError::Conflict(msg)) => (StatusCode::CONFLICT, msg).into_response(),
        Err(e) => {
            tracing::error!(?e, %id, "set_app failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `DELETE /admin/v1/tenants/:id` — soft-delete a tenant.
pub async fn delete_tenant(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, id, &headers).await {
        return s.into_response();
    }
    match state.store.tenants.soft_delete(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(elena_types::StoreError::TenantNotFound(_)) => {
            (StatusCode::NOT_FOUND, format!("tenant {id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %id, "delete_tenant failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
