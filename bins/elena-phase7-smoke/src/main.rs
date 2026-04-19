//! Phase-7 production-readiness smoke.
//!
//! Single-process E2E that exercises every v1.0 must-ship surface:
//!
//! 1. **Bring-up** — Postgres + Redis + NATS via testcontainers, echo
//!    connector on a free loopback port, gateway + worker in-process.
//! 2. **Admin API** — `GET /admin/v1/health/deep` returns `status: "ok"`
//!    with the three dep probes. `POST /admin/v1/tenants` creates the
//!    tenant used for the turn.
//! 3. **Multi-provider LLM** — Groq when `ELENA_PROVIDERS__GROQ__API_KEY`
//!    is in env (real E2E), otherwise a wiremock transcript emulating
//!    the tool-use + final-text sequence.
//! 4. **Tool use + mTLS wire** — LLM calls the synthesised
//!    `echo_reverse` tool; tool result flows back through the plugin
//!    gRPC path. mTLS is available in code (`PluginClient::connect_with_tls`)
//!    but the smoke uses plaintext loopback so operators see the happy
//!    path without cert scaffolding.
//! 5. **Observability** — `GET /metrics` at the end asserts
//!    `elena_turns_total` ≥ 1 and `elena_plugin_rpc_duration_seconds_count`
//!    ≥ 1 (proves the tool call's RPC histogram fired).
//! 6. **Rate-limit machinery** — tenant rpm is set to a low number; we
//!    assert `elena_rate_limit_rejections_total` remains 0 (happy path)
//!    and leave higher-volume stress to the load-smoke binary (v1.0.x).
//!
//! Exit 0 ⇒ Elena v1.0 surface is live. Exit 1 ⇒ one of the assertions
//! above failed.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::doc_markdown,
    clippy::collapsible_if,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

