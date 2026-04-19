//! Phase-6 smoke binary.
//!
//! Spins the full Phase-5 stack (Postgres + Redis + NATS + gateway + worker)
//! plus an in-process [`elena_connector_echo::EchoConnector`] plugin sidecar,
//! then exercises one WebSocket → NATS → worker → plugin round-trip. The
//! LLM is either the real Anthropic API (when `ANTHROPIC_API_KEY` is set)
//! or a `wiremock` two-response transcript that stands in for the
//! tool-use → tool-result → final-text sequence.
//!
//! Exit 0 on round-trip success. Exit 1 on any failure.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound
)]

use std::{collections::HashMap, io::Write as _, net::SocketAddr, process::ExitCode, sync::Arc};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RedisConfig, RouterConfig,
};
use elena_connector_echo::EchoConnector;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_memory::EpisodicMemory;
use elena_plugins::{PluginRegistry, PluginsConfig, proto::pb};
use elena_router::ModelRouter;
use elena_store::{Store, TenantRecord};
use elena_tools::ToolRegistry;
use elena_types::{
    BudgetLimits, PermissionSet, SessionId, TenantId, TenantTier, ThreadId, ToolCallId, UserId,
    WorkspaceId,
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
use tonic::transport::Server;
use tracing::info;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path as wm_path},
};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                tracing_subscriber::EnvFilter::new(
                    "warn,elena_gateway=info,elena_worker=info,elena_plugins=info",
                )
            },
        ))
        .with_writer(std::io::stderr)
        .init();

    // Provider selection: first key found wins. Order favours free tiers
    // (Groq is fastest; OpenRouter has a generous free catalogue; Anthropic
    // comes last). No key at all → wiremock fallback for CI.
    let groq_key = std::env::var("ELENA_PROVIDERS__GROQ__API_KEY")
        .or_else(|_| std::env::var("GROQ_API_KEY"))
        .ok();
    let openrouter_key = std::env::var("ELENA_PROVIDERS__OPENROUTER__API_KEY")
        .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
        .ok();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();

    let mode = if let Some(k) = groq_key {
        eprintln!("elena-phase6-smoke: routing `fast` tier to Groq (llama-3.3-70b-versatile).");
        RunMode::Groq { api_key: k }
    } else if let Some(k) = openrouter_key {
        eprintln!(
            "elena-phase6-smoke: routing `fast` tier to OpenRouter (meta-llama/llama-3.3-70b-instruct:free)."
        );
        RunMode::OpenRouter { api_key: k }
    } else if let Some(k) = anthropic_key {
        eprintln!("elena-phase6-smoke: routing `fast` tier to Anthropic (claude-haiku).");
        RunMode::Anthropic { api_key: k }
    } else {
        eprintln!(
            "elena-phase6-smoke: no provider key in env — running with wiremock as a fake \
             Anthropic backend. Exercises the full gateway → NATS → worker → plugin path."
        );
        RunMode::Mock
    };

    match run(mode).await {
        Ok(()) => {
            println!("\nelena-phase6-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-phase6-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

enum RunMode {
    Anthropic { api_key: String },
    Groq { api_key: String },
    OpenRouter { api_key: String },
    Mock,
}

async fn run(mode: RunMode) -> anyhow::Result<()> {
    eprintln!("elena-phase6-smoke: spinning Postgres + Redis + NATS + echo connector…");
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

    // Spawn echo connector on a free loopback port.
    let echo_listener = TcpListener::bind("127.0.0.1:0").await?;
    let echo_addr = echo_listener.local_addr()?;
    let echo_handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(echo_listener);
        let _ = Server::builder()
            .add_service(pb::elena_plugin_server::ElenaPluginServer::new(EchoConnector))
            .serve_with_incoming(incoming)
            .await;
    });
    // Give the server a moment to start accepting.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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

    let tenant_id = TenantId::new();
    let user_id = UserId::new();
    let workspace_id = WorkspaceId::new();
    store
        .tenants
        .upsert_tenant(&TenantRecord {
            id: tenant_id,
            name: "phase6-smoke".into(),
            tier: TenantTier::Pro,
            budget: BudgetLimits::DEFAULT_PRO,
            permissions: PermissionSet::default(),
            metadata: HashMap::new(),
            allowed_plugin_ids: vec![],
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await?;

    // Build a multi-provider LLM client. Each RunMode registers its
    // provider under the matching name, sets that name as the multiplexer
    // default, and points the router's `fast` tier at the provider's
    // free-tier tool-using model.
    let (mux, tier_routing, _mock_keepalive) = build_llm(&mode).await?;
    let llm: Arc<dyn elena_llm::LlmClient> = Arc::new(mux);

    // Register the echo plugin into the shared ToolRegistry.
    let tools = ToolRegistry::new();
    let plugins_cfg = PluginsConfig {
        endpoints: vec![format!("http://{echo_addr}")],
        ..PluginsConfig::default()
    };
    let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
    plugins.register_all(&plugins_cfg).await?;
    eprintln!(
        "elena-phase6-smoke: registered {} plugin(s); synthesised {} tool(s)",
        plugins.manifests().len(),
        tools.len()
    );

    let context =
        Arc::new(ContextManager::new(Arc::new(NullEmbedder), ContextManagerOptions::default()));
    let memory = Arc::new(EpisodicMemory::new(Arc::new(store.episodes.clone())));
    let router = Arc::new(ModelRouter::new(tier_routing, 2));
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
        rate_limits: Arc::new(elena_config::RateLimitsConfig::default()),
        metrics: Arc::clone(&metrics),
        defaults: Arc::new(DefaultsConfig::default()),
    });

    // Boot the gateway on a random local port.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr: SocketAddr = listener.local_addr()?;
    info!(%listen_addr, "phase6-smoke gateway bound");

    let jwt_secret = "elena-phase6-smoke-secret";
    let gateway_cfg = GatewayConfig {
        listen_addr,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: "elena-phase6".into(),
            audience: "elena-phase6-clients".into(),
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

    let worker_cfg = WorkerConfig {
        nats_url,
        worker_id: "phase6-smoke-worker".into(),
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

    let token = mint_token(jwt_secret, tenant_id, user_id, workspace_id)?;

    // Create a thread.
    let http = reqwest_like_create_thread(&listen_addr, &token).await?;
    let thread_id = http.thread_id;
    println!("thread_id = {thread_id}");

    // Open WebSocket.
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
        "text": "Please reverse the word \"hello\" using the echo_reverse tool and tell me the result.",
        "model": model_override,
    })
    .to_string();
    socket.send(tokio_tungstenite::tungstenite::Message::Text(send)).await?;

    println!("Reply: ");
    std::io::stdout().flush().ok();

    let mut assistant_text = String::new();
    let mut saw_tool_result = false;
    let mut got_done = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let next_frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for a stream event"))?;
        let Some(msg) = next_frame else { break };
        let msg = msg?;
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                let v: serde_json::Value = serde_json::from_str(&text)?;
                match v.get("event").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        if let Some(d) = v.get("delta").and_then(|v| v.as_str()) {
                            assistant_text.push_str(d);
                            print!("{d}");
                            std::io::stdout().flush().ok();
                        }
                    }
                    Some("tool_result") => {
                        saw_tool_result = true;
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
    plugins.shutdown().await;
    echo_handle.abort();
    let _ = gw_handle.await;
    let _ = wk_handle.await;

    if !saw_tool_result {
        anyhow::bail!("no tool_result event observed — echo plugin did not run");
    }
    if !got_done {
        anyhow::bail!("no done event received");
    }
    if !assistant_text.contains("olleh") {
        anyhow::bail!(
            "assistant did not mention the reversed word 'olleh' (got: {assistant_text:?})"
        );
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
        iss: "elena-phase6".into(),
        aud: "elena-phase6-clients".into(),
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

/// Serve two sequential responses on `/v1/messages`:
///   1. `tool_use` for `echo_reverse({"word":"hello"})`
///   2. Text response containing "olleh"
async fn setup_mock_anthropic() -> anyhow::Result<MockServer> {
    let server = MockServer::start().await;
    let usage = r#"{"input_tokens":12,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}"#;

    // --- first call: tool_use(echo_reverse, {"word": "hello"}) ---
    let tool_call_id = ToolCallId::new();
    let input_json = r#"{"word":"hello"}"#;
    // The input_json_delta partial_json field is quoted JSON — escape carefully.
    let input_json_quoted =
        serde_json::to_string(input_json).map_err(|e| anyhow::anyhow!("encode input_json: {e}"))?;
    let tool_use_body = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_01\",\"role\":\"assistant\",\"model\":\"claude-haiku-4-5-20251001\",\"usage\":{usage}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{tool_call_id}\",\"name\":\"echo_reverse\",\"input\":{{}}}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{input_json_quoted}}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{usage}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n"
    );
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(tool_use_body.into_bytes(), "text/event-stream"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // --- second call: text containing "olleh" ---
    let text_body = format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_02\",\"role\":\"assistant\",\"model\":\"claude-haiku-4-5-20251001\",\"usage\":{usage}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"The reversed word is: olleh\"}}}}\n\n\
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
                .set_body_raw(text_body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    Ok(server)
}

/// Build a multiplexer + a `TierModels` routing table suited to the chosen
/// [`RunMode`]. Returns the still-alive mock server when in wiremock mode so
/// the caller can keep it in scope.
#[allow(clippy::too_many_lines)]
async fn build_llm(
    mode: &RunMode,
) -> anyhow::Result<(elena_llm::LlmMultiplexer, elena_config::TierModels, Option<MockServer>)> {
    use elena_config::{TierEntry, TierModels};
    use elena_llm::{LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
    use elena_types::ModelId;

    let mut mux = elena_llm::LlmMultiplexer::new(match mode {
        RunMode::Anthropic { .. } | RunMode::Mock => "anthropic",
        RunMode::Groq { .. } => "groq",
        RunMode::OpenRouter { .. } => "openrouter",
    });

    let (tier_routing, mock_keepalive): (TierModels, Option<MockServer>) = match mode {
        RunMode::Anthropic { api_key } => {
            let cfg = elena_config::AnthropicConfig {
                api_key: SecretString::from(api_key.clone()),
                base_url: std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".to_owned()),
                api_version: "2023-06-01".to_owned(),
                request_timeout_ms: Some(60_000),
                connect_timeout_ms: 10_000,
                max_attempts: 3,
            };
            let client = Arc::new(AnthropicClient::new(
                &cfg,
                AnthropicAuth::ApiKey(SecretString::from(api_key.clone())),
            )?);
            mux.register("anthropic", client as Arc<dyn LlmClient>);
            let tiers = TierModels {
                fast: TierEntry {
                    provider: "anthropic".into(),
                    model: ModelId::new("claude-haiku-4-5-20251001"),
                },
                ..TierModels::default()
            };
            (tiers, None)
        }
        RunMode::Groq { api_key } => {
            let cfg = OpenAiCompatConfig {
                api_key: SecretString::from(api_key.clone()),
                base_url: "https://api.groq.com/openai/v1".into(),
                request_timeout_ms: Some(60_000),
                connect_timeout_ms: 10_000,
                max_attempts: 3,
            };
            let client = Arc::new(OpenAiCompatClient::new("groq", &cfg)?);
            mux.register("groq", client as Arc<dyn LlmClient>);
            let tiers = TierModels {
                fast: TierEntry {
                    provider: "groq".into(),
                    model: ModelId::new("llama-3.3-70b-versatile"),
                },
                standard: TierEntry {
                    provider: "groq".into(),
                    model: ModelId::new("llama-3.3-70b-versatile"),
                },
                premium: TierEntry {
                    provider: "groq".into(),
                    model: ModelId::new("llama-3.3-70b-versatile"),
                },
            };
            (tiers, None)
        }
        RunMode::OpenRouter { api_key } => {
            let cfg = OpenAiCompatConfig {
                api_key: SecretString::from(api_key.clone()),
                base_url: "https://openrouter.ai/api/v1".into(),
                request_timeout_ms: Some(60_000),
                connect_timeout_ms: 10_000,
                max_attempts: 3,
            };
            let client = Arc::new(OpenAiCompatClient::new("openrouter", &cfg)?);
            mux.register("openrouter", client as Arc<dyn LlmClient>);
            let free_model = std::env::var("ELENA_SMOKE_MODEL")
                .unwrap_or_else(|_| "meta-llama/llama-3.3-70b-instruct:free".to_owned());
            let tiers = TierModels {
                fast: TierEntry { provider: "openrouter".into(), model: ModelId::new(&free_model) },
                standard: TierEntry {
                    provider: "openrouter".into(),
                    model: ModelId::new(&free_model),
                },
                premium: TierEntry {
                    provider: "openrouter".into(),
                    model: ModelId::new(&free_model),
                },
            };
            (tiers, None)
        }
        RunMode::Mock => {
            let mock = setup_mock_anthropic().await?;
            let cfg = elena_config::AnthropicConfig {
                api_key: SecretString::from("sk-mock".to_owned()),
                base_url: mock.uri(),
                api_version: "2023-06-01".to_owned(),
                request_timeout_ms: Some(60_000),
                connect_timeout_ms: 10_000,
                max_attempts: 3,
            };
            let client = Arc::new(AnthropicClient::new(
                &cfg,
                AnthropicAuth::ApiKey(SecretString::from("sk-mock".to_owned())),
            )?);
            mux.register("anthropic", client as Arc<dyn LlmClient>);
            let tiers = TierModels {
                fast: TierEntry {
                    provider: "anthropic".into(),
                    model: ModelId::new("claude-haiku-4-5-20251001"),
                },
                ..TierModels::default()
            };
            (tiers, Some(mock))
        }
    };

    Ok((mux, tier_routing, mock_keepalive))
}
