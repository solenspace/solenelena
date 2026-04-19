//! Phase-5 smoke binary — spins gateway + worker + WebSocket client in a
//! single process and exchanges one turn end-to-end. NATS, Postgres, Redis
//! come from testcontainers; the LLM is either the real Anthropic API or a
//! `wiremock` fake.
//!
//! Environment:
//! - `ANTHROPIC_API_KEY` (optional): when set, the smoke runs against the
//!   real Anthropic Messages API. Without it, an in-process `wiremock`
//!   server stands in for Anthropic, which is enough to validate the full
//!   gateway → NATS → worker → store wire path.
//! - `ELENA_SMOKE_MODEL` (optional): override the default
//!   `claude-haiku-4-5-20251001` (only meaningful in real-API mode).
//!
//! Run with:
//! ```bash
//! # Wire-only validation (no API key required):
//! cargo run -p elena-phase5-smoke
//!
//! # Real API round-trip:
//! ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase5-smoke
//! ```

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound
)]

use std::{collections::HashMap, io::Write as _, net::SocketAddr, process::ExitCode, sync::Arc};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RedisConfig, RouterConfig, TierModels,
};
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_memory::EpisodicMemory;
use elena_router::ModelRouter;
use elena_store::{Store, TenantRecord};
use elena_tools::ToolRegistry;
use elena_types::{
    BudgetLimits, PermissionSet, SessionId, TenantId, TenantTier, ThreadId, UserId, WorkspaceId,
};
use elena_worker::{WorkerConfig, run_worker};
use futures::{SinkExt, StreamExt};
use jsonwebtoken::{EncodingKey, Header, encode};
use secrecy::SecretString;
use serde_json::json;
use testcontainers::{GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_util::sync::CancellationToken;
use tracing::info;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path as wm_path},
};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("warn,elena_gateway=info,elena_worker=info"),
        ))
        .with_writer(std::io::stderr)
        .init();

    let mode = if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
        RunMode::Real { api_key: k }
    } else {
        eprintln!(
            "elena-phase5-smoke: ANTHROPIC_API_KEY not set — running with wiremock as the LLM \
             backend (validates the full gateway → NATS → worker wire path)."
        );
        RunMode::Mock
    };

    match run(mode).await {
        Ok(()) => {
            println!("\nelena-phase5-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-phase5-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

enum RunMode {
    Real { api_key: String },
    Mock,
}

async fn run(mode: RunMode) -> anyhow::Result<()> {
    eprintln!("elena-phase5-smoke: spinning Postgres + Redis + NATS…");
    let pg = GenericImage::new("pgvector/pgvector", "pg16")
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
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

    let cfg = ElenaConfig {
        postgres: PostgresConfig {
            url: SecretString::from(format!("postgres://elena:elena@127.0.0.1:{pg_port}/elena")),
            pool_max: 4,
            pool_min: 1,
            connect_timeout_ms: 10_000,
            statement_timeout_ms: 30_000,
        },
        redis: RedisConfig {
            url: SecretString::from(format!("redis://127.0.0.1:{redis_port}")),
            pool_max: 2,
            thread_claim_ttl_ms: 60_000,
        },
        anthropic: None,
        cache: CacheConfig::default(),
        logging: LoggingConfig { level: "warn".into(), format: LogFormat::Pretty },
        defaults: DefaultsConfig::default(),
        context: ContextConfig::default(),
        router: RouterConfig::default(),
        providers: elena_config::ProvidersConfig::default(),
        rate_limits: elena_config::RateLimitsConfig::default(),
    };

    let store = Arc::new(Store::connect(&cfg).await?);
    store.run_migrations().await?;

    // Tenant identities.
    let tenant_id = TenantId::new();
    let user_id = UserId::new();
    let workspace_id = WorkspaceId::new();
    store
        .tenants
        .upsert_tenant(&TenantRecord {
            id: tenant_id,
            name: "phase5-smoke".into(),
            tier: TenantTier::Pro,
            budget: BudgetLimits::DEFAULT_PRO,
            permissions: PermissionSet::default(),
            metadata: HashMap::new(),
            allowed_plugin_ids: vec![],
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await?;

    // Build LoopDeps shared by the worker. In mock mode we stand up a
    // wiremock server that pretends to be Anthropic and serves a single
    // text-only SSE response — enough to validate the full wire path.
    let (api_key, base_url, _mock_keepalive) = match mode {
        RunMode::Real { api_key } => (
            api_key,
            std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_owned()),
            None,
        ),
        RunMode::Mock => {
            let mock = setup_mock_anthropic().await?;
            let url = mock.uri();
            ("sk-mock".to_owned(), url, Some(mock))
        }
    };
    let anthropic_cfg = elena_config::AnthropicConfig {
        api_key: SecretString::from(api_key.clone()),
        base_url,
        api_version: "2023-06-01".to_owned(),
        request_timeout_ms: Some(60_000),
        connect_timeout_ms: 10_000,
        max_attempts: 3,
    };
    let anthropic_client = Arc::new(AnthropicClient::new(
        &anthropic_cfg,
        AnthropicAuth::ApiKey(SecretString::from(api_key)),
    )?);
    let mut mux = elena_llm::LlmMultiplexer::new("anthropic");
    mux.register("anthropic", anthropic_client as Arc<dyn elena_llm::LlmClient>);
    let llm: Arc<dyn elena_llm::LlmClient> = Arc::new(mux);
    let context =
        Arc::new(ContextManager::new(Arc::new(NullEmbedder), ContextManagerOptions::default()));
    let memory = Arc::new(EpisodicMemory::new(Arc::new(store.episodes.clone())));
    let router = Arc::new(ModelRouter::new(TierModels::default(), 2));
    let tools = ToolRegistry::new();
    let plugins = Arc::new(elena_plugins::PluginRegistry::empty(tools.clone()));
    let metrics = Arc::new(elena_observability::LoopMetrics::new()?);
    let deps = Arc::new(elena_core::LoopDeps {
        store: store.clone(),
        llm,
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools,
        context,
        memory,
        router,
        plugins,
        rate_limits: Arc::new(elena_config::RateLimitsConfig::default()),
        metrics: Arc::clone(&metrics),
        defaults: Arc::new(DefaultsConfig::default()),
    });

    // Boot the gateway on a random local port.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr: SocketAddr = listener.local_addr()?;
    info!(%listen_addr, "phase5-smoke gateway bound");

    let jwt_secret = "elena-phase5-smoke-secret";
    let gateway_cfg = GatewayConfig {
        listen_addr,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: "elena-phase5".into(),
            audience: "elena-phase5-clients".into(),
            leeway_seconds: 60,
        },
    };

    let gateway_state =
        GatewayState::connect(&gateway_cfg, store.clone(), Arc::clone(&metrics)).await?;
    let router_app = build_router(gateway_state);
    let cancel = CancellationToken::new();

    let gw_cancel = cancel.clone();
    let gw_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router_app)
            .with_graceful_shutdown(gw_cancel.cancelled_owned())
            .await
        {
            eprintln!("gateway server error: {e}");
        }
    });

    // Boot the worker.
    let worker_cfg = WorkerConfig {
        nats_url,
        worker_id: "phase5-smoke-worker".into(),
        max_concurrent_loops: 4,
        durable_name: None,
        stream_name: None,
    };
    let wk_cancel = cancel.clone();
    let wk_deps = Arc::clone(&deps);
    let wk_handle = tokio::spawn(async move {
        if let Err(e) = run_worker(worker_cfg, wk_deps, wk_cancel).await {
            eprintln!("worker error: {e}");
        }
    });

    // Sign a JWT.
    let token = mint_token(jwt_secret, tenant_id, user_id, workspace_id)?;

    // Create a thread via HTTP.
    let http = reqwest_like_create_thread(&listen_addr, &token).await?;
    let thread_id = http.thread_id;
    println!("thread_id = {thread_id}");

    // Open the WebSocket.
    let ws_url = format!("ws://{listen_addr}/v1/threads/{thread_id}/stream");
    let req = Request::builder()
        .method("GET")
        .uri(&ws_url)
        .header("host", listen_addr.to_string())
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .header("authorization", format!("Bearer {token}"))
        .body(())?;
    let (mut socket, _resp) = tokio_tungstenite::connect_async(req).await?;

    let model_override =
        std::env::var("ELENA_SMOKE_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());
    let send = json!({
        "action": "send_message",
        "text": "Say hi in exactly five words.",
        "model": model_override,
    })
    .to_string();
    socket.send(tokio_tungstenite::tungstenite::Message::Text(send)).await?;

    println!("Reply: ");
    std::io::stdout().flush().ok();

    let mut got_text = false;
    let mut got_done = false;
    while let Some(msg) = socket.next().await {
        let msg = msg?;
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text)?;
                match v.get("event").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        if let Some(d) = v.get("delta").and_then(|v| v.as_str()) {
                            got_text = true;
                            print!("{d}");
                            std::io::stdout().flush().ok();
                        }
                    }
                    Some("done") => {
                        got_done = true;
                        break;
                    }
                    Some("error") => {
                        anyhow::bail!("error event from gateway: {text}");
                    }
                    _ => {}
                }
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }

    cancel.cancel();
    let _ = gw_handle.await;
    let _ = wk_handle.await;

    if !got_text {
        anyhow::bail!("no text delta received");
    }
    if !got_done {
        anyhow::bail!("no done event received");
    }
    Ok(())
}

