//! Deep health — `GET /admin/v1/health/deep`.
//!
//! Synchronously probes every dependency the gateway can reach. Intended
//! for operator tooling, not a per-request readiness probe (Kubernetes
//! continues to hit `/health`).

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;

use crate::state::AdminState;

/// Response body for the deep-health probe.
#[derive(Debug, Serialize)]
pub struct DeepHealth {
    /// Overall status — `"ok"` if everything probed succeeded, else
    /// `"degraded"`.
    pub status: &'static str,
    /// Per-dependency probe results.
    pub deps: Vec<Probe>,
}

/// One dependency's probe result.
#[derive(Debug, Serialize)]
pub struct Probe {
    /// Short name (`"postgres"`, `"redis"`, `"nats"`).
    pub name: &'static str,
    /// `"ok"` or `"error"`.
    pub status: &'static str,
    /// Latency in milliseconds (wall-clock).
    pub latency_ms: u128,
    /// Free-form detail — usually an error message on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `GET /admin/v1/health/deep` handler.
pub async fn deep_health(State(state): State<AdminState>) -> impl IntoResponse {
    let mut deps: Vec<Probe> = Vec::with_capacity(3);
    let mut all_ok = true;

    // Postgres probe: cheapest read — count rows on the thread table via
    // a well-known tenant (or a no-op query). We use `get_tenant` on a
    // nil ID and tolerate the `TenantNotFound` response as "DB reachable."
    let pg_start = std::time::Instant::now();
    let pg_result = state.store.tenants.get_tenant(elena_types::TenantId::default()).await;
    let pg_ok = matches!(pg_result, Ok(_) | Err(elena_types::StoreError::TenantNotFound(_)));
    deps.push(Probe {
        name: "postgres",
        status: if pg_ok { "ok" } else { "error" },
        latency_ms: pg_start.elapsed().as_millis(),
        detail: match pg_result {
            Err(ref e) if !pg_ok => Some(e.to_string()),
            _ => None,
        },
    });
    all_ok &= pg_ok;

    // Redis probe: claim_owner on a random key — cheap GET.
    let redis_start = std::time::Instant::now();
    let redis_result = state.store.cache.claim_owner(elena_types::ThreadId::default()).await;
    let redis_ok = redis_result.is_ok();
    deps.push(Probe {
        name: "redis",
        status: if redis_ok { "ok" } else { "error" },
        latency_ms: redis_start.elapsed().as_millis(),
        detail: redis_result.err().map(|e| e.to_string()),
    });
    all_ok &= redis_ok;

    // NATS probe: if we have a client, check its connection state.
    if let Some(nats) = &state.nats {
        let nats_start = std::time::Instant::now();
        let nats_state = nats.connection_state();
        let nats_ok = matches!(nats_state, async_nats::connection::State::Connected);
        deps.push(Probe {
            name: "nats",
            status: if nats_ok { "ok" } else { "error" },
            latency_ms: nats_start.elapsed().as_millis(),
            detail: if nats_ok { None } else { Some(format!("state: {nats_state:?}")) },
        });
        all_ok &= nats_ok;
    }

    let status_code = if all_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status_code, Json(DeepHealth { status: if all_ok { "ok" } else { "degraded" }, deps }))
        .into_response()
}
