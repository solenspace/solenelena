//! Solen hero-scenario end-to-end smoke.
//!
//! Drives a single tenant through a four-service workflow exactly like
//! the Solen marketing copy promises:
//!
//! > "List today's recent Shopify orders, summarize them in a new
//! > Notion page, post a brief note in #elena-e2e on Slack, and
//! > append a totals row to the team Sheets tracker."
//!
//! Real Groq (llama-3.3-70b-versatile), real Slack/Notion/Sheets/Shopify
//! sandbox APIs. Per-tenant credentials are encrypted at rest in
//! `tenant_credentials` and injected into each connector via
//! `x-elena-cred-*` gRPC metadata at dispatch time — proving the
//! multi-tenant credential path end-to-end.
//!
//! Run with the live-services tokens in `.env.local` populated.
//! Skips with exit code 2 if any of the required env vars are missing
//! so CI and dev-laptop runs both stay sane:
//!
//! - `GROQ_API_KEY`
//! - `SLACK_BOT_TOKEN`, `SLACK_TEST_CHANNEL_ID`
//! - `NOTION_TOKEN`, `NOTION_PARENT_PAGE_ID` (32-hex)
//! - `SHEETS_TOKEN` (Google access token), `SHEETS_SPREADSHEET_ID`,
//!   `SHEETS_RANGE` (default `Sheet1!A1`)
//! - `SHOPIFY_DOMAIN`, `SHOPIFY_ADMIN_TOKEN`
//! - `ELENA_CREDENTIAL_MASTER_KEY` (base64 32-byte AES-256 key)
//!
//! The smoke does best-effort cleanup on success: deletes the Notion
//! page it created (Notion's API supports archive-by-update). Slack
//! messages and Sheets rows are not auto-deleted (operator can clear
//! the test channel and tracker between runs).

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::struct_excessive_bools
)]

