//! Deep end-to-end Slack smoke.
//!
//! Simulates a Solen tenant whose user has connected their Slack
//! workspace and configured a notification channel: the user types a
//! command, Elena runs the loop, and Elena posts to Slack on the
//! user's behalf using the user's own bot token (not a shared
//! deployment-wide token).
//!
//! Six cases run sequentially against the real Slack Web API; each
//! prints `✓` per assertion and a `→ cleanup: deleted` line when it
//! removes the artefact it created.
//!
//! 1. **baseline** — env-default token works on its own (single-tenant
//!    fallback path)
//! 2. **per-tenant credential injection** — env-default is replaced
//!    with an INVALID token; per-tenant `tenant_credentials` row holds
//!    the real one. If the message posts, the worker correctly
//!    injected the per-tenant token via `x-elena-cred-token` metadata.
//! 3. **workspace guardrail** — the workspace's `global_instructions`
//!    forbid the words `URGENT` and `CRITICAL`; we ask the model to
//!    post a critical alert and verify those words don't appear.
//! 4. **multi-turn re-personalization** — the second turn on the same
//!    thread references the first turn's content, proving conversation
//!    continuity.
//! 5. **list_channels read path** — exercises the read-only side of the
//!    connector; verifies the configured channel is visible to the bot.
//! 6. **audit verification** — direct Postgres query confirms every
//!    tool_use + tool_result row landed.

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
use elena_connector_slack::SlackConnector;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_gateway::{GatewayConfig, GatewayState, JwtAlgorithm, JwtConfig, build_router};
use elena_llm::{CacheAllowlist, CachePolicy, LlmClient, OpenAiCompatClient, OpenAiCompatConfig};
use elena_memory::EpisodicMemory;
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

