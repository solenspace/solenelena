//! Hannlys end-to-end smoke.
//!
//! Exercises E1 (Seed buyer flow) and E2 (re-personalization) against a
//! real LLM (Groq when `ELENA_PROVIDERS__GROQ__API_KEY` is set) and the
//! Notion connector talking to either a wiremock Notion (default,
//! CI-friendly) or the real Notion API (when `NOTION_TOKEN` is set with
//! a live integration token + `NOTION_PARENT_PAGE_ID` for the parent).
//!
//! Flow:
//!
//! 1. Bring up Postgres + Redis + NATS testcontainers, plus the Notion
//!    connector spawned in-process pointed at wiremock / real Notion.
//! 2. Create two tenants via the admin API: `creator` and `buyer`.
//! 3. Register the Notion plugin owned by `creator` only (A5). Verify
//!    that a buyer with no ownership row + no allow-list entry sees
//!    NO Notion tools — proves the per-tenant filter works in the
//!    real loop, not just at the store layer.
//! 4. Add `buyer` as a co-owner; allow-list `notion` for the buyer.
//!    Verify the next turn DOES expose Notion tools.
//! 5. E1 — Buyer's first turn: ask the LLM to create a personalized
//!    fitness page. Wait for `notion_create_page` tool result + done.
//! 6. E2 — Buyer's re-personalization: same thread, second turn
//!    asking for an update. Wait for another tool call + done. Asserts
//!    the conversation context survives across turns and the model
//!    can make follow-up tool calls.
//! 7. Verify audit_events has rows for both turns.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

