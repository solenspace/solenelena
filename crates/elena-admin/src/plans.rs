//! Plan CRUD — `/admin/v1/tenants/{tenant_id}/plans` and the
//! `/admin/v1/tenants/{tenant_id}/default-plan` swap route.
//!
//! Wraps [`elena_store::PlanStore`]. Each tenant defines its own plans;
//! `is_default` is *not* mutable through the regular create/patch routes —
//! call `PATCH /default-plan` to swap atomically. That keeps the
//! "exactly one default per tenant" invariant easy to reason about.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use elena_types::{AutonomyMode, BudgetLimits, Plan, PlanId, PlanSlug, StoreError, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::require_tenant_scope;
use crate::state::AdminState;

/// Request body for `POST /admin/v1/tenants/:tenant_id/plans`.
#[derive(Debug, Deserialize)]
pub struct CreatePlanRequest {
    /// Tenant-scoped slug. Must be unique within the tenant.
    pub slug: PlanSlug,
    /// Human-readable name shown in admin UIs.
    pub display_name: String,
    /// Hard budget limits applied to threads under this plan.
    pub budget: BudgetLimits,
    /// Rate-limit override bundle. Defaults to `{}` (use system defaults).
    #[serde(default)]
    pub rate_limits: Option<Value>,
    /// Plugin allow-list intersected with the tenant + workspace lists.
    #[serde(default)]
    pub allowed_plugin_ids: Vec<String>,
    /// Optional tier-models override. `None` = inherit tenant/system.
    #[serde(default)]
    pub tier_models: Option<Value>,
    /// Default autonomy mode for new threads under this plan.
    #[serde(default)]
    pub autonomy_default: Option<AutonomyMode>,
    /// Cache-policy override bundle. Defaults to `{}`.
    #[serde(default)]
    pub cache_policy: Option<Value>,
    /// Maximum router cascade escalations under this plan. Defaults to 1.
    #[serde(default)]
    pub max_cascade_escalations: Option<u32>,
    /// App-specific metadata.
    #[serde(default)]
    pub metadata: Option<HashMap<String, Value>>,
}

/// Request body for `PATCH /admin/v1/tenants/:tenant_id/plans/:plan_id`.
///
/// Every field is optional — present fields are applied, missing fields
/// retain the prior value. `is_default` is intentionally absent: use the
/// dedicated `PATCH /default-plan` route.
#[derive(Debug, Deserialize, Default)]
pub struct UpdatePlanRequest {
    /// Replacement display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Replacement budget bundle.
    #[serde(default)]
    pub budget: Option<BudgetLimits>,
    /// Replacement rate-limits bundle.
    #[serde(default)]
    pub rate_limits: Option<Value>,
    /// Replacement plugin allow-list.
    #[serde(default)]
    pub allowed_plugin_ids: Option<Vec<String>>,
    /// Replacement tier-models override. Send `null` to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    pub tier_models: Option<Option<Value>>,
    /// Replacement autonomy default.
    #[serde(default)]
    pub autonomy_default: Option<AutonomyMode>,
    /// Replacement cache-policy bundle.
    #[serde(default)]
    pub cache_policy: Option<Value>,
    /// Replacement max-cascade-escalations cap.
    #[serde(default)]
    pub max_cascade_escalations: Option<u32>,
    /// Replacement metadata map.
    #[serde(default)]
    pub metadata: Option<HashMap<String, Value>>,
}

/// Distinguishes "field absent in PATCH" from "field present and null".
/// JSON `null` clears the override; absence preserves the current value.
#[allow(clippy::option_option)]
fn deserialize_optional_field<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

/// Request body for `PATCH /admin/v1/tenants/:tenant_id/default-plan`.
#[derive(Debug, Deserialize)]
pub struct SetDefaultPlanRequest {
    /// New default plan id. Must already exist under this tenant.
    pub plan_id: PlanId,
}

/// Response shape for plan reads.
#[derive(Debug, Serialize, Deserialize)]
pub struct PlanResponse {
    /// Stable plan id.
    pub id: PlanId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Tenant-scoped slug.
    pub slug: PlanSlug,
    /// Display name.
    pub display_name: String,
    /// True for the tenant's default plan.
    pub is_default: bool,
    /// Budget limits.
    pub budget: BudgetLimits,
    /// Rate-limit override bundle.
    pub rate_limits: Value,
    /// Plugin allow-list.
    pub allowed_plugin_ids: Vec<String>,
    /// Tier-models override.
    pub tier_models: Option<Value>,
    /// Default autonomy mode.
    pub autonomy_default: AutonomyMode,
    /// Cache-policy override bundle.
    pub cache_policy: Value,
    /// Max cascade escalations.
    pub max_cascade_escalations: u32,
    /// App-specific metadata.
    pub metadata: HashMap<String, Value>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
}

