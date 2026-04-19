//! `elena-server` — unified gateway + worker process.
//!
//! Boots the same components as the dedicated gateway and worker
//! binaries, but inside a single tokio runtime. Targets one-process
//! deploy environments like Railway (which expects exactly one
//! container per service) and local docker-compose smoke runs.
//!
//! Configuration is the same as the rest of Elena: figment loads
//! `/etc/elena/elena.toml` (if present), `$ELENA_CONFIG_FILE` (if
//! set), and `ELENA_*` env vars layered last. The boot fails loudly
//! if any required setting is missing — silent fallbacks make
//! production debugging impossible.
//!
//! Behaviour summary:
//!
//! - HTTP + WebSocket gateway listens on `ELENA_LISTEN_ADDR`
//!   (default `0.0.0.0:8080`).
//! - Worker subscribes to NATS JetStream `elena.work.incoming`.
//! - Both share one `LoopDeps`, one `Store`, one `LoopMetrics`.
//! - `SIGTERM`/`SIGINT` triggers graceful shutdown — gateway stops
//!   accepting new sockets and worker drains in-flight loops.

#![allow(
    clippy::print_stderr,
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::if_not_else
)]

use std::{net::SocketAddr, sync::Arc};

use elena_config::{ElenaConfig, TierEntry, TierModels};
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_core::LoopDeps;
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{
    AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy, LlmClient, LlmMultiplexer,
    OpenAiCompatClient, OpenAiCompatConfig,
};
use elena_memory::EpisodicMemory;
use elena_observability::LoopMetrics;
use elena_plugins::{PluginRegistry, PluginsConfig};
use elena_router::ModelRouter;
use elena_store::{DropCallback, Store};
use elena_tools::ToolRegistry;
use elena_types::{ModelId, TenantTier};
use elena_worker::{WorkerConfig, run_worker};
use secrecy::SecretString;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = elena_config::load().map_err(|e| anyhow::anyhow!("load config: {e}"))?;
    elena_config::validate(&cfg).map_err(|e| anyhow::anyhow!("validate config: {e}"))?;

    let metrics = Arc::new(LoopMetrics::new()?);
    let drops_metric = metrics.audit_drops_total.clone();
    let on_drop: DropCallback = Arc::new(move |n| {
        drops_metric.inc_by(n);
    });

    let store = Arc::new(Store::connect_with_audit_drops(&cfg, on_drop).await?);
    store.run_migrations().await?;
    info!("migrations applied");

    let (mux, tier_routing) = build_llm(&cfg)?;
    let llm: Arc<dyn LlmClient> = Arc::new(mux);

    let tools = ToolRegistry::new();
    let plugins_endpoints = std::env::var("ELENA_PLUGIN_ENDPOINTS")
        .ok()
        .map(|csv| {
            csv.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let plugins_cfg = PluginsConfig { endpoints: plugins_endpoints, ..PluginsConfig::default() };
    let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
    if !plugins_cfg.endpoints.is_empty() {
        plugins.register_all(&plugins_cfg).await?;
        info!(count = plugins.manifests().len(), "plugins registered");
    }

    let context =
        Arc::new(ContextManager::new(Arc::new(NullEmbedder), ContextManagerOptions::default()));
    let memory = Arc::new(EpisodicMemory::new(Arc::new(store.episodes.clone())));
    let router = Arc::new(ModelRouter::new(tier_routing, 2));

    let deps = Arc::new(LoopDeps {
        store: store.clone(),
        llm,
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools,
        context,
        memory,
        router,
        plugins: Arc::clone(&plugins),
        rate_limits: Arc::new(cfg.rate_limits.clone()),
        metrics: Arc::clone(&metrics),
        defaults: Arc::new(cfg.defaults.clone()),
    });

    let listen_addr: SocketAddr = std::env::var("ELENA_LISTEN_ADDR")
        .as_deref()
        .unwrap_or("0.0.0.0:8080")
        .parse()
        .map_err(|e| anyhow::anyhow!("ELENA_LISTEN_ADDR is not a valid socket address: {e}"))?;
    let nats_url = std::env::var("ELENA_NATS_URL")
        .or_else(|_| std::env::var("NATS_URL"))
        .map_err(|_| anyhow::anyhow!("ELENA_NATS_URL or NATS_URL is required"))?;

    let listener = TcpListener::bind(listen_addr).await?;
    let bound: SocketAddr = listener.local_addr()?;
    info!(%bound, "gateway bound");

    let jwt_secret = std::env::var("ELENA_JWT_SECRET")
        .or_else(|_| std::env::var("JWT_HS256_SECRET"))
        .map_err(|_| anyhow::anyhow!("ELENA_JWT_SECRET or JWT_HS256_SECRET is required"))?;
    let gateway_cfg = GatewayConfig {
        listen_addr: bound,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: std::env::var("ELENA_JWT_ISSUER").unwrap_or_else(|_| "elena".into()),
            audience: std::env::var("ELENA_JWT_AUDIENCE")
                .unwrap_or_else(|_| "elena-clients".into()),
            leeway_seconds: 60,
        },
    };
    let gateway_state =
        GatewayState::connect(&gateway_cfg, store.clone(), Arc::clone(&metrics)).await?;
    let app = build_router(gateway_state);

    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());

    let worker_cancel = cancel.clone();
    let worker_deps = Arc::clone(&deps);
    let worker_cfg = WorkerConfig {
        nats_url,
        worker_id: std::env::var("ELENA_WORKER_ID").unwrap_or_else(|_| "elena-server".into()),
        max_concurrent_loops: std::env::var("ELENA_MAX_CONCURRENT_LOOPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8),
        durable_name: std::env::var("ELENA_DURABLE_NAME").ok(),
        stream_name: std::env::var("ELENA_STREAM_NAME").ok(),
    };
    let worker_handle = tokio::spawn(async move {
        if let Err(e) = run_worker(worker_cfg, worker_deps, worker_cancel).await {
            warn!(?e, "worker exited with error");
        }
    });

    let serve_cancel = cancel.clone();
    let serve_handle = tokio::spawn(async move {
        if let Err(e) =
            axum::serve(listener, app).with_graceful_shutdown(serve_cancel.cancelled_owned()).await
        {
            warn!(?e, "gateway exited with error");
        }
    });

    let _ = tokio::join!(worker_handle, serve_handle);
    info!("elena-server shutdown complete");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,elena_worker=info,elena_gateway=info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
}