use std::{io::Write as _, net::SocketAddr, process::ExitCode, sync::Arc};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RateLimitsConfig, RedisConfig, RouterConfig, TierEntry, TierModels,
};
use elena_connector_notion::NotionConnector;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{CacheAllowlist, CachePolicy, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use elena_memory::EpisodicMemory;
use elena_plugins::{PluginRegistry, PluginsConfig, proto::pb};
use elena_router::ModelRouter;
use elena_store::Store;
use elena_tools::ToolRegistry;
use elena_types::{ModelId, SessionId, TenantId, TenantTier, ThreadId, UserId, WorkspaceId};
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
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                tracing_subscriber::EnvFilter::new(
                    "warn,elena_gateway=info,elena_worker=info,elena_admin=info,\
                     elena_plugins=info",
                )
            },
        ))
        .with_writer(std::io::stderr)
        .init();

    let Some(groq_key) = std::env::var("ELENA_PROVIDERS__GROQ__API_KEY")
        .or_else(|_| std::env::var("GROQ_API_KEY"))
        .ok()
    else {
        eprintln!(
            "elena-hannlys-smoke: SKIP — no Groq key in env. Set GROQ_API_KEY or \
             ELENA_PROVIDERS__GROQ__API_KEY to run."
        );
        return ExitCode::from(2);
    };

    match run(groq_key).await {
        Ok(()) => {
            println!("\nelena-hannlys-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-hannlys-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(groq_key: String) -> anyhow::Result<()> {
    eprintln!("elena-hannlys-smoke: bring-up…");

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

    // ---- Notion connector — real or wiremock-backed ----
    //
    // When `NOTION_TOKEN` and `NOTION_PARENT_PAGE_ID` are both present
    // we drive the live Notion API; this is the production-shaped path
    // for Hannlys creators delivering Seed pages to buyers. Otherwise
    // we keep the wiremock fixture so contributors without a Notion
    // workspace can still run the smoke locally.
    let live_notion = matches!(
        (std::env::var("NOTION_TOKEN"), std::env::var("NOTION_PARENT_PAGE_ID")),
        (Ok(_), Ok(_))
    );
    let (notion_mock_keepalive, connector) = if live_notion {
        let token = SecretString::from(std::env::var("NOTION_TOKEN").unwrap_or_default());
        let base = std::env::var("NOTION_API_BASE_URL")
            .unwrap_or_else(|_| "https://api.notion.com".into());
        eprintln!("  i Notion: LIVE (api.notion.com) — pages will persist in your workspace");
        (None, NotionConnector::new(base, token))
    } else {
        let notion_mock = MockServer::start().await;
        mount_notion_mocks(&notion_mock).await;
        let token = SecretString::from("secret_test_hannlys_smoke");
        let c = NotionConnector::new(notion_mock.uri(), token);
        eprintln!("  i Notion: wiremock — set NOTION_TOKEN + NOTION_PARENT_PAGE_ID for live");
        (Some(notion_mock), c)
    };

    // ---- Spawn Notion connector as a gRPC sidecar ----
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let notion_addr = listener.local_addr()?;
    let connector_handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(pb::elena_plugin_server::ElenaPluginServer::new(connector))
            .serve_with_incoming(incoming)
            .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // ---- Elena config + store ----
    let cfg = ElenaConfig {
        postgres: PostgresConfig {
            url: SecretString::from(format!("postgres://elena:elena@127.0.0.1:{pg_port}/elena")),
            pool_max: 6,
            pool_min: 2,
            connect_timeout_ms: 10_000,
            statement_timeout_ms: 30_000,
        },
        redis: RedisConfig {
            url: SecretString::from(format!("redis://127.0.0.1:{redis_port}")),
            pool_max: 4,
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

    // ---- LLM (Groq via OpenAI-compat) ----
    let groq_cfg = OpenAiCompatConfig {
        api_key: SecretString::from(groq_key),
        base_url: "https://api.groq.com/openai/v1".into(),
        request_timeout_ms: Some(60_000),
        connect_timeout_ms: 10_000,
        max_attempts: 3,
    };
    let groq_client = Arc::new(OpenAiCompatClient::new("groq", &groq_cfg)?);
    let mut mux = elena_llm::LlmMultiplexer::new("groq");
    mux.register("groq", groq_client as Arc<dyn LlmClient>);
    let llm: Arc<dyn LlmClient> = Arc::new(mux);

    let model = ModelId::new("llama-3.3-70b-versatile");
    let tier_routing = TierModels {
        fast: TierEntry { provider: "groq".into(), model: model.clone() },
        standard: TierEntry { provider: "groq".into(), model: model.clone() },
        premium: TierEntry { provider: "groq".into(), model },
    };

    // ---- Plugin registry ----
    let tools = ToolRegistry::new();
    let plugins_cfg = PluginsConfig {
        endpoints: vec![format!("http://{notion_addr}")],
        ..PluginsConfig::default()
    };
    let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
    plugins.register_all(&plugins_cfg).await?;
    eprintln!(
        "elena-hannlys-smoke: registered {} plugin(s); {} tools",
        plugins.manifests().len(),
        tools.len()
    );

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
    let jwt_secret = "elena-hannlys-smoke-secret";
    let gateway_cfg = GatewayConfig {
        listen_addr,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: "elena-hannlys".into(),
            audience: "elena-hannlys-clients".into(),
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
        worker_id: "hannlys-smoke-worker".into(),
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

    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build()?;

    // ---- Tenants ----
    let creator_id = TenantId::new();
    let buyer_id = TenantId::new();
    for (id, name) in [(creator_id, "hannlys-creator"), (buyer_id, "hannlys-buyer")] {
        let resp = http
            .post(format!("http://{listen_addr}/admin/v1/tenants"))
            .json(&json!({"id": id, "name": name, "tier": "pro"}))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("create tenant {name} failed: {}", resp.status());
        }
    }
    eprintln!("  ✓ tenants created (creator + buyer)");

    // E5-style guard: notion plugin owned by creator only.
    let resp = http
        .put(format!("http://{listen_addr}/admin/v1/plugins/notion/owners"))
        .json(&json!({"owners": [creator_id]}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("set notion owners failed: {}", resp.status());
    }
    eprintln!("  ✓ notion plugin scoped to creator only (A5)");

    // Now grant buyer as co-owner + add to allow-list (the "purchase" event).
    let resp = http
        .put(format!("http://{listen_addr}/admin/v1/plugins/notion/owners"))
        .json(&json!({"owners": [creator_id, buyer_id]}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("expand notion owners failed: {}", resp.status());
    }
    let resp = http
        .patch(format!("http://{listen_addr}/admin/v1/tenants/{buyer_id}/allowed-plugins"))
        .json(&json!({"allowed_plugin_ids": ["notion"]}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("set buyer allow-list failed: {}", resp.status());
    }
    eprintln!("  ✓ buyer purchased — added to ownership + allow-list");

    // Workspace with a Hannlys-style guardrail prompt. Keep it tight:
    // describe the role, tell the model exactly which tool to call,
    // give a concrete parent id that satisfies the schema's
    // minLength=32. Live mode uses the operator's real parent page;
    // wiremock mode uses a 32-char fake (the wiremock route accepts
    // any value).
    let parent_page_id = std::env::var("NOTION_PARENT_PAGE_ID")
        .unwrap_or_else(|_| "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into());
    let buyer_workspace_id = WorkspaceId::new();
    let resp = http
        .post(format!("http://{listen_addr}/admin/v1/workspaces"))
        .json(&json!({
            "id": buyer_workspace_id,
            "tenant_id": buyer_id,
            "name": "hannlys-buyer-ws",
            "global_instructions": format!(
                "You are a fitness Seed Product. When the buyer asks for a workout plan, \
                 call the notion_create_page tool ONCE with: \
                 parent_page_id='{parent_page_id}', \
                 title containing 'Workout' and today's date, \
                 and 3 paragraphs of exercises. After you receive the tool result, \
                 reply with one short confirmation sentence and stop."
            ),
            "allowed_plugin_ids": ["notion"]
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create workspace failed: {}", resp.status());
    }
    eprintln!("  ✓ buyer workspace created with Seed-style guardrail");

    // Create thread + JWT.
    let buyer_user = UserId::new();
    let thread_id =
        create_thread(&http, listen_addr, jwt_secret, buyer_id, buyer_user, buyer_workspace_id)
            .await?;
    eprintln!("  thread_id = {thread_id}");

    // ---- E1: Seed buyer flow ----
    eprintln!("\nelena-hannlys-smoke: E1 — buyer requests personalized workout");
    let token = mint_token(jwt_secret, buyer_id, buyer_user, buyer_workspace_id)?;
    let (saw_tool, reply_chars) = run_turn(
        listen_addr,
        &token,
        thread_id,
        "I have dumbbells and a yoga mat. I want to focus on upper body strength. \
             I'm a beginner. Please create my workout plan.",
    )
    .await?;
    if !saw_tool {
        anyhow::bail!("E1: model never called notion_create_page");
    }
    eprintln!("  ✓ E1 green — Notion page created (reply chars={reply_chars})");

    // ---- E2: Re-personalization on the SAME thread ----
    eprintln!("\nelena-hannlys-smoke: E2 — re-personalization on same thread");
    let (saw_tool2, reply_chars2) = run_turn(
        listen_addr,
        &token,
        thread_id,
        "I just bought a pull-up bar. Update my workout plan to include pull-ups. \
             Use the notion_create_page tool again to create the new version.",
    )
    .await?;
    if !saw_tool2 {
        anyhow::bail!("E2: model never called notion_create_page on the second turn");
    }
    eprintln!("  ✓ E2 green — re-personalization (reply chars={reply_chars2})");

    // ---- Audit verification ----
    let mut audit_kinds: Vec<String> = Vec::new();
    for _ in 0..40 {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT kind FROM audit_events WHERE tenant_id = $1 AND thread_id = $2 \
             ORDER BY created_at ASC",
        )
        .bind(buyer_id.as_uuid())
        .bind(thread_id.as_uuid())
        .fetch_all(store.threads.pool_for_test())
        .await
        .unwrap_or_default();
        if rows.len() >= 4 {
            audit_kinds = rows.into_iter().map(|(k,)| k).collect();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let tool_use_count = audit_kinds.iter().filter(|k| k.as_str() == "tool_use").count();
    let tool_result_count = audit_kinds.iter().filter(|k| k.as_str() == "tool_result").count();
    if tool_use_count < 2 || tool_result_count < 2 {
        anyhow::bail!(
            "audit: expected ≥2 tool_use + ≥2 tool_result rows for E1+E2, got kinds={audit_kinds:?}"
        );
    }
    eprintln!(
        "  ✓ audit: {} rows total ({tool_use_count} tool_use + {tool_result_count} tool_result)",
        audit_kinds.len()
    );

    cancel.cancel();
    plugins.shutdown().await;
    connector_handle.abort();
    let _ = gw_handle.await;
    let _ = wk_handle.await;
    drop(notion_mock_keepalive);
    Ok(())
}

async fn mount_notion_mocks(server: &MockServer) {
    Mock::given(method("POST"))
        .and(wm_path("/v1/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "page",
            "id": "00000000-0000-0000-0000-000000000001",
            "url": "https://notion.example/page-1",
            "archived": false,
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(wm_path("/v1/users/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "user",
            "id": "bot-1",
            "type": "bot",
        })))
        .mount(server)
        .await;
}

async fn create_thread(
    http: &reqwest::Client,
    listen_addr: SocketAddr,
    jwt_secret: &str,
    tenant_id: TenantId,
    user_id: UserId,
    workspace_id: WorkspaceId,
) -> anyhow::Result<ThreadId> {
    let token = mint_token(jwt_secret, tenant_id, user_id, workspace_id)?;
    let resp = http
        .post(format!("http://{listen_addr}/v1/threads"))
        .header("authorization", format!("Bearer {token}"))
        .json(&json!({"title": "hannlys-smoke"}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create_thread returned {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    let thread_id: ThreadId = serde_json::from_value(body["thread_id"].clone())?;
    Ok(thread_id)
}

async fn run_turn(
    listen_addr: SocketAddr,
    token: &str,
    thread_id: ThreadId,
    prompt: &str,
) -> anyhow::Result<(bool, usize)> {
    let url = format!("ws://{listen_addr}/v1/threads/{thread_id}/stream");
    let req = Request::builder()
        .method("GET")
        .uri(&url)
        .header("host", listen_addr.to_string())
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .header("authorization", format!("Bearer {token}"))
        .body(())?;
    let (mut socket, _) = tokio_tungstenite::connect_async(req).await?;

    let send = json!({
        "action": "send_message",
        "text": prompt,
        "model": "llama-3.3-70b-versatile",
        // Yolo so we don't pause on every tool call (notion is a write).
        "autonomy": "yolo",
    })
    .to_string();
    socket.send(tokio_tungstenite::tungstenite::Message::Text(send)).await?;

    print!("    Reply: ");
    std::io::stdout().flush().ok();
    let mut text = String::new();
    let mut saw_tool_result = false;
    let mut got_done = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while !got_done {
        let next = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("turn timed out"))?;
        let Some(msg) = next else { break };
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t)?;
            match v.get("event").and_then(|e| e.as_str()) {
                Some("text_delta") => {
                    if let Some(d) = v.get("delta").and_then(|v| v.as_str()) {
                        text.push_str(d);
                        print!("{d}");
                        std::io::stdout().flush().ok();
                    }
                }
                Some("tool_result") => {
                    saw_tool_result = true;
                }
                Some("done") => got_done = true,
                Some("error") => anyhow::bail!("error event: {t}"),
                _ => {}
            }
        }
    }
    println!();
    info!(saw_tool_result, chars = text.len(), "turn complete");
    Ok((saw_tool_result, text.len()))
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
        iss: "elena-hannlys".into(),
        aud: "elena-hannlys-clients".into(),
        exp,
    };
    let token = encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok(token)
}
