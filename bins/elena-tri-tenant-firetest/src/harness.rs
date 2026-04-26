//! In-process Elena harness for the fire-test.
//!
//! Boots Postgres + Redis + NATS via testcontainers, a wiremock LLM
//! that returns deterministic Anthropic-shaped SSE per turn, two
//! embedded plugins (echo and video_mock), then an axum gateway and a
//! NATS-consuming worker. Lifted from `bins/elena-hannlys-smoke` —
//! same boot order, same shutdown order, same JWT minting.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RateLimitsConfig, RedisConfig, RouterConfig, TierEntry, TierModels,
};
use elena_context::{ContextManager, ContextManagerOptions, EpisodicMemory, NullEmbedder};
use elena_gateway::{
    CorsConfig, GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router,
};
use elena_llm::{
    AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy, LlmClient, LlmMultiplexer,
};
use elena_plugins::{PluginRegistry, PluginsConfig};
use elena_router::ModelRouter;
use elena_store::Store;
use elena_tools::ToolRegistry;
use elena_types::{ModelId, TenantTier};
use elena_worker::{WorkerConfig, run_worker};
use secrecy::SecretString;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::video_mock::VideoMockConnector;

pub const JWT_SECRET: &str = "elena-firetest-secret-do-not-use-in-prod";
pub const JWT_ISSUER: &str = "elena-firetest";
pub const JWT_AUDIENCE: &str = "elena-firetest-clients";
pub const MODEL_ID: &str = "claude-haiku-4-5-20251001";

/// Live state of the in-process Elena. Drop drains containers + tasks.
pub struct Harness {
    pub listen_addr: SocketAddr,
    pub store: Arc<Store>,
    pub cancel: CancellationToken,
    pub gw_handle: Option<JoinHandle<()>>,
    pub wk_handle: Option<JoinHandle<()>>,
    /// Kept alive for the duration of the test so its TCP listener stays bound.
    _llm_mock: MockServer,
    _pg: ContainerAsync<GenericImage>,
    _redis: ContainerAsync<GenericImage>,
    _nats: ContainerAsync<GenericImage>,
}

impl Harness {
    pub async fn start() -> Result<Self> {
        // ---- testcontainers ----
        let pg = GenericImage::new("pgvector/pgvector", "pg16")
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "elena")
            .with_env_var("POSTGRES_USER", "elena")
            .with_env_var("POSTGRES_DB", "elena")
            .start()
            .await?;
        let pg_port = pg.get_host_port_ipv4(5432).await?;

        let redis = GenericImage::new("redis", "7-alpine")
            .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
            .start()
            .await?;
        let redis_port = redis.get_host_port_ipv4(6379).await?;

        let nats = GenericImage::new("nats", "2.10-alpine")
            .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
            .with_cmd(["-js"])
            .start()
            .await?;
        let nats_port = nats.get_host_port_ipv4(4222).await?;
        let nats_url = format!("nats://127.0.0.1:{nats_port}");

        // ---- wiremock LLM (Anthropic-shaped SSE) ----
        let llm_mock = setup_llm_mock().await;

        // ---- elena config + store ----
        let cfg = ElenaConfig {
            postgres: PostgresConfig {
                url: SecretString::from(format!(
                    "postgres://elena:elena@127.0.0.1:{pg_port}/elena"
                )),
                pool_max: 16,
                pool_min: 4,
                connect_timeout_ms: 10_000,
                // Bumped from prod default — the cascading tenant
                // delete at the end of the run touches ~25 k rows
                // across 11 tables and needs more headroom. Raise
                // production accordingly when a tenant has a lot of
                // history.
                statement_timeout_ms: 180_000,
            },
            redis: RedisConfig {
                url: SecretString::from(format!("redis://127.0.0.1:{redis_port}")),
                pool_max: 16,
                thread_claim_ttl_ms: 60_000,
            },
            anthropic: None,
            cache: CacheConfig::default(),
            logging: LoggingConfig { level: "warn".into(), format: LogFormat::Pretty },
            defaults: DefaultsConfig::default(),
            context: ContextConfig::default(),
            router: RouterConfig::default(),
            providers: elena_config::ProvidersConfig::default(),
            rate_limits: RateLimitsConfig::default(),
        };
        let store = Arc::new(Store::connect(&cfg).await?);
        store.run_migrations().await?;

        // ---- LLM (wiremock'd Anthropic) ----
        let anthropic_cfg = elena_config::AnthropicConfig {
            api_key: SecretString::from("sk-mock"),
            base_url: llm_mock.uri(),
            api_version: "2023-06-01".to_owned(),
            request_timeout_ms: Some(30_000),
            connect_timeout_ms: 10_000,
            max_attempts: 3,
            pool_max_idle_per_host: 64,
        };
        let anthropic = Arc::new(AnthropicClient::new(
            &anthropic_cfg,
            AnthropicAuth::ApiKey(SecretString::from("sk-mock")),
        )?);
        let mut mux = LlmMultiplexer::new("anthropic");
        mux.register("anthropic", anthropic as Arc<dyn LlmClient>);
        let llm: Arc<dyn LlmClient> = Arc::new(mux);

        let model = ModelId::new(MODEL_ID);
        let tier_routing = TierModels {
            fast: TierEntry {
                provider: "anthropic".into(),
                model: model.clone(),
                max_output_tokens: None,
            },
            standard: TierEntry {
                provider: "anthropic".into(),
                model: model.clone(),
                max_output_tokens: None,
            },
            premium: TierEntry { provider: "anthropic".into(), model, max_output_tokens: None },
        };

