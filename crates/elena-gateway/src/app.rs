//! axum `Router` assembly.

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRef;
use axum::http::HeaderValue;
use axum::http::Method;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::routing::{get, post};
use elena_observability::LoopMetrics;
use elena_plugins::PluginRegistry;
use elena_store::Store;
use secrecy::SecretString;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::auth::JwtValidator;
use crate::config::{CorsConfig, GatewayConfig};
use crate::error::GatewayError;
use crate::fanout::{self, FanoutTable};
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
    /// Optional shared secret enforced as `X-Elena-Admin-Token` on
    /// every `/admin/v1/*` call. `None` disables the check (tests +
    /// smokes); production sets it from `ELENA_ADMIN_TOKEN`.
    pub admin_token: Option<SecretString>,
    /// Optional plugin registry handed off to the admin router so
    /// `GET /admin/v1/plugins` reflects the live registration set. The
    /// boot path in `elena-server` calls `with_plugins(...)` after
    /// constructing the registry.
    pub plugins: Option<Arc<PluginRegistry>>,
    /// S6 — CORS allow-list (empty = no CORS layer mounted).
    pub cors: CorsConfig,
    /// X1 — Gateway-local fanout table. Per-thread broadcast channels
    /// fed by a single wildcard NATS subscription. WS handlers
    /// subscribe to the local channel rather than NATS directly so
    /// NATS interest scales with pod count, not client count.
    pub fanout: FanoutTable,
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
        // S6 — guard against the "*" wildcard escape hatch up front.
        // Browsers paired with `Access-Control-Allow-Origin: *` won't
        // send credentials anyway, but the wildcard makes it too easy
        // to accidentally trust an unintended origin; force the
        // operator to enumerate.
        for origin in &cfg.cors.allow_origins {
            if origin.trim() == "*" {
                return Err(GatewayError::Internal(
                    "gateway.cors.allow_origins must not contain \"*\"; enumerate origins"
                        .to_owned(),
                ));
            }
        }
        let fanout = fanout::new_fanout_table();
        // X1 — start the single wildcard subscriber that fans every
        // per-thread event into the local broadcast table. Idempotent:
        // tasks spawned here run for the process lifetime; restarting
        // gateway state would orphan a previous pump task.
        fanout::spawn_fanout_pump(nats.clone(), fanout.clone());
        Ok(Self {
            jwt,
            store,
            nats,
            jet,
            metrics,
            admin_token: None,
            plugins: None,
            cors: cfg.cors.clone(),
            fanout,
        })
    }

    /// Attach the admin-API shared secret. The boot path in
    /// `elena-server` calls this once after `connect` if
    /// `ELENA_ADMIN_TOKEN` is present in env.
    #[must_use]
    pub fn with_admin_token(mut self, token: SecretString) -> Self {
        self.admin_token = Some(token);
        self
    }

    /// Attach the plugin registry so the admin router can answer
    /// `GET /admin/v1/plugins`.
    #[must_use]
    pub fn with_plugins(mut self, plugins: Arc<PluginRegistry>) -> Self {
        self.plugins = Some(plugins);
        self
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

impl FromRef<GatewayState> for Arc<Store> {
    fn from_ref(state: &GatewayState) -> Self {
        Arc::clone(&state.store)
    }
}

/// Build the axum router wired with the public routes and the state.
pub fn build_router(state: GatewayState) -> Router {
    let mut admin_state =
        elena_admin::AdminState::new(Arc::clone(&state.store), Some(state.nats.clone()));
    if let Some(token) = state.admin_token.clone() {
        admin_state = admin_state.with_admin_token(token);
    }
    if let Some(plugins) = state.plugins.clone() {
        admin_state = admin_state.with_plugins(plugins);
    }
    let admin = elena_admin::admin_router(admin_state);

    // S6 — assemble a CORS layer from the operator's allow-list. Empty
    // list = no layer mounted (preserves pre-S6 behaviour for headless
    // BFF deployments). Credentials are always disabled — Elena's
    // browser auth path is the JWT in `Authorization`, not cookies, so
    // there's no need to opt into the credentialed-CORS posture.
    let cors_layer = build_cors_layer(&state.cors);

    let core = Router::new()
        .route("/health", get(health::health))
        .route("/ready", get(health::ready))
        .route("/version", get(health::version))
        .route("/info", get(health::info))
        .route("/metrics", get(metrics::metrics))
        .route("/v1/threads", post(threads::create_thread))
        .route("/v1/threads/{thread_id}/stream", get(ws::ws_upgrade))
        .route("/v1/threads/{thread_id}/approvals", post(approvals::post_approvals))
        .with_state(state);

    let mut router = core.nest("/admin/v1", admin);
    if let Some(layer) = cors_layer {
        router = router.layer(layer);
    }
    router
}

fn build_cors_layer(cfg: &CorsConfig) -> Option<CorsLayer> {
    if cfg.allow_origins.is_empty() {
        return None;
    }
    let origins: Vec<HeaderValue> =
        cfg.allow_origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()).collect();
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
            .allow_headers([AUTHORIZATION, CONTENT_TYPE])
            .allow_credentials(false),
    )
}