fn mint_token(
    secret: &str,
    tenant_id: TenantId,
    user_id: UserId,
    workspace_id: WorkspaceId,
) -> anyhow::Result<String> {
    use serde::Serialize;
    #[derive(Serialize)]
    struct Claims {
        tenant_id: TenantId,
        user_id: UserId,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        tier: TenantTier,
        iss: String,
        aud: String,
        exp: u64,
    }
    let exp = u64::try_from(chrono::Utc::now().timestamp() + 3_600).unwrap_or(0);
    let claims = Claims {
        tenant_id,
        user_id,
        workspace_id,
        session_id: SessionId::new(),
        tier: TenantTier::Pro,
        iss: "elena-phase5".into(),
        aud: "elena-phase5-clients".into(),
        exp,
    };
    Ok(encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

#[derive(serde::Deserialize)]
struct CreateThreadResponse {
    thread_id: ThreadId,
}

async fn reqwest_like_create_thread(
    addr: &SocketAddr,
    token: &str,
) -> anyhow::Result<CreateThreadResponse> {
    // Avoid pulling reqwest just for one POST — use a tiny TCP+HTTP/1.1 send.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    let body = "{}";
    let req = format!(
        "POST /v1/threads HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let resp = String::from_utf8_lossy(&buf);
    let (_, body_part) =
        resp.split_once("\r\n\r\n").ok_or_else(|| anyhow::anyhow!("malformed HTTP response"))?;
    Ok(serde_json::from_str(body_part)?)
}

/// Spin up an in-process wiremock server that pretends to be Anthropic and
/// always responds with a one-shot text-only SSE message. Returned `MockServer`
/// must be kept alive for the test's lifetime.
async fn setup_mock_anthropic() -> anyhow::Result<MockServer> {
    let server = MockServer::start().await;
    let usage = r#"{"input_tokens":12,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}"#;
    let body = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_mock\",\"role\":\"assistant\",\"model\":\"claude-haiku-4-5-20251001\",\"usage\":{usage}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"Hi from wiremock five words\"}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{usage}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    );
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;
    Ok(server)
}
