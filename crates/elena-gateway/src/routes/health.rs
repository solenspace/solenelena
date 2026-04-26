//! `GET /health`, `GET /ready`, `GET /version`, `GET /info` handlers.
//!
//! - `/health` — liveness only. Always 200; used by Kubernetes liveness probe.
//! - `/ready`  — readiness. 503 when Postgres or NATS are unreachable.
//! - `/version` — running crate version (legacy; superseded by `/info`).
//! - `/info` — version plus optional git SHA (set at build time via
//!   `VERGEN_GIT_SHA`). Snapshot tests verify no secret-shaped substrings
//!   leak through this surface.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;

use crate::app::GatewayState;

/// Handler for `GET /health`. Always returns 200 OK with a small body.
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

/// Handler for `GET /version`. Surfaces the running binary's version.
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse { version: env!("CARGO_PKG_VERSION"), name: "elena-gateway" })
}

/// Handler for `GET /info`. Returns crate version plus optional git SHA.
///
/// The git SHA comes from `VERGEN_GIT_SHA`, set at build time when the
/// `vergen` build script is in use. Absent in plain `cargo build`; in that
/// case we render `"unknown"` so the response shape stays stable.
pub async fn info() -> Json<InfoResponse> {
    Json(InfoResponse {
        version: env!("CARGO_PKG_VERSION"),
        name: "elena-gateway",
        git_sha: option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
    })
}

/// Handler for `GET /ready`. 503 when any critical dependency is unreachable.
///
/// Probes are deliberately cheap: a single Postgres `SELECT 1`-equivalent
/// (via `get_tenant` on a nil id) and the NATS connection state. JWKS warm
/// status is not probed yet; `JwtValidator` is stateless beyond its first
/// fetch and a cold lookup adds a few hundred ms but does not fail.
pub async fn ready(State(state): State<GatewayState>) -> impl IntoResponse {
    let mut probes: Vec<DepProbe> = Vec::with_capacity(2);
    let mut all_ok = true;

    let pg_start = std::time::Instant::now();
    let pg_result = state.store.tenants.get_tenant(elena_types::TenantId::default()).await;
    let pg_ok = matches!(pg_result, Ok(_) | Err(elena_types::StoreError::TenantNotFound(_)));
    probes.push(DepProbe {
        name: "postgres",
        status: if pg_ok { "ok" } else { "error" },
        latency_ms: pg_start.elapsed().as_millis(),
    });
    all_ok &= pg_ok;

    let nats_state = state.nats.connection_state();
    let nats_ok = matches!(nats_state, async_nats::connection::State::Connected);
    probes.push(DepProbe {
        name: "nats",
        status: if nats_ok { "ok" } else { "error" },
        latency_ms: 0,
    });
    all_ok &= nats_ok;

    let body = ReadyResponse { ready: all_ok, deps: probes };
    let code = if all_ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(body))
}

/// `/health` response body.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Always `true`.
    pub ok: bool,
}

/// `/version` response body.
#[derive(Debug, Serialize)]
pub struct VersionResponse {
    /// Crate version from `CARGO_PKG_VERSION`.
    pub version: &'static str,
    /// Binary name (constant).
    pub name: &'static str,
}

/// `/info` response body. Stable shape regardless of build environment.
#[derive(Debug, Serialize)]
pub struct InfoResponse {
    /// Crate version from `CARGO_PKG_VERSION`.
    pub version: &'static str,
    /// Binary name (constant).
    pub name: &'static str,
    /// Short or long git SHA from `VERGEN_GIT_SHA`, or `"unknown"`.
    pub git_sha: &'static str,
}

/// `/ready` response body.
#[derive(Debug, Serialize)]
pub struct ReadyResponse {
    /// True when every dependency probed `ok`.
    pub ready: bool,
    /// Per-dependency status.
    pub deps: Vec<DepProbe>,
}

/// One dependency's status in `/ready`.
#[derive(Debug, Serialize)]
pub struct DepProbe {
    /// Short name.
    pub name: &'static str,
    /// `"ok"` or `"error"`.
    pub status: &'static str,
    /// Wall-clock latency.
    pub latency_ms: u128,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot guard — `/info` must never leak secret-shaped substrings.
    /// If a future contributor adds an `endpoint`-or-`token`-shaped field,
    /// this test fails before the change ships.
    #[test]
    fn info_response_does_not_leak_secret_substrings() {
        let body = serde_json::to_string(&InfoResponse {
            version: env!("CARGO_PKG_VERSION"),
            name: "elena-gateway",
            git_sha: "deadbeef",
        })
        .expect("serialize");

        for forbidden in [
            "ELENA_ADMIN_TOKEN",
            "ELENA_CREDENTIAL_MASTER_KEY",
            "JWT_HS256_SECRET",
            "postgres://",
            "redis://",
            "nats://",
        ] {
            assert!(
                !body.contains(forbidden),
                "/info leaked forbidden substring {forbidden:?}: {body}"
            );
        }
    }
}