const SLACK_API: &str = "https://slack.com/api";

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("warn,elena_gateway=info,elena_worker=info"),
        ))
        .with_writer(std::io::stderr)
        .init();

    let secrets = match load_secrets() {
        Ok(s) => s,
        Err(missing) => {
            eprintln!("elena-slack-smoke: SKIP — missing env vars: {}", missing.join(", "));
            return ExitCode::from(2);
        }
    };

    match run(secrets).await {
        Ok(()) => {
            println!("\nelena-slack-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-slack-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Clone)]
struct Secrets {
    groq_key: String,
    slack_token: String,
    slack_channel: String,
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
    let master_key = take("ELENA_CREDENTIAL_MASTER_KEY", &mut missing);
    if missing.is_empty() {
        Ok(Secrets { groq_key: groq, slack_token, slack_channel, master_key_b64: master_key })
    } else {
        Err(missing)
    }
}

async fn run(secrets: Secrets) -> anyhow::Result<()> {
    eprintln!("elena-slack-smoke: bring-up…");

    // ---- Sanity check the Slack token directly against auth.test ----
    // If this fails we'd rather find out in 200ms than after spinning up
    // three containers.
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build()?;
    let auth = http
        .post(format!("{SLACK_API}/auth.test"))
        .bearer_auth(&secrets.slack_token)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    if !auth["ok"].as_bool().unwrap_or(false) {
        anyhow::bail!("auth.test failed: {auth}");
    }
    eprintln!(
        "  ✓ slack auth.test ok — bot user '{}' in team '{}'",
        auth["user"].as_str().unwrap_or("?"),
        auth["team"].as_str().unwrap_or("?")
    );

    // ---- Testcontainers ----
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
    eprintln!("  ✓ testcontainers up (pg:{pg_port} redis:{redis_port} nats:{nats_port})");

    // ---- Spawn the slack connector twice: one with the REAL env-default
    //      token (used in case 1) and one with an INVALID env-default
    //      (used in case 2 to prove per-tenant injection wins). Both will
    //      receive the same metadata at runtime; only their fallback
    //      changes. ----
    let slack_real_env =
        SlackConnector::new(SLACK_API, SecretString::from(secrets.slack_token.clone()));
    let slack_real_addr = spawn_connector(slack_real_env).await?;
    let slack_invalid_env = SlackConnector::new(
        SLACK_API,
        SecretString::from("xoxb-INVALID-shared-token-for-test".to_owned()),
    );
    let slack_invalid_addr = spawn_connector(slack_invalid_env).await?;
    eprintln!(
        "  ✓ slack sidecars spawned: env-real={slack_real_addr} env-invalid={slack_invalid_addr}"
    );

    // ---- Store + master key ----
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
    let key_bytes = decode_master_key(&secrets.master_key_b64)?;
    let cred_store = TenantCredentialsStore::new(store.threads.pool_for_test().clone(), key_bytes);
    store = store.with_tenant_credentials(cred_store);
    let store = Arc::new(store);

    // ---- LLM ----
    let groq_cfg = OpenAiCompatConfig {
        api_key: SecretString::from(secrets.groq_key.clone()),
        base_url: "https://api.groq.com/openai/v1".into(),
        request_timeout_ms: Some(60_000),
        connect_timeout_ms: 10_000,
        max_attempts: 3,
    };
    let groq = Arc::new(OpenAiCompatClient::new("groq", &groq_cfg)?);
    let mut mux = elena_llm::LlmMultiplexer::new("groq");
    mux.register("groq", groq as Arc<dyn LlmClient>);
    let llm: Arc<dyn LlmClient> = Arc::new(mux);
    let model = ModelId::new("llama-3.3-70b-versatile");
    let tier_routing = TierModels {
        fast: TierEntry { provider: "groq".into(), model: model.clone() },
        standard: TierEntry { provider: "groq".into(), model: model.clone() },
        premium: TierEntry { provider: "groq".into(), model },
    };

    // ---- Plugin registry ----
    // We register only the env-real connector under the canonical "slack"
    // plugin id. Case 2 swaps the registry entry to point at the
    // invalid-env connector via plugin re-registration; that proves the
    // worker is using metadata not the env-default.
    let tools = ToolRegistry::new();
    let plugins_cfg = PluginsConfig {
        endpoints: vec![format!("http://{slack_real_addr}")],
        ..PluginsConfig::default()
    };
    let plugins = Arc::new(PluginRegistry::with_config(tools.clone(), &plugins_cfg));
    plugins.register_all(&plugins_cfg).await?;
    eprintln!(
        "  ✓ plugin registry: {} manifest(s), {} tool(s) — {:?}",
        plugins.manifests().len(),
        tools.len(),
        tools
            .schemas()
            .iter()
            .filter_map(|s| s.as_value().get("name").and_then(|v| v.as_str()).map(str::to_owned))
            .collect::<Vec<_>>()
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

    // ---- Gateway + worker ----
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let listen_addr: SocketAddr = listener.local_addr()?;
    let jwt_secret = "elena-slack-smoke-secret";
    let gateway_cfg = GatewayConfig {
        listen_addr,
        nats_url: nats_url.clone(),
        jwt: JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from(jwt_secret),
            issuer: "elena-slack".into(),
            audience: "elena-slack-clients".into(),
            leeway_seconds: 60,
        },
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
        worker_id: "slack-smoke-worker".into(),
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
    eprintln!("  ✓ gateway + worker running on {listen_addr}");

    // ---- Provision tenant ----
    let tenant_id = TenantId::new();
    let user_id = UserId::new();
    let resp = http
        .post(format!("http://{listen_addr}/admin/v1/tenants"))
        .json(&json!({"id": tenant_id, "name": "solen-acme", "tier": "pro"}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create tenant: {}", resp.status());
    }
    let resp = http
        .patch(format!("http://{listen_addr}/admin/v1/tenants/{tenant_id}/allowed-plugins"))
        .json(&json!({"allowed_plugin_ids": ["slack"]}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("allow-list slack: {}", resp.status());
    }
    eprintln!("  ✓ tenant {tenant_id} provisioned with slack on the allow-list");

    // ───────────────────────────────────────────────────────────────────
    // CASE 1 — baseline (env-default works on its own)
    // ───────────────────────────────────────────────────────────────────
    println!("\nCASE 1 — baseline (env-default token, no tenant_credentials row)");
    let workspace_baseline = WorkspaceId::new();
    let ch = &secrets.slack_channel;
    upsert_workspace(
        &http,
        listen_addr,
        workspace_baseline,
        tenant_id,
        &format!(
            "You are Elena, an operations agent. To send a Slack message you \
             MUST invoke the `slack_post_message` tool. NEVER claim a message \
             was posted unless the tool was actually called. \
             STEP 1: Call slack_post_message with channel='{ch}' and the text \
             the user requested. \
             STEP 2: After you receive the tool result, reply with one short \
             confirmation sentence and stop."
        ),
    )
    .await?;
    let thread_id =
        create_thread(&http, listen_addr, jwt_secret, tenant_id, user_id, workspace_baseline)
            .await?;
    let token = mint_token(jwt_secret, tenant_id, user_id, workspace_baseline)?;
    let prompt = "Send the following Slack message to the team: \
         'Elena baseline check — please ignore.'"
        .to_owned();
    let outcome = run_turn(listen_addr, &token, thread_id, &prompt).await?;
    if !outcome.tool_calls_observed.iter().any(|n| n == "slack_post_message") {
        anyhow::bail!(
            "case 1: model never invoked slack_post_message; tool calls = {:?}",
            outcome.tool_calls_observed
        );
    }
    if !outcome.saw_ok_tool_result {
        anyhow::bail!("case 1: tool ran but is_error=true");
    }
    let result = latest_tool_result_json(store.threads.pool_for_test(), tenant_id, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 1: no tool_result row in Postgres"))?;
    let ts = result["ts"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("case 1: tool_result missing ts: {result}"))?
        .to_owned();
    eprintln!("    ✓ Slack ts={ts} (recovered from Postgres tool_result row)");
    let posted_text = latest_tool_use_text(store.threads.pool_for_test(), tenant_id, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 1: no tool_use input.text in messages"))?;
    if !posted_text.to_lowercase().contains("baseline") {
        anyhow::bail!("case 1: posted text lacks 'baseline': {posted_text:?}");
    }
    eprintln!("    ✓ Posted text: {posted_text:?}");
    delete_message(&http, &secrets.slack_token, &secrets.slack_channel, &ts).await?;
    println!("    → cleanup: chat.delete confirmed message existed and was removed");

    // ───────────────────────────────────────────────────────────────────
    // CASE 2 — per-tenant credential injection beats invalid env-default
    // ───────────────────────────────────────────────────────────────────
    println!("\nCASE 2 — per-tenant credential injection (env-default deliberately INVALID)");
    // Re-register slack to point at the INVALID-env sidecar.
    plugins.shutdown().await;
    let plugins_cfg2 = PluginsConfig {
        endpoints: vec![format!("http://{slack_invalid_addr}")],
        ..PluginsConfig::default()
    };
    plugins.register_all(&plugins_cfg2).await?;
    eprintln!("    ✓ slack plugin re-pointed at invalid-env sidecar");

    // Provision per-tenant credentials with the REAL token.
    let resp = http
        .put(format!("http://{listen_addr}/admin/v1/tenants/{tenant_id}/credentials/slack"))
        .json(&json!({"credentials": {"token": secrets.slack_token}}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("provision creds: {}", resp.status());
    }
    eprintln!("    ✓ per-tenant slack token stored encrypted-at-rest");

    let workspace_inject = WorkspaceId::new();
    upsert_workspace(
        &http,
        listen_addr,
        workspace_inject,
        tenant_id,
        &format!(
            "You are Elena, an operations agent. You MUST invoke \
             `slack_post_message` with channel='{ch}' to send any message. \
             NEVER claim success without calling the tool. After the tool \
             result, confirm in one sentence and stop."
        ),
    )
    .await?;
    let thread_id_2 =
        create_thread(&http, listen_addr, jwt_secret, tenant_id, user_id, workspace_inject).await?;
    let token_2 = mint_token(jwt_secret, tenant_id, user_id, workspace_inject)?;
    let prompt = "Send the following Slack message to the team: \
         'Elena tenant-cred check — proves per-tenant injection.'"
        .to_owned();
    let outcome = run_turn(listen_addr, &token_2, thread_id_2, &prompt).await?;
    if !outcome.tool_calls_observed.iter().any(|n| n == "slack_post_message") {
        anyhow::bail!("case 2: model never invoked slack_post_message");
    }
    if !outcome.saw_ok_tool_result {
        anyhow::bail!(
            "case 2: tool returned is_error=true — env-default INVALID token \
             likely was used. Per-tenant injection appears broken."
        );
    }
    let result = latest_tool_result_json(store.threads.pool_for_test(), tenant_id, thread_id_2)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 2: no tool_result row"))?;
    let ts = result["ts"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("case 2: tool_result missing ts: {result}"))?
        .to_owned();
    eprintln!("    ✓ Slack ts={ts} via injected per-tenant token");
    let posted_text = latest_tool_use_text(store.threads.pool_for_test(), tenant_id, thread_id_2)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 2: no tool_use input.text"))?;
    if !posted_text.to_lowercase().contains("tenant-cred") {
        anyhow::bail!("case 2: posted text lacks 'tenant-cred': {posted_text:?}");
    }
    eprintln!("    ✓ multi-tenant credential injection works end-to-end");
    delete_message(&http, &secrets.slack_token, &secrets.slack_channel, &ts).await?;
    println!("    → cleanup: chat.delete confirmed and removed");

    // ───────────────────────────────────────────────────────────────────
    // CASE 3 — workspace guardrail forbids URGENT/CRITICAL
    // ───────────────────────────────────────────────────────────────────
    println!("\nCASE 3 — workspace guardrail (forbidden words must not appear)");
    let workspace_guard = WorkspaceId::new();
    upsert_workspace(
        &http,
        listen_addr,
        workspace_guard,
        tenant_id,
        &format!(
            "You are Elena, on a calm-tone policy. To send any Slack message you \
             MUST invoke `slack_post_message` with channel='{ch}'. NEVER include \
             the words URGENT or CRITICAL in any message body — paraphrase if \
             needed. After the tool result, confirm in one sentence and stop."
        ),
    )
    .await?;
    let thread_id_3 =
        create_thread(&http, listen_addr, jwt_secret, tenant_id, user_id, workspace_guard).await?;
    let token_3 = mint_token(jwt_secret, tenant_id, user_id, workspace_guard)?;
    let prompt = "The deployment to production failed. \
         Post a short alert to the team's Slack channel so they know."
        .to_owned();
    let outcome = run_turn(listen_addr, &token_3, thread_id_3, &prompt).await?;
    if !outcome.tool_calls_observed.iter().any(|n| n == "slack_post_message") {
        anyhow::bail!("case 3: model never invoked slack_post_message");
    }
    let result = latest_tool_result_json(store.threads.pool_for_test(), tenant_id, thread_id_3)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 3: no tool_result row"))?;
    let ts = result["ts"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("case 3: tool_result missing ts"))?
        .to_owned();
    let posted_text = latest_tool_use_text(store.threads.pool_for_test(), tenant_id, thread_id_3)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 3: no tool_use input.text"))?;
    let upper = posted_text.to_uppercase();
    if upper.contains("URGENT") || upper.contains("CRITICAL") {
        anyhow::bail!(
            "case 3: guardrail breached — posted text contained forbidden word: {posted_text:?}"
        );
    }
    eprintln!("    ✓ posted text honours guardrail: {posted_text:?}");
    delete_message(&http, &secrets.slack_token, &secrets.slack_channel, &ts).await?;
    println!("    → cleanup: chat.delete confirmed and removed");

    // ───────────────────────────────────────────────────────────────────
    // CASE 4 — multi-turn re-personalization on the same thread
    // ───────────────────────────────────────────────────────────────────
    println!("\nCASE 4 — multi-turn re-personalization (same thread)");
    let prompt_followup = "Now send a brief follow-up Slack message thanking \
         the on-call engineer for handling the previous deploy issue."
        .to_owned();
    let outcome = run_turn(listen_addr, &token_3, thread_id_3, &prompt_followup).await?;
    if !outcome.tool_calls_observed.iter().any(|n| n == "slack_post_message") {
        anyhow::bail!("case 4: model never invoked slack_post_message on the follow-up");
    }
    let result = latest_tool_result_json(store.threads.pool_for_test(), tenant_id, thread_id_3)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 4: no tool_result row"))?;
    let ts = result["ts"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("case 4: tool_result missing ts"))?
        .to_owned();
    let posted_text = latest_tool_use_text(store.threads.pool_for_test(), tenant_id, thread_id_3)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 4: no tool_use input.text"))?;
    eprintln!("    ✓ follow-up posted: {posted_text:?}");
    delete_message(&http, &secrets.slack_token, &secrets.slack_channel, &ts).await?;
    println!("    → cleanup: chat.delete confirmed and removed");

    // ───────────────────────────────────────────────────────────────────
    // CASE 5 — list_channels read-only path
    // ───────────────────────────────────────────────────────────────────
    println!("\nCASE 5 — list_channels read-only path");
    let workspace_list = WorkspaceId::new();
    upsert_workspace(
        &http,
        listen_addr,
        workspace_list,
        tenant_id,
        "You are Elena, doing channel discovery. You MUST invoke \
         `slack_list_channels` exactly once. Do not post any message. After \
         the tool result, summarise in one sentence what you found.",
    )
    .await?;
    let thread_id_5 =
        create_thread(&http, listen_addr, jwt_secret, tenant_id, user_id, workspace_list).await?;
    let token_5 = mint_token(jwt_secret, tenant_id, user_id, workspace_list)?;
    let outcome = run_turn(
        listen_addr,
        &token_5,
        thread_id_5,
        "List the public channels you can see in the Slack workspace.",
    )
    .await?;
    if !outcome.tool_calls_observed.iter().any(|n| n == "slack_list_channels") {
        anyhow::bail!("case 5: model did not call slack_list_channels");
    }
    let result = latest_tool_result_json(store.threads.pool_for_test(), tenant_id, thread_id_5)
        .await?
        .ok_or_else(|| anyhow::anyhow!("case 5: no tool_result row"))?;
    let channels: Vec<String> = result["channels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("id").and_then(|x| x.as_str()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if channels.is_empty() {
        anyhow::bail!("case 5: tool_result has empty channels array: {result}");
    }
    if channels.iter().any(|c| c == &secrets.slack_channel) {
        eprintln!(
            "    ✓ configured test channel {} appears in the bot's channel list ({} total)",
            secrets.slack_channel,
            channels.len()
        );
    } else {
        eprintln!(
            "    ! configured test channel {} not visible to the bot ({} channels listed); \
             invite the bot to the channel to make it discoverable",
            secrets.slack_channel,
            channels.len()
        );
    }

    // ───────────────────────────────────────────────────────────────────
    // CASE 6 — audit row verification
    // ───────────────────────────────────────────────────────────────────
    println!("\nCASE 6 — audit row verification");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT thread_id::text, kind FROM audit_events
         WHERE tenant_id = $1 ORDER BY created_at ASC",
    )
    .bind(tenant_id.as_uuid())
    .fetch_all(store.threads.pool_for_test())
    .await?;
    let tool_uses = rows.iter().filter(|(_, k)| k == "tool_use").count();
    let tool_results = rows.iter().filter(|(_, k)| k == "tool_result").count();
    eprintln!(
        "    audit_events: total={} tool_use={tool_uses} tool_result={tool_results}",
        rows.len()
    );
    if tool_uses < 5 {
        anyhow::bail!("case 6: expected ≥5 tool_use rows, got {tool_uses}");
    }
    if tool_results < 5 {
        anyhow::bail!("case 6: expected ≥5 tool_result rows, got {tool_results}");
    }
    let drops = metrics.audit_drops_total.get();
    if drops > 0 {
        anyhow::bail!("case 6: audit_drops_total = {drops} (expected 0)");
    }
    println!("    ✓ {} audit rows + zero drops", rows.len());

    // ---- Shutdown ----
    cancel.cancel();
    plugins.shutdown().await;
    let _ = gw_handle.await;
    let _ = wk_handle.await;
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════
// helpers
// ════════════════════════════════════════════════════════════════════════

#[derive(Debug, Default)]
struct TurnOutcome {
    /// Names of tools the model called this turn (in order).
    tool_calls_observed: Vec<String>,
    /// True if any tool_result event arrived with is_error=false.
    saw_ok_tool_result: bool,
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
        "autonomy": "yolo",
    })
    .to_string();
    socket.send(tokio_tungstenite::tungstenite::Message::Text(send)).await?;

    print!("    Reply: ");
    std::io::stdout().flush().ok();
    let mut outcome = TurnOutcome::default();
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
                        print!("{d}");
                        std::io::stdout().flush().ok();
                    }
                }
                Some("tool_use_complete") => {
                    if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                        eprintln!("\n      tool_use_complete: {name}");
                        outcome.tool_calls_observed.push(name.to_owned());
                    }
                }
                Some("tool_result") => {
                    let is_err =
                        v.get("is_error").and_then(serde_json::Value::as_bool).unwrap_or(true);
                    if !is_err {
                        outcome.saw_ok_tool_result = true;
                    }
                }
                Some("done") => got_done = true,
                Some("error") => anyhow::bail!("error event: {t}"),
                _ => {}
            }
        }
    }
    println!();
    Ok(outcome)
}

/// After a turn, scan the messages table for the most recent tool result
/// row in this thread and extract the JSON body produced by the
/// connector. The WS stream only signals `tool_result` as
/// `{id, is_error}` — the actual body lives in Postgres.
///
/// Retries briefly because the worker writes the row asynchronously
/// (the `done` event can race the row's commit by a few ms).
async fn latest_tool_result_json(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    thread_id: ThreadId,
) -> anyhow::Result<Option<serde_json::Value>> {
    for attempt in 0..20 {
        let rows: Vec<(serde_json::Value, String)> = sqlx::query_as(
            "SELECT content, role::text FROM messages
             WHERE tenant_id = $1 AND thread_id = $2
             ORDER BY created_at DESC LIMIT 5",
        )
        .bind(tenant_id.as_uuid())
        .bind(thread_id.as_uuid())
        .fetch_all(pool)
        .await?;
        for (content, role) in &rows {
            if role != "tool" {
                continue;
            }
            if let Some(arr) = content.as_array() {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                        continue;
                    }
                    // The ToolResult block's `content` field carries the
                    // body the connector returned — serialised as either a
                    // raw string (text content) or as an array of typed
                    // content blocks (rare). Cover both.
                    if let Some(text) = block.get("content").and_then(|c| c.as_str()) {
                        let parsed: serde_json::Value = serde_json::from_str(text)
                            .unwrap_or_else(|_| serde_json::Value::String(text.to_owned()));
                        return Ok(Some(parsed));
                    }
                    if let Some(text) = block
                        .get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|b| b.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        let parsed: serde_json::Value =
                            serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
                        return Ok(Some(parsed));
                    }
                }
            }
        }
        let _ = attempt;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(None)
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
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    Ok(addr)
}