fn build_llm(cfg: &ElenaConfig) -> anyhow::Result<(LlmMultiplexer, TierModels)> {
    // The multiplexer's default lookup name is the operator's
    // `providers.default`, so a thread without an explicit `provider`
    // tag in its `LlmRequest` falls back to it.
    let mut mux = LlmMultiplexer::new(&cfg.providers.default);

    if let Some(a) = &cfg.providers.anthropic {
        let client = Arc::new(AnthropicClient::new(a, AnthropicAuth::ApiKey(a.api_key.clone()))?);
        mux.register("anthropic", client as Arc<dyn LlmClient>);
    } else if let Some(a) = &cfg.anthropic {
        // Backwards compat with the legacy single-provider env
        // (`ELENA_ANTHROPIC__*`). New deployments should configure under
        // `[providers.anthropic]` instead.
        let client = Arc::new(AnthropicClient::new(a, AnthropicAuth::ApiKey(a.api_key.clone()))?);
        mux.register("anthropic", client as Arc<dyn LlmClient>);
    }

    if let Some(g) = &cfg.providers.groq {
        let oa = OpenAiCompatConfig {
            api_key: g.api_key.clone(),
            base_url: g.base_url.clone(),
            request_timeout_ms: g.request_timeout_ms,
            connect_timeout_ms: g.connect_timeout_ms,
            max_attempts: g.max_attempts,
        };
        mux.register("groq", Arc::new(OpenAiCompatClient::new("groq", &oa)?) as Arc<dyn LlmClient>);
    }
    if let Some(o) = &cfg.providers.openrouter {
        let oa = OpenAiCompatConfig {
            api_key: o.api_key.clone(),
            base_url: o.base_url.clone(),
            request_timeout_ms: o.request_timeout_ms,
            connect_timeout_ms: o.connect_timeout_ms,
            max_attempts: o.max_attempts,
        };
        mux.register(
            "openrouter",
            Arc::new(OpenAiCompatClient::new("openrouter", &oa)?) as Arc<dyn LlmClient>,
        );
    }

    let tiers = if !cfg.defaults.tier_models.fast.model.as_str().is_empty() {
        cfg.defaults.tier_models.clone()
    } else {
        let provider = cfg.providers.default.clone();
        let model = std::env::var("ELENA_DEFAULT_MODEL").map_err(|_| {
            anyhow::anyhow!("ELENA_DEFAULT_MODEL is required when [defaults.tiers] is empty")
        })?;
        TierModels {
            fast: TierEntry { provider: provider.clone(), model: ModelId::new(&model) },
            standard: TierEntry { provider: provider.clone(), model: ModelId::new(&model) },
            premium: TierEntry { provider, model: ModelId::new(&model) },
        }
    };

    Ok((mux, tiers))
}

fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    warn!(?e, "failed to install SIGTERM handler");
                    return;
                }
            };
        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            _ = tokio::signal::ctrl_c() => info!("received SIGINT"),
        }
        cancel.cancel();
    });
}