        // ---- Plugins (embedded only — echo + video_mock) ----
        let tools = ToolRegistry::new();
        let plugins_cfg = PluginsConfig::default();
        let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
        plugins.register_embedded(Arc::new(Arc::new(elena_connector_echo::EchoConnector))).await?;
        plugins.register_embedded(Arc::new(Arc::new(VideoMockConnector))).await?;

        // ---- LoopDeps ----
        let context =
            Arc::new(ContextManager::new(Arc::new(NullEmbedder), ContextManagerOptions::default()));
        let memory = Arc::new(EpisodicMemory::new(Arc::new(store.episodes.clone())));
        let router = Arc::new(ModelRouter::new(tier_routing, 1));
        let metrics = Arc::new(elena_observability::LoopMetrics::new()?);
        let deps = Arc::new(elena_core::LoopDeps {
            store: store.clone(),
            llm,
            cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
            tools,
            context,
            memory,
            router,
            plugins: Arc::clone(&plugins),
            rate_limits: Arc::new(RateLimitsConfig::default()),
            metrics: Arc::clone(&metrics),
            defaults: Arc::new(DefaultsConfig::default()),
        });

        // ---- Gateway ----
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let listen_addr: SocketAddr = listener.local_addr()?;
        let gateway_cfg = GatewayConfig {
            listen_addr,
            nats_url: nats_url.clone(),
            jwt: JwtConfig {
                algorithm: JwtAlgorithm::HS256,
                secret_or_public_key: SecretString::from(JWT_SECRET),
                issuer: JWT_ISSUER.into(),
                audience: JWT_AUDIENCE.into(),
                leeway_seconds: 60,
            },
            cors: CorsConfig::default(),
        };
        let gateway_state =
            GatewayState::connect(&gateway_cfg, store.clone(), Arc::clone(&metrics)).await?;
        // Attach the plugin registry so /admin/v1/plugins works for assertions.
        let gateway_state = gateway_state.with_plugins(Arc::clone(&plugins));
        let router_app = build_router(gateway_state);
        let cancel = CancellationToken::new();
        let gw_cancel = cancel.clone();
        let gw_handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router_app)
                .with_graceful_shutdown(gw_cancel.cancelled_owned())
                .await
            {
                warn!(?e, "gateway server error");
            }
        });

        let worker_cfg = WorkerConfig {
            nats_url,
            worker_id: "firetest-worker".into(),
            max_concurrent_loops: 32,
            durable_name: None,
            stream_name: None,
        };
        let wk_cancel = cancel.clone();
        let wk_deps = Arc::clone(&deps);
        let wk_handle = tokio::spawn(async move {
            if let Err(e) = run_worker(worker_cfg, wk_deps, wk_cancel).await {
                warn!(?e, "worker error");
            }
        });

        // Give the worker a moment to subscribe before drivers fire.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        Ok(Self {
            listen_addr,
            store,
            cancel,
            gw_handle: Some(gw_handle),
            wk_handle: Some(wk_handle),
            _llm_mock: llm_mock,
            _pg: pg,
            _redis: redis,
            _nats: nats,
        })
    }

    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(h) = self.gw_handle.take() {
            let _ = h.await;
        }
        if let Some(h) = self.wk_handle.take() {
            let _ = h.await;
        }
        // testcontainers drop on Self drop.
    }

    /// Convenience: an HTTP client tuned for the harness.
    pub fn http(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client builds")
    }

    /// `http://<addr>` — base URL for /admin/v1/* and /v1/* calls.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.listen_addr)
    }

    /// `ws://<addr>` — base for WebSocket upgrades.
    pub fn ws_base_url(&self) -> String {
        format!("ws://{}", self.listen_addr)
    }
}

/// Wiremock LLM that always replies with `tool_use` → text on the
/// first message, then text-only on subsequent messages. The sequence
/// repeats deterministically so concurrent turns don't fight over a
/// shared mock-state counter.
///
/// One mock route handles every `POST /v1/messages` and returns SSE
/// containing a `tool_use` block for `echo_reverse` plus a short
/// `text_delta`. The loop will dispatch the tool, then re-stream and
/// (since the same mock returns) get `tool_use` again — that would
/// loop forever, so the response varies by request body presence of
/// any `tool_result` content block: if seen, return text-only.
async fn setup_llm_mock() -> MockServer {
    let server = MockServer::start().await;
    let usage = r#"{"input_tokens":4,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}"#;

    // Fresh-turn tool_use response (matches when the body has NO tool_result
    // block — i.e. the model is being asked for its first reply this turn).
    let tool_call_id = elena_types::ToolCallId::new();
    let input_json = r#"{"word":"firetest"}"#;
    let input_quoted = serde_json::to_string(input_json).expect("static");
    let tool_use_body = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_t\",\"role\":\"assistant\",\"model\":\"{MODEL_ID}\",\"usage\":{usage}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{tool_call_id}\",\"name\":\"echo_reverse\",\"input\":{{}}}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{input_quoted}}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{usage}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    );

    let text_body = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_x\",\"role\":\"assistant\",\"model\":\"{MODEL_ID}\",\"usage\":{usage}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"firetest ack: tsetterif\"}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{usage}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    );

    // Order matters: register the more-specific (body contains tool_result)
    // matcher FIRST so it wins when applicable; the catch-all tool_use
    // matcher handles fresh turns.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(wiremock::matchers::body_string_contains("tool_result"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(text_body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(tool_use_body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    server
}