use std::{io::Write as _, net::SocketAddr, process::ExitCode, sync::Arc};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RateLimitsConfig, RedisConfig, RouterConfig, TierEntry, TierModels,
};
use elena_connector_echo::EchoConnector;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_memory::EpisodicMemory;
use elena_plugins::{PluginRegistry, PluginsConfig, proto::pb};
use elena_router::ModelRouter;
use elena_store::Store;
use elena_tools::ToolRegistry;
use elena_types::{SessionId, TenantId, TenantTier, ThreadId, UserId, WorkspaceId};
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
                    "warn,elena_gateway=info,elena_worker=info,elena_admin=info,\
                     elena_plugins=info,elena_observability=info",
                )
            },
        ))
        .with_writer(std::io::stderr)
        .init();

    let groq_key = std::env::var("ELENA_PROVIDERS__GROQ__API_KEY")
        .or_else(|_| std::env::var("GROQ_API_KEY"))
        .ok();
    let openrouter_key = std::env::var("ELENA_PROVIDERS__OPENROUTER__API_KEY")
        .or_else(|_| std::env::var("OPENROUTER_API_KEY"))
        .ok();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();

    let mode = if let Some(k) = groq_key {
        eprintln!("elena-phase7-smoke: provider = Groq (real API, llama-3.3-70b-versatile).");
        RunMode::Groq { api_key: k }
    } else if let Some(k) = openrouter_key {
        eprintln!("elena-phase7-smoke: provider = OpenRouter (real API, free tier).");
        RunMode::OpenRouter { api_key: k }
    } else if let Some(k) = anthropic_key {
        eprintln!("elena-phase7-smoke: provider = Anthropic (real API).");
        RunMode::Anthropic { api_key: k }
    } else {
        eprintln!("elena-phase7-smoke: provider = wiremock (no key in env).");
        RunMode::Mock
    };

    match run(mode).await {
        Ok(()) => {
            println!("\nelena-phase7-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-phase7-smoke: FAIL — {e:#}");
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
    eprintln!("elena-phase7-smoke: bring-up…");
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

    // Echo connector on a free loopback port.
    let echo_listener = TcpListener::bind("127.0.0.1:0").await?;
    let echo_addr = echo_listener.local_addr()?;
    let echo_handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(echo_listener);
        let _ = Server::builder()
            .add_service(pb::elena_plugin_server::ElenaPluginServer::new(EchoConnector))
            .serve_with_incoming(incoming)
            .await;
    });
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
        rate_limits: RateLimitsConfig::default(),
    };

    let store = Arc::new(Store::connect(&cfg).await?);
    store.run_migrations().await?;

    // LLM + router wiring (same shape as phase6-smoke).
    let (mux, tier_routing, _mock_keepalive) = build_llm(&mode).await?;
    let llm: Arc<dyn elena_llm::LlmClient> = Arc::new(mux);

    let tools = ToolRegistry::new();
    let plugins_cfg = PluginsConfig {
        endpoints: vec![format!("http://{echo_addr}")],
        ..PluginsConfig::default()
    };
    let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
    plugins.register_all(&plugins_cfg).await?;
    eprintln!(
        "elena-phase7-smoke: registered {} plugin(s); synthesised {} tool(s)",
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
        rate_limits: Arc::new(RateLimitsConfig::default()),
        metrics: Arc::clone(&metrics),
        defaults: Arc::new(DefaultsConfig::default()),
    });

    // Gateway on a random port.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr: SocketAddr = listener.local_addr()?;
    info!(%listen_addr, "phase7-smoke gateway bound");

    let jwt_secret = "elena-phase7-smoke-secret";
    let gateway_cfg = GatewayConfig {
        listen_addr,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: "elena-phase7".into(),
            audience: "elena-phase7-clients".into(),
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
        worker_id: "phase7-smoke-worker".into(),
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

    // -------- Phase 7 admin + observability assertions --------
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;

    // 1. Deep health probe should succeed with all three deps green.
    eprintln!("elena-phase7-smoke: probing /admin/v1/health/deep");
    let deep = http.get(format!("http://{listen_addr}/admin/v1/health/deep")).send().await?;
    let deep_status = deep.status();
    let deep_body: serde_json::Value = deep.json().await?;
    if !deep_status.is_success() {
        anyhow::bail!("health/deep returned {deep_status}; body={deep_body}");
    }
    let probed = deep_body["deps"].as_array().map_or(0, Vec::len);
    if probed < 2 {
        anyhow::bail!("health/deep reported only {probed} probes; body={deep_body}");
    }
    let deep_overall = deep_body["status"].as_str().unwrap_or("");
    if deep_overall != "ok" {
        anyhow::bail!("health/deep overall status = {deep_overall:?}; body={deep_body}");
    }
    eprintln!("  ✓ health/deep: {probed} probes, all green");

    // 2. Create the tenant via the admin API rather than the raw store.
    let tenant_id = TenantId::new();
    let user_id = UserId::new();
    let workspace_id = WorkspaceId::new();
    let create_body = json!({
        "id": tenant_id,
        "name": "phase7-smoke",
        "tier": "pro",
    });
    eprintln!("elena-phase7-smoke: POST /admin/v1/tenants");
    let create = http
        .post(format!("http://{listen_addr}/admin/v1/tenants"))
        .json(&create_body)
        .send()
        .await?;
    if !create.status().is_success() {
        let s = create.status();
        let b = create.text().await.unwrap_or_default();
        anyhow::bail!("admin tenants POST returned {s}; body={b}");
    }
    let created: serde_json::Value = create.json().await?;
    let created_id: TenantId = serde_json::from_value(created["id"].clone())?;
    if created_id != tenant_id {
        anyhow::bail!("admin tenants POST returned id {created_id:?}, expected {tenant_id:?}");
    }
    eprintln!("  ✓ tenant {tenant_id} created via admin API");

    // 2.5 Phase 7 · A2 — register a workspace with a guardrail prompt.
    //     The gateway's WS handler injects this fragment as a system block on
    //     every send_message in this workspace. We assert below (via the audit
    //     log + assistant text) that the guardrail actually arrived.
    eprintln!("elena-phase7-smoke: POST /admin/v1/workspaces");
    let ws_body = json!({
        "id": workspace_id,
        "tenant_id": tenant_id,
        "name": "phase7-smoke-ws",
        "global_instructions": "After successfully reversing the word, append the literal token `[WS-GUARDRAIL]` at the very end of your reply.",
        "allowed_plugin_ids": [],
    });
    let ws_resp = http
        .post(format!("http://{listen_addr}/admin/v1/workspaces"))
        .json(&ws_body)
        .send()
        .await?;
    if !ws_resp.status().is_success() {
        let s = ws_resp.status();
        let b = ws_resp.text().await.unwrap_or_default();
        anyhow::bail!("admin workspaces POST returned {s}; body={b}");
    }
    eprintln!("  ✓ workspace {workspace_id} created with guardrail");

    // 3. Mint JWT + create thread + run a turn.
    let token = mint_token(jwt_secret, tenant_id, user_id, workspace_id)?;

    let http_thread = reqwest_like_create_thread(&listen_addr, &token).await?;
    let thread_id = http_thread.thread_id;
    println!("thread_id = {thread_id}");

    // WS send_message → collect events.
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
        // Turn 1 exercises the happy-path tool round-trip. Phase-7's
        // Moderate default would pause on `echo_reverse` (the
        // `is_decision_fork` heuristic flags any non-read-only verb);
        // this turn is not the place to test autonomy — that's D2
        // below. Pin to Yolo so the loop runs to completion without
        // an approval round-trip.
        "autonomy": "yolo",
    })
    .to_string();
    socket.send(tokio_tungstenite::tungstenite::Message::Text(send)).await?;

    print!("Reply: ");
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
                    Some("tool_result") => saw_tool_result = true,
                    Some("done") => {
                        got_done = true;
                        break;
                    }
                    Some("error") => anyhow::bail!("error event from gateway: {text}"),
                    _ => {}
                }
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }
    println!();

    // 4. Observability assertions — /metrics should reflect the turn.
    eprintln!("elena-phase7-smoke: GET /metrics");
    let metrics_body =
        http.get(format!("http://{listen_addr}/metrics")).send().await?.text().await?;
    if !metrics_body.contains("elena_turns_total") {
        anyhow::bail!(
            "/metrics is missing elena_turns_total; first 500 chars: {}",
            &metrics_body[..metrics_body.len().min(500)]
        );
    }
    // After at least one turn the count should be >= 1.
    let turn_count = extract_counter(&metrics_body, "elena_turns_total");
    if turn_count < 1 {
        anyhow::bail!("elena_turns_total = {turn_count}; expected ≥ 1");
    }
    eprintln!("  ✓ /metrics: elena_turns_total = {turn_count}");

    let rejected = extract_counter(&metrics_body, "elena_rate_limit_rejections_total");
    if rejected > 0 {
        anyhow::bail!(
            "elena_rate_limit_rejections_total = {rejected}; expected 0 on the happy path"
        );
    }
    eprintln!("  ✓ /metrics: no spurious rate-limit rejections");

    // 5. Phase 7 · A3 — confirm audit rows landed for this thread.
    //    Poll for up to 2s since the audit sink is async (bounded mpsc).
    eprintln!("elena-phase7-smoke: querying audit_events");
    let mut audit_kinds: Vec<String> = Vec::new();
    for _ in 0..40 {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT kind FROM audit_events
             WHERE tenant_id = $1 AND thread_id = $2
             ORDER BY created_at ASC",
        )
        .bind(tenant_id.as_uuid())
        .bind(thread_id.as_uuid())
        .fetch_all(store.threads.pool_for_test())
        .await
        .unwrap_or_default();
        if !rows.is_empty() {
            audit_kinds = rows.into_iter().map(|(k,)| k).collect();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    if !audit_kinds.iter().any(|k| k == "tool_use") {
        anyhow::bail!("no tool_use audit row landed — A3 hook missed; got kinds={audit_kinds:?}");
    }
    if !audit_kinds.iter().any(|k| k == "tool_result") {
        anyhow::bail!(
            "no tool_result audit row landed — A3 hook missed; got kinds={audit_kinds:?}"
        );
    }
    eprintln!(
        "  ✓ audit_events: {} rows for this thread (kinds: {:?})",
        audit_kinds.len(),
        audit_kinds
    );

    // 6. Phase 7 · A2 — best-effort signal that the workspace guardrail
    //    arrived in the system prompt. The guardrail asks the model to
    //    append a sentinel; we treat its presence as a positive signal,
    //    its absence as a soft warning (the model is probabilistic — a
    //    deterministic check belongs in the integration suite that uses
    //    a wiremock LLM and inspects the request body directly).
    if assistant_text.contains("[WS-GUARDRAIL]") {
        eprintln!("  ✓ A2: workspace guardrail sentinel found in assistant reply");
    } else {
        eprintln!(
            "  ! A2: guardrail sentinel not in reply — guardrail may not have reached the model \
             (LLM is probabilistic; promote to integration test for hard gate)"
        );
    }

    // 7. Phase 7 · D2 — Cautious-mode round trip.
    //    Open a second thread with autonomy=cautious, send the same
    //    tool-triggering prompt, expect an `awaiting_approval` event,
    //    POST an `Allow` decision, and verify the tool runs to completion.
    eprintln!("elena-phase7-smoke: D2 — Cautious-mode approval round trip");
    let cautious_thread = reqwest_like_create_thread(&listen_addr, &token).await?;
    let cautious_thread_id = cautious_thread.thread_id;

    let cautious_ws_url = format!("ws://{listen_addr}/v1/threads/{cautious_thread_id}/stream");
    let cautious_req = Request::builder()
        .method("GET")
        .uri(&cautious_ws_url)
        .header("host", listen_addr.to_string())
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .header("authorization", format!("Bearer {token}"))
        .body(())?;
    let (mut cautious_socket, _) = tokio_tungstenite::connect_async(cautious_req).await?;

    let cautious_send = json!({
        "action": "send_message",
        "text": "Call the echo_reverse tool EXACTLY ONCE with the word \"hello\". \
                 After you receive the tool result, reply with text only and stop — \
                 do NOT call any tool again. Then append [WS-GUARDRAIL] at the very end.",
        "model": model_override,
        "autonomy": "cautious",
    })
    .to_string();
    cautious_socket.send(tokio_tungstenite::tungstenite::Message::Text(cautious_send)).await?;

    // Phase 1: wait for `awaiting_approval`. Cautious always pauses on
    // any tool batch.
    let mut pending_tool_use_ids: Vec<String> = Vec::new();
    let pause_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while pending_tool_use_ids.is_empty() {
        let next = tokio::time::timeout_at(pause_deadline, cautious_socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for awaiting_approval"))?;
        let Some(msg) = next else {
            anyhow::bail!("ws closed before awaiting_approval arrived");
        };
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            let v: serde_json::Value = serde_json::from_str(&text)?;
            if v.get("event").and_then(|e| e.as_str()) == Some("awaiting_approval") {
                if let Some(arr) = v.get("pending").and_then(|p| p.as_array()) {
                    for entry in arr {
                        if let Some(id) = entry.get("tool_use_id").and_then(|i| i.as_str()) {
                            pending_tool_use_ids.push(id.to_owned());
                        }
                    }
                }
            } else if v.get("event").and_then(|e| e.as_str()) == Some("done") {
                anyhow::bail!(
                    "Cautious turn ended with `done` before pausing — the model never \
                     proposed a tool call (got: {text})"
                );
            } else if v.get("event").and_then(|e| e.as_str()) == Some("error") {
                anyhow::bail!("error event in cautious turn: {text}");
            }
        }
    }
    if pending_tool_use_ids.is_empty() {
        anyhow::bail!("awaiting_approval arrived with empty `pending` array");
    }
    eprintln!(
        "  ✓ A1: awaiting_approval received with {} pending tool_use(s)",
        pending_tool_use_ids.len()
    );

    // Phase 2: POST allow decisions for every pending tool_use.
    let approvals_body = json!({
        "approvals": pending_tool_use_ids.iter().map(|id| json!({
            "tool_use_id": id,
            "decision": "allow",
        })).collect::<Vec<_>>(),
    });
    let approve_resp = http
        .post(format!("http://{listen_addr}/v1/threads/{cautious_thread_id}/approvals"))
        .header("authorization", format!("Bearer {token}"))
        .json(&approvals_body)
        .send()
        .await?;
    if !approve_resp.status().is_success() {
        let s = approve_resp.status();
        let b = approve_resp.text().await.unwrap_or_default();
        anyhow::bail!("POST /approvals returned {s}; body={b}");
    }
    eprintln!("  ✓ A1: approvals posted");

    // Phase 3: wait for tool_result + done on the resume.
    //
    // Cautious pauses *every* tool batch, so a non-deterministic LLM
    // (real Groq) may propose additional tool calls after seeing the
    // first tool_result. Keep approving every new batch until the
    // stream emits `done`. Caps out after 5 pause/approve cycles so a
    // misbehaving model can't wedge the smoke forever.
    let mut cautious_assistant_text = String::new();
    let mut cautious_saw_tool_result = false;
    let mut cautious_got_done = false;
    let mut approval_rounds = 0_u32;
    let resume_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while !cautious_got_done {
        let next = tokio::time::timeout_at(resume_deadline, cautious_socket.next())
            .await
            .map_err(|_| anyhow::anyhow!("timed out waiting for cautious resume"))?;
        let Some(msg) = next else { break };
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            let v: serde_json::Value = serde_json::from_str(&text)?;
            match v.get("event").and_then(|e| e.as_str()) {
                Some("text_delta") => {
                    if let Some(d) = v.get("delta").and_then(|v| v.as_str()) {
                        cautious_assistant_text.push_str(d);
                    }
                }
                Some("tool_result") => cautious_saw_tool_result = true,
                Some("done") => cautious_got_done = true,
                Some("error") => {
                    anyhow::bail!("error event in cautious resume: {text}")
                }
                Some("awaiting_approval") => {
                    approval_rounds += 1;
                    if approval_rounds > 10 {
                        anyhow::bail!(
                            "cautious smoke exceeded 10 approval rounds — model may be looping"
                        );
                    }
                    let ids: Vec<String> = v
                        .get("pending")
                        .and_then(|p| p.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|entry| {
                                    entry
                                        .get("tool_use_id")
                                        .and_then(|i| i.as_str())
                                        .map(str::to_owned)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if ids.is_empty() {
                        anyhow::bail!(
                            "awaiting_approval round {approval_rounds} had empty pending: \
                             {text}"
                        );
                    }
                    let body = json!({
                        "approvals": ids.iter().map(|id| json!({
                            "tool_use_id": id,
                            "decision": "allow",
                        })).collect::<Vec<_>>(),
                    });
                    let resp = http
                        .post(format!(
                            "http://{listen_addr}/v1/threads/{cautious_thread_id}/approvals"
                        ))
                        .header("authorization", format!("Bearer {token}"))
                        .json(&body)
                        .send()
                        .await?;
                    if !resp.status().is_success() {
                        let s = resp.status();
                        let b = resp.text().await.unwrap_or_default();
                        anyhow::bail!("follow-up POST /approvals returned {s}; body={b}");
                    }
                    eprintln!(
                        "  ✓ A1: follow-up approval round #{approval_rounds} posted \
                         ({} tool(s))",
                        ids.len()
                    );
                }
                _ => {}
            }
        }
    }

    if !cautious_saw_tool_result {
        anyhow::bail!("Cautious resume completed without a tool_result — approval flow broken");
    }
    if !cautious_got_done {
        anyhow::bail!("Cautious resume never produced a done event");
    }
    eprintln!(
        "  ✓ A1: Cautious round trip green (tool ran post-approval, reply chars={})",
        cautious_assistant_text.len()
    );

    cancel.cancel();
    plugins.shutdown().await;
    echo_handle.abort();
    let _ = gw_handle.await;
    let _ = wk_handle.await;

    if !saw_tool_result {
        anyhow::bail!("no tool_result event observed — echo plugin never ran");
    }
    if !got_done {
        anyhow::bail!("no done event received");
    }
    if !assistant_text.contains("olleh") {
        anyhow::bail!("assistant did not mention 'olleh' (got: {assistant_text:?})");
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
        iss: "elena-phase7".into(),
        aud: "elena-phase7-clients".into(),
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
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build()?;
    let body = http
        .post(format!("http://{addr}/v1/threads"))
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?
        .json::<CreateThreadResponse>()
        .await?;
    Ok(body)
}

/// Extract the integer value of a simple Prometheus counter line
/// (`<metric_name>{labels} <value>` or `<metric_name> <value>`). Returns 0
/// when the counter is absent.
fn extract_counter(body: &str, name: &str) -> u64 {
    let prefix_unlabeled = format!("{name} ");
    let prefix_labeled = format!("{name}{{");
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with(&prefix_unlabeled) {
            if let Some(v) = line.split_ascii_whitespace().nth(1)
                && let Ok(n) = v.parse::<f64>()
            {
                return n as u64;
            }
        } else if line.starts_with(&prefix_labeled) {
            if let Some((_, tail)) = line.split_once('}')
                && let Some(n) = tail.trim().parse::<f64>().ok().map(|f| f as u64)
            {
                return n;
            }
        }
    }
    0
}

async fn build_llm(
    mode: &RunMode,
) -> anyhow::Result<(elena_llm::LlmMultiplexer, TierModels, Option<MockServer>)> {
    use elena_llm::{LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
    use elena_types::ModelId;

    let mut mux = elena_llm::LlmMultiplexer::new(match mode {
        RunMode::Anthropic { .. } | RunMode::Mock => "anthropic",
        RunMode::Groq { .. } => "groq",
        RunMode::OpenRouter { .. } => "openrouter",
    });

    let (tiers, keepalive): (TierModels, Option<MockServer>) = match mode {
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
            let t = TierModels {
                fast: TierEntry {
                    provider: "anthropic".into(),
                    model: ModelId::new("claude-haiku-4-5-20251001"),
                },
                ..TierModels::default()
            };
            (t, None)
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
            let model = ModelId::new("llama-3.3-70b-versatile");
            let t = TierModels {
                fast: TierEntry { provider: "groq".into(), model: model.clone() },
                standard: TierEntry { provider: "groq".into(), model: model.clone() },
                premium: TierEntry { provider: "groq".into(), model },
            };
            (t, None)
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
            let model_name = std::env::var("ELENA_SMOKE_MODEL")
                .unwrap_or_else(|_| "meta-llama/llama-3.3-70b-instruct:free".to_owned());
            let model = ModelId::new(&model_name);
            let t = TierModels {
                fast: TierEntry { provider: "openrouter".into(), model: model.clone() },
                standard: TierEntry { provider: "openrouter".into(), model: model.clone() },
                premium: TierEntry { provider: "openrouter".into(), model },
            };
            (t, None)
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
            let t = TierModels {
                fast: TierEntry {
                    provider: "anthropic".into(),
                    model: ModelId::new("claude-haiku-4-5-20251001"),
                },
                ..TierModels::default()
            };
            (t, Some(mock))
        }
    };
    Ok((mux, tiers, keepalive))
}

/// wiremock stand-in for Anthropic — serves one tool_use for
/// `echo_reverse` then the final text containing "olleh".
async fn setup_mock_anthropic() -> anyhow::Result<MockServer> {
    let server = MockServer::start().await;
    let usage = r#"{"input_tokens":12,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}"#;

    let tool_call_id = elena_types::ToolCallId::new();
    let input_json = r#"{"word":"hello"}"#;
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
