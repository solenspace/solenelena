//! Budget + usage observability — `/admin/v1/tenants/:id/budget-state` and
//! `/admin/v1/apps/:id/usage-summary`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use elena_types::{AppId, TenantId};
use serde::{Deserialize, Serialize};

use crate::auth::require_tenant_scope;
use crate::state::AdminState;

/// Wire shape for `GET /admin/v1/tenants/:id/budget-state`.
#[derive(Debug, Serialize)]
pub struct BudgetStateResponse {
    /// Tenant the state belongs to.
    pub tenant_id: TenantId,
    /// Tokens spent today (rolls over at `day_rollover_at`).
    pub tokens_used_today: u64,
    /// When the daily counter next resets.
    pub day_rollover_at: DateTime<Utc>,
    /// Threads currently active for this tenant.
    pub threads_active: u32,
}

/// Query for `GET /admin/v1/apps/:id/usage-summary`.
#[derive(Debug, Deserialize, Default)]
pub struct UsageSummaryQuery {
    /// Inclusive lower bound on `messages.created_at`.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `messages.created_at`.
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
}

/// Wire shape for `GET /admin/v1/apps/:id/usage-summary`.
#[derive(Debug, Serialize)]
pub struct UsageSummaryResponse {
    /// App the summary belongs to.
    pub app_id: AppId,
    /// Total tokens (summed `messages.token_count`).
    pub tokens_total: u64,
    /// Distinct tenants contributing to the total.
    pub tenant_count: u32,
    /// Distinct threads contributing to the total.
    pub thread_count: u32,
    /// Echo of the requested window for client-side caching keys.
    pub since: Option<DateTime<Utc>>,
    /// Echo of the requested window.
    pub until: Option<DateTime<Utc>>,
}

/// `GET /admin/v1/tenants/:id/budget-state`.
pub async fn get_budget_state(
    State(state): State<AdminState>,
    Path(tenant_id): Path<TenantId>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(s) = require_tenant_scope(&state.store, tenant_id, &headers).await {
        return s.into_response();
    }
    match state.store.tenants.get_budget_state(tenant_id).await {
        Ok(s) => (
            StatusCode::OK,
            Json(BudgetStateResponse {
                tenant_id,
                tokens_used_today: s.tokens_used_today,
                day_rollover_at: s.day_rollover_at,
                threads_active: s.threads_active,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(?e, %tenant_id, "get_budget_state failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `GET /admin/v1/apps/:id/usage-summary?since=&until=`.
pub async fn get_usage_summary(
    State(state): State<AdminState>,
    Path(app_id): Path<AppId>,
    Query(q): Query<UsageSummaryQuery>,
) -> impl IntoResponse {
    match state.store.apps.usage_summary(app_id, q.since, q.until).await {
        Ok(summary) => (
            StatusCode::OK,
            Json(UsageSummaryResponse {
                app_id,
                tokens_total: summary.tokens_total,
                tenant_count: summary.tenant_count,
                thread_count: summary.thread_count,
                since: q.since,
                until: q.until,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(?e, %app_id, "get_usage_summary failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
