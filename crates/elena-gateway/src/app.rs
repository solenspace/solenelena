//! axum `Router` assembly.

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::routing::{get, post};
use elena_observability::LoopMetrics;
use elena_store::Store;

use crate::auth::JwtValidator;
use crate::config::GatewayConfig;
use crate::error::GatewayError;
use crate::routes::{approvals, health, metrics, threads, ws};

/// State held by the gateway router.
///
/// Cheap to clone — holds `Arc`-shaped handles.
#[derive(Clone)]
pub struct GatewayState {
    /// JWT validator.
    pub jwt: JwtValidator,
    /// Shared persistence handles.
    pub store: Arc<Store>,
    /// NATS core client (event subscribers + abort publishes).
    pub nats: async_nats::Client,
    /// `JetStream` context (work-queue publishes).
    pub jet: async_nats::jetstream::Context,
    /// Process-wide metrics handles. Shared with the worker via
    /// `LoopDeps.metrics` so `/metrics` renders one consolidated surface.
    pub metrics: Arc<LoopMetrics>,
}

impl std::fmt::Debug for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayState").field("jwt", &self.jwt).finish_non_exhaustive()
    }
}

impl GatewayState {
    /// Connect every dependency the gateway needs, including the NATS
    /// `JetStream` stream backing `elena.work.incoming`.
    pub async fn connect(
        cfg: &GatewayConfig,
        store: Arc<Store>,
        metrics: Arc<LoopMetrics>,
    ) -> Result<Self, GatewayError> {
        let jwt = JwtValidator::from_config(&cfg.jwt)?;
        let (nats, jet) = crate::nats::connect(&cfg.nats_url).await?;
        Ok(Self { jwt, store, nats, jet, metrics })
    }
}

impl FromRef<GatewayState> for JwtValidator {
    fn from_ref(state: &GatewayState) -> Self {
        state.jwt.clone()
    }
}

impl FromRef<GatewayState> for Arc<LoopMetrics> {
    fn from_ref(state: &GatewayState) -> Self {
        Arc::clone(&state.metrics)
    }
}

/// Build the axum router wired with the public routes and the state.
pub fn build_router(state: GatewayState) -> Router {
    let admin_state =
        elena_admin::AdminState::new(Arc::clone(&state.store), Some(state.nats.clone()));
    let admin = elena_admin::admin_router(admin_state);

    let core = Router::new()
        .route("/health", get(health::health))
        .route("/version", get(health::version))
        .route("/metrics", get(metrics::metrics))
        .route("/v1/threads", post(threads::create_thread))
        .route("/v1/threads/{thread_id}/stream", get(ws::ws_upgrade))
        .route("/v1/threads/{thread_id}/approvals", post(approvals::post_approvals))
        .with_state(state);

    core.nest("/admin/v1", admin)
}
