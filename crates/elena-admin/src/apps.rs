//! App registry CRUD + onboarding.
//!
//! Apps (Solen, Hannlys, Omnii, ...) are an admin-only grouping above tenants.
//! The runtime never reads from this table; the admin panel uses these routes
//! to filter, list, and onboard tenants under a named product.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::Utc;
use elena_store::{TenantListFilter, TenantRecord};
use elena_types::{
    App, AppId, AppSlug, BudgetLimits, PermissionSet, Plan, PlanId, PlanSlug, StoreError, TenantId,
    TenantTier,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::AdminState;

/// Default page size for list endpoints when the client omits `limit`.
const DEFAULT_LIST_LIMIT: u32 = 50;
/// Hard cap on list page size to bound query cost.
const MAX_LIST_LIMIT: u32 = 200;

/// Request body for `POST /admin/v1/apps`.
#[derive(Debug, Deserialize)]
pub struct CreateAppRequest {
    /// Globally unique slug (lowercase kebab-case by convention).
    pub slug: AppSlug,
    /// Human-readable name shown in admin UIs.
    pub display_name: String,
    /// Optional plan blueprint applied to tenants onboarded under this app.
    #[serde(default)]
    pub default_plan_template: Option<Value>,
    /// Plugin allow-list seed for tenants onboarded under this app.
    #[serde(default)]
    pub default_allowed_plugin_ids: Vec<String>,
    /// Opaque metadata.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

/// Request body for `PATCH /admin/v1/apps/:id`.
#[derive(Debug, Deserialize, Default)]
pub struct UpdateAppRequest {
    /// Replacement display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Replacement plan template. Send `null` (i.e. `Some(None)`) to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    pub default_plan_template: Option<Option<Value>>,
    /// Replacement plugin allow-list seed.
    #[serde(default)]
    pub default_allowed_plugin_ids: Option<Vec<String>>,
    /// Replacement metadata map.
    #[serde(default)]
    pub metadata: Option<HashMap<String, Value>>,
}

/// Wire shape returned by every app read.
#[derive(Debug, Serialize)]
pub struct AppResponse {
    /// Stable identifier.
    pub id: AppId,
    /// Globally unique slug.
    pub slug: AppSlug,
    /// Display name.
    pub display_name: String,
    /// Plan template applied at onboarding time.
    pub default_plan_template: Option<Value>,
    /// Plugin allow-list seed.
    pub default_allowed_plugin_ids: Vec<String>,
    /// Opaque metadata.
    pub metadata: HashMap<String, Value>,
    /// Row creation time.
    pub created_at: chrono::DateTime<Utc>,
    /// Last update time.
    pub updated_at: chrono::DateTime<Utc>,
}

impl From<App> for AppResponse {
    fn from(app: App) -> Self {
        Self {
            id: app.id,
            slug: app.slug,
            display_name: app.display_name,
            default_plan_template: app.default_plan_template,
            default_allowed_plugin_ids: app.default_allowed_plugin_ids,
            metadata: app.metadata,
            created_at: app.created_at,
            updated_at: app.updated_at,
        }
    }
}

/// Pagination params for `GET /admin/v1/apps`.
#[derive(Debug, Deserialize, Default)]
pub struct ListAppsQuery {
    /// Page size (default 50, max 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Offset.
    #[serde(default)]
    pub offset: Option<u32>,
}

/// Pagination + filter params for `GET /admin/v1/apps/:id/tenants`.
#[derive(Debug, Deserialize, Default)]
pub struct ListAppTenantsQuery {
    /// Page size (default 50, max 200).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Offset.
    #[serde(default)]
    pub offset: Option<u32>,
    /// Optional case-sensitive name prefix.
    #[serde(default)]
    pub name: Option<String>,
    /// Include soft-deleted tenants. Default false.
    #[serde(default)]
    pub include_deleted: bool,
}

/// Request body for `POST /admin/v1/apps/:id/tenants`.
///
/// Atomic onboarding: creates a tenant under the app and seeds its default
/// plan from `app.default_plan_template`. Roll-back on any failure.
#[derive(Debug, Deserialize)]
pub struct OnboardTenantRequest {
    /// Stable tenant id (operators supply external IDs).
    pub id: TenantId,
    /// Human-readable name.
    pub name: String,
    /// Pricing tier. Defaults to Pro.
    #[serde(default)]
    pub tier: TenantTier,
    /// Optional budget override (otherwise template/tier default).
    #[serde(default)]
    pub budget: Option<BudgetLimits>,
    /// Optional permission-set override.
    #[serde(default)]
    pub permissions: Option<PermissionSet>,
    /// Opaque tenant metadata.
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
}

/// `POST /admin/v1/apps` — create an app.
pub async fn create_app(
    State(state): State<AdminState>,
    Json(req): Json<CreateAppRequest>,
) -> impl IntoResponse {
    let now = Utc::now();
    let app = App {
        id: AppId::new(),
        slug: req.slug,
        display_name: req.display_name,
        default_plan_template: req.default_plan_template,
        default_allowed_plugin_ids: req.default_allowed_plugin_ids,
        metadata: req.metadata,
        created_at: now,
        updated_at: now,
    };
    match state.store.apps.upsert(&app).await {
        Ok(()) => (StatusCode::CREATED, Json(AppResponse::from(app))).into_response(),
        Err(StoreError::Conflict(msg)) => (StatusCode::CONFLICT, msg).into_response(),
        Err(e) => {
            tracing::error!(?e, "create_app failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/apps` — list apps.
pub async fn list_apps(
    State(state): State<AdminState>,
    Query(q): Query<ListAppsQuery>,
) -> impl IntoResponse {
    let limit = clamp_limit(q.limit);
    let offset = q.offset.unwrap_or(0);
    match state.store.apps.list(limit, offset).await {
        Ok(apps) => {
            let body: Vec<AppResponse> = apps.into_iter().map(AppResponse::from).collect();
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, "list_apps failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/apps/:id` — fetch one app.
pub async fn get_app(
    State(state): State<AdminState>,
    Path(id): Path<AppId>,
) -> impl IntoResponse {
    match state.store.apps.get(id).await {
        Ok(Some(app)) => (StatusCode::OK, Json(AppResponse::from(app))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, format!("app {id} not found")).into_response(),
        Err(e) => {
            tracing::error!(?e, %id, "get_app failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `PATCH /admin/v1/apps/:id` — partial update.
pub async fn update_app(
    State(state): State<AdminState>,
    Path(id): Path<AppId>,
    Json(req): Json<UpdateAppRequest>,
) -> impl IntoResponse {
    let current = match state.store.apps.get(id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("app {id} not found")).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let updated = App {
        id: current.id,
        slug: current.slug,
        display_name: req.display_name.unwrap_or(current.display_name),
        default_plan_template: req.default_plan_template.unwrap_or(current.default_plan_template),
        default_allowed_plugin_ids: req
            .default_allowed_plugin_ids
            .unwrap_or(current.default_allowed_plugin_ids),
        metadata: req.metadata.unwrap_or(current.metadata),
        created_at: current.created_at,
        updated_at: Utc::now(),
    };

    match state.store.apps.upsert(&updated).await {
        Ok(()) => (StatusCode::OK, Json(AppResponse::from(updated))).into_response(),
        Err(StoreError::Conflict(msg)) => (StatusCode::CONFLICT, msg).into_response(),
        Err(e) => {
            tracing::error!(?e, %id, "update_app failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `DELETE /admin/v1/apps/:id` — delete an app. 409 when tenants reference it.
pub async fn delete_app(
    State(state): State<AdminState>,
    Path(id): Path<AppId>,
) -> impl IntoResponse {
    match state.store.apps.delete(id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(StoreError::Conflict(msg)) => (StatusCode::CONFLICT, msg).into_response(),
        Err(StoreError::Database(msg)) if msg.starts_with("no app ") => {
            (StatusCode::NOT_FOUND, format!("app {id} not found")).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %id, "delete_app failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/apps/:id/tenants` — list tenants under this app.
pub async fn list_app_tenants(
    State(state): State<AdminState>,
    Path(id): Path<AppId>,
    Query(q): Query<ListAppTenantsQuery>,
) -> impl IntoResponse {
    let filter = TenantListFilter {
        app_id: Some(id),
        name_prefix: q.name,
        limit: clamp_limit(q.limit),
        offset: q.offset.unwrap_or(0),
        include_deleted: q.include_deleted,
    };
    match state.store.tenants.list_all(&filter).await {
        Ok(tenants) => {
            let body: Vec<TenantSummary> = tenants.into_iter().map(TenantSummary::from).collect();
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::error!(?e, %id, "list_app_tenants failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `POST /admin/v1/apps/:id/tenants` — onboard a tenant under this app.
///
/// Atomic in spirit: we create the tenant first; if the default plan seed
/// then fails, we soft-delete the tenant to keep the system consistent
/// (a Postgres transaction would be ideal but the audit-write side path
/// prevents wrapping the full sequence in one). Operators retrying after
/// a 5xx will hit a 409 on the soft-deleted ID — they must use a fresh
/// id on retry.
pub async fn onboard_tenant(
    State(state): State<AdminState>,
    Path(app_id): Path<AppId>,
    Json(req): Json<OnboardTenantRequest>,
) -> impl IntoResponse {
    let app = match state.store.apps.get(app_id).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, format!("app {app_id} not found")).into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let now = Utc::now();
    let allowed_plugin_ids = if app.default_allowed_plugin_ids.is_empty() {
        Vec::new()
    } else {
        app.default_allowed_plugin_ids.clone()
    };

    let budget = req
        .budget
        .or_else(|| {
            app.default_plan_template
                .as_ref()
                .and_then(|tpl| tpl.get("budget"))
                .and_then(|b| serde_json::from_value::<BudgetLimits>(b.clone()).ok())
        })
        .unwrap_or(BudgetLimits::DEFAULT_PRO);

    let tenant = TenantRecord {
        id: req.id,
        name: req.name,
        tier: req.tier,
        budget,
        permissions: req.permissions.unwrap_or_default(),
        metadata: req.metadata,
        allowed_plugin_ids: allowed_plugin_ids.clone(),
        app_id: Some(app_id),
        deleted_at: None,
        created_at: now,
        updated_at: now,
    };

    if let Err(e) = state.store.tenants.upsert_tenant(&tenant).await {
        tracing::error!(?e, %app_id, tenant_id = %tenant.id, "onboarding tenant insert failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    if state.store.plans.default_for_tenant(tenant.id).await.is_err() {
        let plan = build_default_plan(&app, &tenant, now);
        if let Err(e) = state.store.plans.upsert(&plan).await {
            tracing::error!(?e, %app_id, tenant_id = %tenant.id, "onboarding plan seed failed");
            // Best-effort rollback so the next retry with a fresh id succeeds.
            if let Err(soft_err) = state.store.tenants.soft_delete(tenant.id).await {
                tracing::error!(?soft_err, "rollback soft_delete failed");
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("default plan seed failed; tenant rolled back: {e}"),
            )
                .into_response();
        }
    }

    (StatusCode::CREATED, Json(TenantSummary::from(tenant))).into_response()
}

fn build_default_plan(
    app: &App,
    tenant: &TenantRecord,
    now: chrono::DateTime<Utc>,
) -> Plan {
    let template = app.default_plan_template.as_ref();
    let slug = template
        .and_then(|t| t.get("slug").and_then(|v| v.as_str()))
        .map_or_else(|| tier_str(tenant.tier).to_owned(), str::to_owned);
    let display_name = template
        .and_then(|t| t.get("display_name").and_then(|v| v.as_str()))
        .map_or_else(|| slug.clone(), str::to_owned);
    let rate_limits = template
        .and_then(|t| t.get("rate_limits").cloned())
        .unwrap_or_else(|| serde_json::json!({}));
    let cache_policy = template
        .and_then(|t| t.get("cache_policy").cloned())
        .unwrap_or_else(|| serde_json::json!({}));
    let tier_models =
        template.and_then(|t| t.get("tier_models").cloned()).filter(|v| !v.is_null());
    let autonomy = template
        .and_then(|t| t.get("autonomy_default").and_then(|v| v.as_str()))
        .map(elena_types::AutonomyMode::from_str_or_default)
        .unwrap_or_default();
    let max_cascade_escalations = template
        .and_then(|t| t.get("max_cascade_escalations").and_then(serde_json::Value::as_u64))
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(1);
    let metadata = template
        .and_then(|t| t.get("metadata"))
        .and_then(|v| serde_json::from_value::<HashMap<String, Value>>(v.clone()).ok())
        .unwrap_or_default();

    Plan {
        id: PlanId::new(),
        tenant_id: tenant.id,
        slug: PlanSlug::new(slug),
        display_name,
        is_default: true,
        budget: tenant.budget,
        rate_limits,
        allowed_plugin_ids: tenant.allowed_plugin_ids.clone(),
        tier_models,
        autonomy_default: autonomy,
        cache_policy,
        max_cascade_escalations,
        metadata,
        created_at: now,
        updated_at: now,
    }
}

const fn tier_str(t: TenantTier) -> &'static str {
    match t {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Team => "team",
        TenantTier::Enterprise => "enterprise",
    }
}

fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT)
}

/// Lightweight tenant projection used by app + tenant list endpoints.
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
    pub deleted_at: Option<chrono::DateTime<Utc>>,
    /// Creation timestamp.
    pub created_at: chrono::DateTime<Utc>,
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

/// `Option<Option<T>>` deserializer used for fields that distinguish
/// "absent" (don't change) from "null" (clear) in PATCH bodies.
#[allow(clippy::option_option)]
fn deserialize_optional_field<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}