impl From<Plan> for PlanResponse {
    fn from(p: Plan) -> Self {
        Self {
            id: p.id,
            tenant_id: p.tenant_id,
            slug: p.slug,
            display_name: p.display_name,
            is_default: p.is_default,
            budget: p.budget,
            rate_limits: p.rate_limits,
            allowed_plugin_ids: p.allowed_plugin_ids,
            tier_models: p.tier_models,
            autonomy_default: p.autonomy_default,
            cache_policy: p.cache_policy,
            max_cascade_escalations: p.max_cascade_escalations,
            metadata: p.metadata,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}

/// `POST /admin/v1/tenants/:tenant_id/plans` — create a plan.
///
/// Always creates with `is_default = false`. The first plan a tenant ever
/// gets (via the create-tenant flow) is seeded as default; later swaps go
/// through `PATCH /default-plan`.
pub async fn create_plan(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,

    headers: HeaderMap,
    Json(req): Json<CreatePlanRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    let now = Utc::now();
    let plan = Plan {
        id: PlanId::new(),
        tenant_id,
        slug: req.slug,
        display_name: req.display_name,
        is_default: false,
        budget: req.budget,
        rate_limits: req.rate_limits.unwrap_or_else(|| serde_json::json!({})),
        allowed_plugin_ids: req.allowed_plugin_ids,
        tier_models: req.tier_models,
        autonomy_default: req.autonomy_default.unwrap_or_default(),
        cache_policy: req.cache_policy.unwrap_or_else(|| serde_json::json!({})),
        max_cascade_escalations: req.max_cascade_escalations.unwrap_or(1),
        metadata: req.metadata.unwrap_or_default(),
        created_at: now,
        updated_at: now,
    };
    match state.store.plans.upsert(&plan).await {
        Ok(()) => (StatusCode::CREATED, Json(PlanResponse::from(plan))).into_response(),
        Err(StoreError::Conflict(msg)) => (StatusCode::CONFLICT, msg).into_response(),
        Err(e) => {
            tracing::error!(?e, "create_plan failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/tenants/:tenant_id/plans` — list every plan for a tenant.
pub async fn list_plans(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.plans.list_by_tenant(tenant_id).await {
        Ok(plans) => {
            let resp: Vec<PlanResponse> = plans.into_iter().map(PlanResponse::from).collect();
            (StatusCode::OK, Json(resp)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "list_plans failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/tenants/:tenant_id/plans/:plan_id`.
pub async fn get_plan(
    State(state): State<AdminState>,
    Path((tenant_id, plan_id)): Path<(TenantId, PlanId)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.plans.get(tenant_id, plan_id).await {
        Ok(Some(plan)) => (StatusCode::OK, Json(PlanResponse::from(plan))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, format!("plan {plan_id} not found")).into_response(),
        // Surface cross-tenant lookups as 404 — same posture as workspaces.rs:
        // don't leak which plans exist under other tenants.
        Err(StoreError::TenantMismatch { .. }) => {
            (StatusCode::NOT_FOUND, format!("plan {plan_id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "get_plan failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `PATCH /admin/v1/tenants/:tenant_id/plans/:plan_id` — partial update.
///
/// Re-reads the current row, overlays the patch, re-upserts. Slug is not
/// patchable (would invalidate any external reference); use create + swap
/// + delete if a slug change is truly needed.
pub async fn update_plan(
    State(state): State<AdminState>,
    Path((tenant_id, plan_id)): Path<(TenantId, PlanId)>,
    headers: HeaderMap,
    Json(req): Json<UpdatePlanRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    let mut plan = match state.store.plans.get(tenant_id, plan_id).await {
        Ok(Some(p)) => p,
        Ok(None) | Err(StoreError::TenantMismatch { .. }) => {
            return (StatusCode::NOT_FOUND, format!("plan {plan_id} not found")).into_response();
        }
        Err(e) => {
            tracing::error!(?e, "update_plan get failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    if let Some(v) = req.display_name {
        plan.display_name = v;
    }
    if let Some(v) = req.budget {
        plan.budget = v;
    }
    if let Some(v) = req.rate_limits {
        plan.rate_limits = v;
    }
    if let Some(v) = req.allowed_plugin_ids {
        plan.allowed_plugin_ids = v;
    }
    if let Some(v) = req.tier_models {
        // Outer Some = field present; inner Option = present value or null.
        plan.tier_models = v;
    }
    if let Some(v) = req.autonomy_default {
        plan.autonomy_default = v;
    }
    if let Some(v) = req.cache_policy {
        plan.cache_policy = v;
    }
    if let Some(v) = req.max_cascade_escalations {
        plan.max_cascade_escalations = v;
    }
    if let Some(v) = req.metadata {
        plan.metadata = v;
    }
    plan.updated_at = Utc::now();

    match state.store.plans.upsert(&plan).await {
        Ok(()) => (StatusCode::OK, Json(PlanResponse::from(plan))).into_response(),
        Err(StoreError::Conflict(msg)) => (StatusCode::CONFLICT, msg).into_response(),
        Err(e) => {
            tracing::error!(?e, "update_plan upsert failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `DELETE /admin/v1/tenants/:tenant_id/plans/:plan_id`.
///
/// Surfaces 409 Conflict when the plan is referenced by an assignment
/// (the FK on `plan_assignments.plan_id` is `ON DELETE RESTRICT`).
/// Admins must reassign before deleting.
pub async fn delete_plan(
    State(state): State<AdminState>,
    Path((tenant_id, plan_id)): Path<(TenantId, PlanId)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.plans.delete(tenant_id, plan_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(StoreError::Conflict(_)) => (
            StatusCode::CONFLICT,
            "plan is referenced by one or more assignments; reassign before deleting",
        )
            .into_response(),
        Err(StoreError::Database(_)) => {
            (StatusCode::NOT_FOUND, format!("plan {plan_id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "delete_plan failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `PATCH /admin/v1/tenants/:tenant_id/default-plan` — atomic default swap.
///
/// Single-statement update; the partial unique index
/// `plans_one_default_per_tenant` is respected by Postgres' end-of-statement
/// constraint check.
pub async fn set_default_plan(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,

    headers: HeaderMap,
    Json(req): Json<SetDefaultPlanRequest>,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.plans.set_default(tenant_id, req.plan_id).await {
        Ok(()) => (StatusCode::OK, format!("default set to {}", req.plan_id)).into_response(),
        Err(StoreError::Database(msg)) => {
            // Either no plans for tenant or the plan_id belongs to a
            // different tenant — both surface as 404 to the admin.
            (StatusCode::NOT_FOUND, msg).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "set_default_plan failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