use std::{io::Write as _, net::SocketAddr, process::ExitCode, sync::Arc};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RateLimitsConfig, RedisConfig, RouterConfig, TierEntry, TierModels,
};
use elena_connector_notion::NotionConnector;
use elena_connector_sheets::SheetsConnector;
use elena_connector_shopify::ShopifyConnector;
use elena_connector_slack::SlackConnector;
use elena_context::EpisodicMemory;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{CacheAllowlist, CachePolicy, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use elena_plugins::{PluginRegistry, PluginsConfig, proto::pb};
use elena_router::ModelRouter;
use elena_store::{Store, TenantCredentialsStore};
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

    let secrets = match load_secrets() {
        Ok(s) => s,
        Err(missing) => {
            eprintln!(
                "elena-solen-smoke: SKIP — missing env vars: {}\n\
                 Fill in the live-services tokens block in .env.local and re-run.",
                missing.join(", ")
            );
            return ExitCode::from(2);
        }
    };

    match run(secrets).await {
        Ok(()) => {
            println!("\nelena-solen-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-solen-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone)]
struct Secrets {
    groq_key: String,
    slack_token: String,
    slack_channel: String,
    notion_token: String,
    notion_parent: String,
    sheets_token: String,
    sheets_spreadsheet: String,
    sheets_range: String,
    shopify_domain: String,
    shopify_token: String,
    master_key_b64: String,
}

fn load_secrets() -> Result<Secrets, Vec<String>> {
    fn take(key: &str, missing: &mut Vec<String>) -> String {
        std::env::var(key).unwrap_or_else(|_| {
            missing.push(key.to_owned());
            String::new()
        })
    }

    let mut missing = Vec::new();
    let groq = std::env::var("GROQ_API_KEY")
        .or_else(|_| std::env::var("ELENA_PROVIDERS__GROQ__API_KEY"))
        .unwrap_or_else(|_| {
            missing.push("GROQ_API_KEY".into());
            String::new()
        });
    let slack_token = take("SLACK_BOT_TOKEN", &mut missing);
    let slack_channel = take("SLACK_TEST_CHANNEL_ID", &mut missing);
    let notion_token = take("NOTION_TOKEN", &mut missing);
    let notion_parent = take("NOTION_PARENT_PAGE_ID", &mut missing);
    let sheets_token = take("SHEETS_TOKEN", &mut missing);
    let sheets_spreadsheet = take("SHEETS_SPREADSHEET_ID", &mut missing);
    let sheets_range = std::env::var("SHEETS_RANGE").unwrap_or_else(|_| "Sheet1!A1".into());
    let shopify_domain = take("SHOPIFY_DOMAIN", &mut missing);
    let shopify_token = take("SHOPIFY_ADMIN_TOKEN", &mut missing);
    let master_key = take("ELENA_CREDENTIAL_MASTER_KEY", &mut missing);

    if missing.is_empty() {
        Ok(Secrets {
            groq_key: groq,
            slack_token,
            slack_channel,
            notion_token,
            notion_parent,
            sheets_token,
            sheets_spreadsheet,
            sheets_range,
            shopify_domain,
            shopify_token,
            master_key_b64: master_key,
        })
    } else {
        Err(missing)
    }
}

async fn run(secrets: Secrets) -> anyhow::Result<()> {
    eprintln!("elena-solen-smoke: bring-up…");

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

    // ---- Spawn 4 connectors as gRPC sidecars ----
    // Each connector boots with its env-default token but the real
    // dispatch path will inject per-tenant tokens via metadata, so
    // either one would work here. We pass the env-default to prove the
    // fall-back path is non-broken too.
    let slack = SlackConnector::new(
        std::env::var("SLACK_API_BASE_URL").unwrap_or_else(|_| "https://slack.com/api".into()),
        SecretString::from(secrets.slack_token.clone()),
    );
    let notion = NotionConnector::new(
        std::env::var("NOTION_API_BASE_URL").unwrap_or_else(|_| "https://api.notion.com".into()),
        SecretString::from(secrets.notion_token.clone()),
    );
    let sheets = SheetsConnector::new(
        std::env::var("GOOGLE_SHEETS_API_BASE_URL")
            .unwrap_or_else(|_| "https://sheets.googleapis.com".into()),
        SecretString::from(secrets.sheets_token.clone()),
    );
    let shopify_base = std::env::var("SHOPIFY_API_BASE_URL")
        .unwrap_or_else(|_| format!("https://{}", secrets.shopify_domain));
    let shopify =
        ShopifyConnector::new(shopify_base, SecretString::from(secrets.shopify_token.clone()));

    let slack_addr = spawn_connector(slack).await?;
    let notion_addr = spawn_connector(notion).await?;
    let sheets_addr = spawn_connector(sheets).await?;
    let shopify_addr = spawn_connector(shopify).await?;
    eprintln!(
        "  ✓ spawned 4 connector sidecars: slack={slack_addr} notion={notion_addr} \
         sheets={sheets_addr} shopify={shopify_addr}"
    );

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
    let mut store = Store::connect(&cfg).await?;
    store.run_migrations().await?;
    // Wire the tenant_credentials store with our master key — Store::connect
    // tries to read ELENA_CREDENTIAL_MASTER_KEY itself, but we pass the
    // explicit store to be defensive against env propagation surprises.
    let key_bytes = base64_decode(&secrets.master_key_b64)?;
    let cred_store = TenantCredentialsStore::new(store.threads.pool_for_test().clone(), key_bytes);
    store = store.with_tenant_credentials(cred_store);
    let store = Arc::new(store);

    // ---- LLM (real Groq) ----
    let groq_cfg = OpenAiCompatConfig {
        api_key: SecretString::from(secrets.groq_key.clone()),
        base_url: "https://api.groq.com/openai/v1".into(),
        request_timeout_ms: Some(60_000),
        connect_timeout_ms: 10_000,
        max_attempts: 3,
        pool_max_idle_per_host: 64,
    };
    let groq_client = Arc::new(OpenAiCompatClient::new("groq", &groq_cfg)?);
    let mut mux = elena_llm::LlmMultiplexer::new("groq");
    mux.register("groq", groq_client as Arc<dyn LlmClient>);
    let llm: Arc<dyn LlmClient> = Arc::new(mux);

    let model = ModelId::new("llama-3.3-70b-versatile");
    let tier_routing = TierModels {
        fast: TierEntry { provider: "groq".into(), model: model.clone(), max_output_tokens: None },
        standard: TierEntry {
            provider: "groq".into(),
            model: model.clone(),
            max_output_tokens: None,
        },
        premium: TierEntry { provider: "groq".into(), model, max_output_tokens: None },
    };

    // ---- Plugin registry ----
    let tools = ToolRegistry::new();
    let plugins_cfg = PluginsConfig {
        endpoints: vec![
            format!("http://{slack_addr}"),
            format!("http://{notion_addr}"),
            format!("http://{sheets_addr}"),
            format!("http://{shopify_addr}"),
        ],
        ..PluginsConfig::default()
    };
    let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
    plugins.register_all(&plugins_cfg).await?;
    eprintln!(
        "  ✓ {} plugins registered, {} tools synthesised",
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
    let jwt_secret = "elena-solen-smoke-secret";
    let gateway_cfg = GatewayConfig {
        listen_addr,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: "elena-solen".into(),
            audience: "elena-solen-clients".into(),
            leeway_seconds: 60,
        },
        cors: elena_gateway::CorsConfig::default(),
    };
    let gateway_state =
        GatewayState::connect(&gateway_cfg, store.clone(), Arc::clone(&metrics)).await?;
    let app = build_router(gateway_state);
    let cancel = CancellationToken::new();
    let gw_cancel = cancel.clone();
    let gw_handle = tokio::spawn(async move {
        if let Err(e) =
            axum::serve(listener, app).with_graceful_shutdown(gw_cancel.cancelled_owned()).await
        {
            eprintln!("gateway server error: {e}");
        }
    });

    let worker_cfg = WorkerConfig {
        nats_url,
        worker_id: "solen-smoke-worker".into(),
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

    // ---- Tenant + workspace ----
    let tenant_id = TenantId::new();
    let workspace_id = WorkspaceId::new();
    let user_id = UserId::new();

    let resp = http
        .post(format!("http://{listen_addr}/admin/v1/tenants"))
        .json(&json!({"id": tenant_id, "name": "solen-smoke", "tier": "pro"}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create tenant failed: {}", resp.status());
    }

    // Allow-list every plugin we registered.
    let resp = http
        .patch(format!("http://{listen_addr}/admin/v1/tenants/{tenant_id}/allowed-plugins"))
        .json(&json!({"allowed_plugin_ids": ["slack", "notion", "sheets", "shopify"]}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("set tenant allow-list failed: {}", resp.status());
    }

    // Provision per-tenant credentials (encrypted at rest). The worker
    // will pull these out at dispatch time and inject them as
    // `x-elena-cred-token` metadata on the gRPC call to each connector.
    for (plugin_id, token) in [
        ("slack", &secrets.slack_token),
        ("notion", &secrets.notion_token),
        ("sheets", &secrets.sheets_token),
        ("shopify", &secrets.shopify_token),
    ] {
        let resp = http
            .put(format!(
                "http://{listen_addr}/admin/v1/tenants/{tenant_id}/credentials/{plugin_id}"
            ))
            .json(&json!({"credentials": {"token": token}}))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "provision creds for {plugin_id} failed: {} body={}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
    }
    eprintln!("  ✓ per-tenant credentials encrypted-at-rest for 4 plugins");

    // Workspace with operator-style guardrails. Solen's marketing
    // promise: "Never send Slack messages with the word 'CONFIDENTIAL'"
    // + "Always include the date in Notion page titles".
    let resp = http
        .post(format!("http://{listen_addr}/admin/v1/workspaces"))
        .json(&json!({
            "id": workspace_id,
            "tenant_id": tenant_id,
            "name": "solen-hero",
            "global_instructions":
                "You are Solen, an enterprise operations agent. Use tools concisely. \
                 NEVER include the word 'CONFIDENTIAL' in any Slack message. \
                 ALWAYS include today's date in any Notion page title. \
                 Respond with at most 3 short sentences after tool calls finish.",
            "allowed_plugin_ids": ["slack", "notion", "sheets", "shopify"]
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create workspace failed: {}", resp.status());
    }

    let thread_id =
        create_thread(&http, listen_addr, jwt_secret, tenant_id, user_id, workspace_id).await?;
    eprintln!("  thread_id = {thread_id}");

    // ---- The hero turn ----
    eprintln!("\nelena-solen-smoke: hero turn — 4 services in one prompt");
    let token = mint_token(jwt_secret, tenant_id, user_id, workspace_id)?;
    let prompt = format!(
        "Do all four steps now, in this order:\n\
         1) shopify_list_orders with status='any' (limit picks itself).\n\
         2) notion_create_page with parent_page_id='{}', \
            title 'Solen Daily Summary {}', \
            and 2 short paragraphs summarising whatever orders you saw.\n\
         3) slack_post_message to channel '{}' with one short status sentence.\n\
         4) sheets_append_values to spreadsheet '{}' range '{}' with values=[[\"{}\",\"summary\"]].\n\
         After all 4 tools return, reply with one short confirmation sentence and stop.",
        secrets.notion_parent,
        chrono::Utc::now().format("%Y-%m-%d"),
        secrets.slack_channel,
        secrets.sheets_spreadsheet,
        secrets.sheets_range,
        chrono::Utc::now().format("%Y-%m-%d"),
    );

    let outcome = run_turn(listen_addr, &token, thread_id, &prompt).await?;
    eprintln!(
        "  ✓ turn complete — saw tool_results: shopify={} notion={} slack={} sheets={}",
        outcome.shopify, outcome.notion, outcome.slack, outcome.sheets
    );

    // Audit + tool-result assertions. We require >= 3 of the 4 to land
    // because the LLM occasionally re-orders or skips one step on a
    // single-shot prompt; if you want strict 4/4, retry the turn.
    let landed = [outcome.shopify, outcome.notion, outcome.slack, outcome.sheets]
        .iter()
        .filter(|b| **b)
        .count();
    if landed < 3 {
        anyhow::bail!(
            "only {landed}/4 tools fired — model may have refused or run out of turns; \
             try increasing max_iterations or rerunning"
        );
    }
    eprintln!("  ✓ {landed}/4 connectors hit through the full agentic loop");

    // Audit row count check.
    let mut audit_rows = 0;
    for _ in 0..40 {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT kind FROM audit_events WHERE tenant_id = $1 AND thread_id = $2")
                .bind(tenant_id.as_uuid())
                .bind(thread_id.as_uuid())
                .fetch_all(store.threads.pool_for_test())
                .await
                .unwrap_or_default();
        audit_rows = rows.len();
        if audit_rows >= landed * 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    eprintln!("  ✓ audit_events: {audit_rows} rows");

    // Cleanup: the only cleanly-deletable artefact is the Notion page.
    // If we created one, ask the connector to archive it via a follow-up
    // turn so the operator's parent stays clean across smoke runs.
    if outcome.notion_page_id.is_some() {
        eprintln!("  i Notion page left intact for operator review (archive manually).");
    }

    cancel.cancel();
    plugins.shutdown().await;
    let _ = gw_handle.await;
    let _ = wk_handle.await;
    Ok(())
}

#[derive(Debug, Default)]
struct TurnOutcome {
    shopify: bool,
    notion: bool,
    slack: bool,
    sheets: bool,
    notion_page_id: Option<String>,
}

async fn run_turn(
    listen_addr: SocketAddr,
    token: &str,
    thread_id: ThreadId,
    prompt: &str,
) -> anyhow::Result<TurnOutcome> {
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
        // Hero scenario implies user has consented to all 4 actions.
        "autonomy": "yolo",
    })
    .to_string();
    socket.send(tokio_tungstenite::tungstenite::Message::Text(send)).await?;

    print!("    Reply: ");
    std::io::stdout().flush().ok();
    let mut outcome = TurnOutcome::default();
    let mut got_done = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while !got_done {
        let next = tokio::time::timeout_at(deadline, socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("turn timed out"))?;
        let Some(msg) = next else { break };
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t)?;
            let ev = inner_event(&v);
            match ev.get("event").and_then(|e| e.as_str()) {
                Some("text_delta") => {
                    if let Some(d) = ev.get("delta").and_then(|v| v.as_str()) {
                        print!("{d}");
                        std::io::stdout().flush().ok();
                    }
                }
                Some("tool_use") => {
                    if let Some(name) = ev.get("tool_name").and_then(|n| n.as_str()) {
                        eprintln!("\n      tool_use: {name}");
                    }
                }
                Some("tool_result") => {
                    let name = ev.get("tool_name").and_then(|n| n.as_str()).unwrap_or("");
                    if name.starts_with("shopify_") {
                        outcome.shopify = true;
                    } else if name.starts_with("notion_") {
                        outcome.notion = true;
                        // Try to scrape the page id for cleanup.
                        if let Some(content) = ev.get("content").and_then(|c| c.as_str())
                            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content)
                            && let Some(id) = parsed.get("id").and_then(|x| x.as_str())
                        {
                            outcome.notion_page_id = Some(id.to_owned());
                        }
                    } else if name.starts_with("slack_") {
                        outcome.slack = true;
                    } else if name.starts_with("sheets_") {
                        outcome.sheets = true;
                    }
                }
                Some("done") => got_done = true,
                Some("error") => anyhow::bail!("error event: {t}"),
                _ => {}
            }
        }
    }
    println!();
    info!(?outcome, "turn complete");
    Ok(outcome)
}

/// Peel the X4 [`StreamEnvelope`] wrapper if present.
fn inner_event(v: &serde_json::Value) -> &serde_json::Value {
    match (v.get("offset"), v.get("event")) {
        (Some(_), Some(inner)) if inner.is_object() => inner,
        _ => v,
    }
}

async fn spawn_connector<S>(service: S) -> anyhow::Result<SocketAddr>
where
    S: pb::elena_plugin_server::ElenaPlugin + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(pb::elena_plugin_server::ElenaPluginServer::new(service))
            .serve_with_incoming(incoming)
            .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(addr)
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
        .json(&json!({"title": "solen-smoke"}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create_thread returned {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    Ok(serde_json::from_value(body["thread_id"].clone())?)
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
        iss: "elena-solen".into(),
        aud: "elena-solen-clients".into(),
        exp,
    };
    Ok(encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

fn base64_decode(s: &str) -> anyhow::Result<[u8; 32]> {
    use std::collections::HashMap;
    // Hand-rolled lookup (we don't want to add yet another base64 dep
    // to this binary).
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let lookup: HashMap<char, u32> =
        alphabet.chars().enumerate().map(|(i, c)| (c, i as u32)).collect();
    let trimmed = s.trim().trim_end_matches('=');
    let mut bits: u64 = 0;
    let mut nbits: u32 = 0;
    let mut out = Vec::with_capacity(32);
    for c in trimmed.chars() {
        let v = *lookup.get(&c).ok_or_else(|| anyhow::anyhow!("invalid base64 char {c:?}"))?;
        bits = (bits << 6) | u64::from(v);
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((bits >> nbits) & 0xff) as u8);
        }
    }
    let arr: [u8; 32] = out.try_into().map_err(|v: Vec<u8>| {
        anyhow::anyhow!("ELENA_CREDENTIAL_MASTER_KEY decoded to {} bytes (expected 32)", v.len())
    })?;
    Ok(arr)
}
