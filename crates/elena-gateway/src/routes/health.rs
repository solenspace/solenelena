//! `GET /health` and `GET /version` handlers.

use axum::Json;
use serde::Serialize;

/// Handler for `GET /health`. Always returns 200 OK with a small body.
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

/// Handler for `GET /version`. Surfaces the running binary's version.
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse { version: env!("CARGO_PKG_VERSION"), name: "elena-gateway" })
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
