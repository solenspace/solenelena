//! `GET /metrics` — Prometheus text endpoint.
//!
//! Phase-7 opens this route publicly (no auth) for scrape by the cluster's
//! Prometheus instance. Operator wraps the gateway behind an ingress that
//! restricts `/metrics` to the monitoring namespace in production — Elena
//! itself does not gate the endpoint (standard pattern for Prom exposition).
//!
//! Body is `text/plain; version=0.0.4; charset=utf-8` exactly as Prometheus
//! expects.

use std::sync::Arc;

use axum::{extract::State, http::HeaderValue, http::header::CONTENT_TYPE, response::IntoResponse};
use elena_observability::LoopMetrics;

/// Handler for `GET /metrics`.
pub async fn metrics(State(metrics): State<Arc<LoopMetrics>>) -> impl IntoResponse {
    match metrics.render_text() {
        Ok(body) => (
            [(CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"))],
            body,
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("prometheus encode failed: {e}"),
        )
            .into_response(),
    }
}
