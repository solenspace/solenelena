//! Tenant CRUD — `/admin/v1/tenants`.
//!
//! Wraps [`elena_store::TenantStore`]. Every route is idempotent; this is
//! deliberate so operator-driven retry is safe.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use elena_store::TenantRecord;
use elena_types::{BudgetLimits, PermissionSet, TenantId, TenantTier};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Phase 7 · A4 — plugin allow-list. Empty = no filter.
    #[serde(default)]
    pub allowed_plugin_ids: Vec<String>,
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

/// `PATCH /admin/v1/tenants/:id/allowed-plugins` — A4 allow-list swap.
pub async fn update_allowed_plugins(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
    Json(req): Json<UpdateAllowedPluginsRequest>,
) -> impl IntoResponse {
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
        created_at: now,
        updated_at: now,
    };
    match state.store.tenants.upsert_tenant(&record).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(TenantResponse { id: record.id, name: record.name, tier: record.tier }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(?e, "create_tenant failed");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("tenant upsert failed: {e}"))
                .into_response()
        }
    }
}

/// `GET /admin/v1/tenants/:id` — fetch a tenant by id.
pub async fn get_tenant(
    State(state): State<AdminState>,
    Path(id): Path<TenantId>,
) -> impl IntoResponse {
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
    Json(req): Json<UpdateBudgetRequest>,
) -> impl IntoResponse {
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
