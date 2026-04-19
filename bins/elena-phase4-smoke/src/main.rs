//! Phase-4 smoke binary — runs a two-turn conversation through the full
//! agentic loop, exercising retrieval + memory + routing end-to-end against
//! the real Anthropic Messages API.
//!
//! Environment:
//! - `ANTHROPIC_API_KEY` (required) — without it, exits 2 with a skip
//!   message so CI can gate cleanly.
//! - `ELENA_EMBEDDING_MODEL_PATH` + `ELENA_TOKENIZER_PATH` (optional) —
//!   when both are set, the binary loads [`OnnxEmbedder`] so retrieval is
//!   fully active. When either is unset, a `NullEmbedder` is installed
//!   and the binary announces "retrieval disabled" mode (the loop still
//!   runs, just without semantic context lookup).
//! - `ELENA_SMOKE_MODEL` (optional) — override the default tier `Fast`
//!   model id.
//!
//! Spins up throwaway Postgres (with pgvector) + Redis via testcontainers.
//!
//! Run with:
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase4-smoke
//! ```

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound
)]

use std::{collections::HashMap, io::Write as _, path::PathBuf, process::ExitCode, sync::Arc};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RedisConfig, RouterConfig, TierModels,
};
use elena_context::{ContextManager, ContextManagerOptions, Embedder, NullEmbedder, OnnxEmbedder};
use elena_core::{LoopState, run_loop};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_memory::EpisodicMemory;
use elena_router::ModelRouter;
use elena_store::{Store, TenantRecord};
use elena_tools::ToolRegistry;
use elena_types::{
    BudgetLimits, ContentBlock, Message, ModelId, PermissionSet, SessionId, StreamEvent,
    TenantContext, TenantId, TenantTier, Terminal, ThreadId, UserId, WorkspaceId,
};
use futures::StreamExt;
use secrecy::SecretString;
use testcontainers::{GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!(
            "elena-phase4-smoke: SKIP — ANTHROPIC_API_KEY is not set.\n\
             To run the smoke against the real API:\n    \
             ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase4-smoke"
        );
        return ExitCode::from(2);
    };

    match run(key).await {
        Ok(()) => {
            println!("\nelena-phase4-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-phase4-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(api_key: String) -> anyhow::Result<()> {
    eprintln!("elena-phase4-smoke: spinning throwaway Postgres + Redis…");
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
    let workspace_id = WorkspaceId::new();
    let user_id = UserId::new();
    let tenant = TenantContext {
        tenant_id,
        user_id,
        workspace_id,
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        permissions: PermissionSet::default(),
        budget: BudgetLimits::DEFAULT_PRO,
        tier: TenantTier::Pro,
        metadata: HashMap::new(),
    };
    store
        .tenants
        .upsert_tenant(&TenantRecord {
            id: tenant_id,
            name: "phase4-smoke".into(),
            tier: TenantTier::Pro,
            budget: BudgetLimits::DEFAULT_PRO,
            permissions: PermissionSet::default(),
            metadata: HashMap::new(),
            allowed_plugin_ids: vec![],
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await?;

    let anthropic_cfg = elena_config::AnthropicConfig {
        api_key: SecretString::from(api_key.clone()),
        base_url: std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_owned()),
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

    // Embedder: real ONNX if both env vars are set, else Null.
    let embedder = load_embedder();
    println!(
        "elena-phase4-smoke: retrieval {}.",
        if embedder.dimension() > 0 {
            "ENABLED"
        } else {
            "DISABLED (set ELENA_EMBEDDING_MODEL_PATH + ELENA_TOKENIZER_PATH to enable)"
        }
    );

    let context =
        Arc::new(ContextManager::new(Arc::clone(&embedder), ContextManagerOptions::default()));
    let episode_store = Arc::new(store.episodes.clone());
    let memory = Arc::new(EpisodicMemory::new(episode_store));
    let router = Arc::new(ModelRouter::new(TierModels::default(), 2));

    let tools = ToolRegistry::new();
    let plugins = Arc::new(elena_plugins::PluginRegistry::empty(tools.clone()));
    let deps = Arc::new(elena_core::LoopDeps {
        store: store.clone(),
        llm,
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools,
        context: Arc::clone(&context),
        memory,
        router,
        plugins,
        rate_limits: Arc::new(elena_config::RateLimitsConfig::default()),
        metrics: Arc::new(elena_observability::LoopMetrics::new()?),
        defaults: Arc::new(DefaultsConfig::default()),
    });

    // Seed thread with the distinctive turn 1 statement.
    let thread_id =
        store.threads.create_thread(tenant_id, user_id, workspace_id, Some("phase4-smoke")).await?;
    let model_override =
        std::env::var("ELENA_SMOKE_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".into());

    let turn1_text = "My favorite color is verdigris. Please acknowledge in one short sentence.";
    println!("\nTurn 1 prompt: \"{turn1_text}\"");
    let turn1_msg = Message::user_text(tenant_id, thread_id, turn1_text);
    store.threads.append_message(&turn1_msg).await?;
    context.embed_and_store(tenant_id, turn1_msg.id, turn1_text, &store.threads).await.ok();

    run_single_turn(&deps, &tenant, thread_id, &model_override, "turn-1").await?;

    // Turn 2: query about the color. Retrieval should pull turn 1's text.
    let turn2_text = "What color did I mention earlier? Keep it to a single word.";
    println!("\nTurn 2 prompt: \"{turn2_text}\"");
    let turn2_msg = Message::user_text(tenant_id, thread_id, turn2_text);
    store.threads.append_message(&turn2_msg).await?;
    context.embed_and_store(tenant_id, turn2_msg.id, turn2_text, &store.threads).await.ok();

    let turn2_output =
        run_single_turn(&deps, &tenant, thread_id, &model_override, "turn-2").await?;

    let verdigris_found = turn2_output.to_lowercase().contains("verdigris");
    if context.retrieval_enabled() && !verdigris_found {
        anyhow::bail!(
            "Turn 2 did not surface 'verdigris'. Retrieval enabled but memory didn't \
             survive: {turn2_output:?}"
        );
    }
    println!(
        "\nRetrieval check: {} (turn 2 {} 'verdigris')",
        if verdigris_found { "OK" } else { "N/A" },
        if verdigris_found { "mentioned" } else { "did not mention" }
    );
    Ok(())
}

fn load_embedder() -> Arc<dyn Embedder> {
    let model_path = std::env::var_os("ELENA_EMBEDDING_MODEL_PATH").map(PathBuf::from);
    let tokenizer_path = std::env::var_os("ELENA_TOKENIZER_PATH").map(PathBuf::from);
    match (model_path, tokenizer_path) {
        (Some(mp), Some(tp)) => match OnnxEmbedder::load(&mp, &tp) {
            Ok(e) => Arc::new(e),
            Err(err) => {
                eprintln!(
                    "elena-phase4-smoke: OnnxEmbedder load failed ({err}); disabling retrieval"
                );
                Arc::new(NullEmbedder)
            }
        },
        _ => Arc::new(NullEmbedder),
    }
}

async fn run_single_turn(
    deps: &Arc<elena_core::LoopDeps>,
    tenant: &TenantContext,
    thread_id: ThreadId,
    model_override: &str,
    label: &str,
) -> anyhow::Result<String> {
    let mut state_tenant = tenant.clone();
    state_tenant.thread_id = thread_id;
    let state = LoopState::new(thread_id, state_tenant, ModelId::new(model_override), 5, 512);

    let (handle, mut stream) =
        run_loop(state, Arc::clone(deps), format!("smoke-{label}"), CancellationToken::new());

    let mut text = String::new();
    let mut terminal: Option<Terminal> = None;
    print!("Reply: ");
    std::io::stdout().flush().ok();
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta } => {
                text.push_str(&delta);
                print!("{delta}");
                std::io::stdout().flush().ok();
            }
            StreamEvent::PhaseChange { from, to } => {
                eprintln!("\n[phase {label}] {from} → {to}");
            }
            StreamEvent::Done(t) => {
                terminal = Some(t);
                break;
            }
            StreamEvent::Error(err) => anyhow::bail!("stream error: {err}"),
            _ => {}
        }
    }
    let _ = handle.await;

    match terminal {
        Some(Terminal::Completed) => Ok(text),
        Some(t) => anyhow::bail!("non-success terminal: {t:?}"),
        None => anyhow::bail!("stream ended without terminal"),
    }
}

// Keep `ContentBlock` referenced so `use` churn doesn't prune it — it lives
// in every real LLM turn's request body.
#[allow(dead_code)]
fn _phantom_block() -> ContentBlock {
    ContentBlock::Text { text: "phantom".into(), cache_control: None }
}