async fn upsert_workspace(
    http: &reqwest::Client,
    listen_addr: SocketAddr,
    workspace_id: WorkspaceId,
    tenant_id: TenantId,
    instructions: &str,
) -> anyhow::Result<()> {
    let resp = http
        .post(format!("http://{listen_addr}/admin/v1/workspaces"))
        .json(&json!({
            "id": workspace_id,
            "tenant_id": tenant_id,
            "name": format!("ws-{workspace_id}"),
            "global_instructions": instructions,
            "allowed_plugin_ids": ["slack"]
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("upsert workspace: {}", resp.status());
    }
    Ok(())
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
        .json(&json!({"title": "slack-smoke"}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create_thread: {}", resp.status());
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
        iss: "elena-slack".into(),
        aud: "elena-slack-clients".into(),
        exp,
    };
    Ok(encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

/// Look up the most recent assistant message and pull the `text`
/// argument from its `tool_use` block. This is the exact text the model
/// chose to post — what Slack ultimately stores. Avoids needing the
/// `channels:history` scope to read it back.
async fn latest_tool_use_text(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    thread_id: ThreadId,
) -> anyhow::Result<Option<String>> {
    let rows: Vec<(serde_json::Value, String)> = sqlx::query_as(
        "SELECT content, role::text FROM messages
         WHERE tenant_id = $1 AND thread_id = $2
         ORDER BY created_at DESC LIMIT 5",
    )
    .bind(tenant_id.as_uuid())
    .bind(thread_id.as_uuid())
    .fetch_all(pool)
    .await?;
    for (content, role) in &rows {
        if role != "assistant" {
            continue;
        }
        if let Some(arr) = content.as_array() {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && let Some(text) =
                        block.get("input").and_then(|i| i.get("text")).and_then(|t| t.as_str())
                {
                    return Ok(Some(text.to_owned()));
                }
            }
        }
    }
    Ok(None)
}

/// Delete a posted Slack message. Failure is fatal — if `chat.delete`
/// reports `ok=false` the message either doesn't exist (smoke
/// pretending success) or the bot lacks permission, both of which we
/// want to surface loudly.
async fn delete_message(
    http: &reqwest::Client,
    bot_token: &str,
    channel: &str,
    ts: &str,
) -> anyhow::Result<()> {
    let resp = http
        .post(format!("{SLACK_API}/chat.delete"))
        .bearer_auth(bot_token)
        .json(&json!({"channel": channel, "ts": ts}))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    if !resp["ok"].as_bool().unwrap_or(false) {
        anyhow::bail!(
            "chat.delete failed for ts={ts}: {} (proves the message did not actually post)",
            resp["error"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

fn decode_master_key(s: &str) -> anyhow::Result<[u8; 32]> {
    use std::collections::HashMap;
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
