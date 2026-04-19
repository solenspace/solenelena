//! Phase 3 smoke binary — runs the full agentic loop end-to-end against the
//! real Anthropic Messages API with a real tool call.
//!
//! Reads `ANTHROPIC_API_KEY` from the environment. If unset, exits 2 with a
//! clear skip message (so CI can skip without failing). On a clean
//! round-trip (model invokes `reverse_text`, receives the result, composes
//! a final answer) it exits 0.
//!
//! Spins up throwaway Postgres (with pgvector) and Redis via testcontainers
//! so no external infra is required.
//!
//! Run with:
//! ```bash
//! ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase3-smoke
//! ```

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound
)]

use std::{collections::HashMap, io::Write as _, process::ExitCode, sync::Arc};

use async_trait::async_trait;
use elena_config::{
    CacheConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig, PostgresConfig, RedisConfig,
};
use elena_core::{LoopState, run_loop};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_store::{Store, TenantRecord};
use elena_tools::{Tool, ToolContext, ToolOutput, ToolRegistry};
use elena_types::{
    BudgetLimits, ContentBlock, Message, ModelId, PermissionSet, SessionId, StreamEvent,
    TenantContext, TenantId, TenantTier, Terminal, ThreadId, ToolError, ToolResultContent, UserId,
    WorkspaceId,
};
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::{Value, json};
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
            "elena-phase3-smoke: SKIP — ANTHROPIC_API_KEY is not set.\n\
             To run the smoke against the real API:\n    \
             ANTHROPIC_API_KEY=sk-ant-... cargo run -p elena-phase3-smoke"
        );
        return ExitCode::from(2);
    };

    match run(key).await {
        Ok(()) => {
            println!("\nelena-phase3-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("\nelena-phase3-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(api_key: String) -> anyhow::Result<()> {
    let model = std::env::var("ELENA_SMOKE_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned());

    eprintln!("elena-phase3-smoke: spinning throwaway Postgres + Redis…");
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
        context: elena_config::ContextConfig::default(),
        router: elena_config::RouterConfig::default(),
        providers: elena_config::ProvidersConfig::default(),
        rate_limits: elena_config::RateLimitsConfig::default(),
    };

    let store = Store::connect(&cfg).await?;
    store.run_migrations().await?;
    let store = Arc::new(store);

    let tenant_id = TenantId::new();
    let tenant = TenantContext {
        tenant_id,
        user_id: UserId::new(),
        workspace_id: WorkspaceId::new(),
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
            name: "phase3-smoke".into(),
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
    let llm =
        AnthropicClient::new(&anthropic_cfg, AnthropicAuth::ApiKey(SecretString::from(api_key)))?;

    let registry = ToolRegistry::new();
    registry.register(ReverseTextTool::new());

    let embedder: Arc<dyn elena_context::Embedder> = Arc::new(elena_context::NullEmbedder);
    let context = Arc::new(elena_context::ContextManager::new(
        Arc::clone(&embedder),
        elena_context::ContextManagerOptions::default(),
    ));
    let episode_store = Arc::new(store.episodes.clone());
    let memory = Arc::new(elena_memory::EpisodicMemory::new(episode_store));
    let router = Arc::new(elena_router::ModelRouter::new(elena_config::TierModels::default(), 2));

    let plugins = Arc::new(elena_plugins::PluginRegistry::empty(registry.clone()));
    let mut mux = elena_llm::LlmMultiplexer::new("anthropic");
    mux.register("anthropic", Arc::new(llm) as Arc<dyn elena_llm::LlmClient>);
    let deps = Arc::new(elena_core::LoopDeps {
        store: store.clone(),
        llm: Arc::new(mux),
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools: registry,
        context,
        memory,
        router,
        plugins,
        rate_limits: Arc::new(elena_config::RateLimitsConfig::default()),
        metrics: Arc::new(elena_observability::LoopMetrics::new()?),
        defaults: Arc::new(DefaultsConfig::default()),
    });

    // Seed thread + user message.
    let thread_id = store
        .threads
        .create_thread(tenant_id, tenant.user_id, tenant.workspace_id, Some("smoke"))
        .await?;
    let user_msg = Message::user_text(
        tenant_id,
        thread_id,
        "Use the `reverse_text` tool to reverse the string \"hello elena\", \
         then tell me the reversed result in one short sentence.",
    );
    store.threads.append_message(&user_msg).await?;

    let mut initial_tenant = tenant.clone();
    initial_tenant.thread_id = thread_id;
    let mut state = LoopState::new(thread_id, initial_tenant, ModelId::new(model), 5, 512);
    state.system_prompt = vec![ContentBlock::Text {
        text: "You are a helpful assistant that uses the `reverse_text` tool when asked to \
               reverse strings. Keep answers short."
            .into(),
        cache_control: None,
    }];

    println!("Prompt: \"Use reverse_text to reverse 'hello elena'.\"\n");
    println!("Events:");

    let cancel = CancellationToken::new();
    let (handle, mut stream) = run_loop(state, deps, "smoke-worker".into(), cancel);

    let mut got_text = false;
    let mut tool_called = false;
    let mut terminal: Option<Terminal> = None;

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta } => {
                got_text = true;
                print!("{delta}");
                std::io::stdout().flush().ok();
            }
            StreamEvent::ToolUseComplete { id, name, input } => {
                eprintln!("\n[tool_use] {name} {input} (id={id})");
                if name == "reverse_text" {
                    tool_called = true;
                }
            }
            StreamEvent::ToolResult { id, is_error } => {
                eprintln!("[tool_result] id={id} is_error={is_error}");
            }
            StreamEvent::PhaseChange { from, to } => {
                eprintln!("[phase] {from} → {to}");
            }
            StreamEvent::Done(t) => {
                terminal = Some(t);
                break;
            }
            StreamEvent::Error(err) => {
                anyhow::bail!("stream error: {err}");
            }
            _ => {}
        }
    }

    let returned = handle.await?;
    eprintln!("\n[driver-terminal] {returned:?}");

    match terminal {
        Some(Terminal::Completed) if got_text && tool_called => Ok(()),
        Some(Terminal::Completed) => {
            anyhow::bail!(
                "completed but expected tool_call+text (tool_called={tool_called}, got_text={got_text})"
            )
        }
        Some(t) => anyhow::bail!("non-success terminal: {t:?}"),
        None => anyhow::bail!("stream ended without terminal event"),
    }
}

#[derive(Debug, Clone)]
struct ReverseTextTool {
    description: String,
    schema: Value,
}

impl ReverseTextTool {
    fn new() -> Self {
        Self {
            description: "Reverse the characters of a string. Returns the reversed form.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The string to reverse."
                    }
                },
                "required": ["text"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ReverseTextTool {
    fn name(&self) -> &str {
        "reverse_text"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> &Value {
        &self.schema
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let text = input["text"].as_str().unwrap_or_default();
        let reversed: String = text.chars().rev().collect();
        Ok(ToolOutput { content: ToolResultContent::text(reversed), is_error: false })
    }
}
