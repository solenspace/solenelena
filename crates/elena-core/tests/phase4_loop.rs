//! Phase-4 agentic-loop integration tests.
//!
//! Spins throwaway Postgres + Redis via testcontainers and mocks Anthropic
//! with wiremock. Exercises retrieval, routing / cascade escalation, and
//! episodic memory end-to-end.

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![cfg(test)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::unnecessary_literal_bound,
    clippy::field_reassign_with_default,
    clippy::doc_markdown
)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RedisConfig, RouterConfig, TierModels,
};
use elena_context::EpisodicMemory;
use elena_context::{ContextManager, ContextManagerOptions, Embedder, FakeEmbedder};
use elena_core::{LoopState, run_loop};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_router::ModelRouter;
use elena_store::{Episode, Store, TenantRecord};
use elena_tools::ToolRegistry;
use elena_types::{
    BudgetLimits, EpisodeId, Message, ModelId, Outcome, PermissionSet, SessionId, StreamEvent,
    TenantContext, TenantId, TenantTier, Terminal, ThreadId, UserId, WorkspaceId,
};
use futures::StreamExt;
use secrecy::SecretString;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

// ---------------------------------------------------------------------------
//  Harness
// ---------------------------------------------------------------------------

struct Harness {
    pg: ContainerAsync<GenericImage>,
    redis: ContainerAsync<GenericImage>,
    mock: MockServer,
    store: Arc<Store>,
    deps: Arc<elena_core::LoopDeps>,
    tenant: TenantContext,
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Synchronous best-effort cleanup. The testcontainers crate's Drop
        // spawns an async task that often does not complete before the
        // test process exits, leaking containers across runs.
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f"])
            .arg(self.pg.id())
            .arg(self.redis.id())
            .output();
    }
}

async fn harness(embedder: Arc<dyn Embedder>) -> Harness {
    let pg = GenericImage::new("pgvector/pgvector", "pg16")
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
        .with_env_var("POSTGRES_PASSWORD", "elena")
        .with_env_var("POSTGRES_USER", "elena")
        .with_env_var("POSTGRES_DB", "elena")
        .start()
        .await
        .expect("pg");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");

    let redis = GenericImage::new("redis", "7-alpine")
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .start()
        .await
        .expect("redis");
    let redis_port = redis.get_host_port_ipv4(6379).await.expect("redis port");

    let mock = MockServer::start().await;

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
            thread_claim_ttl_ms: 30_000,
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

    let store = Store::connect(&cfg).await.expect("connect");
    store.run_migrations().await.expect("migrations");

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
        plan: None,
        metadata: HashMap::new(),
    };
    store
        .tenants
        .upsert_tenant(&TenantRecord {
            id: tenant_id,
            name: "phase4-test".into(),
            tier: TenantTier::Pro,
            budget: BudgetLimits::DEFAULT_PRO,
            permissions: PermissionSet::default(),
            metadata: HashMap::new(),
            allowed_plugin_ids: vec![],
            app_id: None,
            deleted_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .expect("upsert");

    let anthropic_cfg = elena_config::AnthropicConfig {
        api_key: SecretString::from("sk-test"),
        base_url: mock.uri(),
        api_version: "2023-06-01".into(),
        request_timeout_ms: Some(10_000),
        connect_timeout_ms: 2_000,
        max_attempts: 2,
        pool_max_idle_per_host: 64,
    };
    let llm =
        AnthropicClient::new(&anthropic_cfg, AnthropicAuth::ApiKey(anthropic_cfg.api_key.clone()))
            .expect("llm");

    let store_arc = Arc::new(store);
    let context = Arc::new(ContextManager::new(
        Arc::clone(&embedder),
        ContextManagerOptions {
            recency_window: 10,
            retrieval_top_k: 5,
            default_budget_tokens: 10_000,
        },
    ));
    let episode_store = Arc::new(store_arc.episodes.clone());
    let memory = Arc::new(EpisodicMemory::new(episode_store));
    let router = Arc::new(ModelRouter::new(TierModels::default(), 2));

    let tools = ToolRegistry::new();
    let plugins = Arc::new(elena_plugins::PluginRegistry::empty(tools.clone()));
    let mut mux = elena_llm::LlmMultiplexer::new("anthropic");
    mux.register("anthropic", Arc::new(llm) as Arc<dyn elena_llm::LlmClient>);
    let deps = Arc::new(elena_core::LoopDeps {
        store: store_arc.clone(),
        llm: Arc::new(mux),
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools,
        context,
        memory,
        router,
        plugins,
        rate_limits: Arc::new(elena_config::RateLimitsConfig::default()),
        metrics: Arc::new(elena_observability::LoopMetrics::new().expect("metrics")),
        defaults: Arc::new(DefaultsConfig::default()),
    });

    Harness { pg, redis, mock, store: store_arc, deps, tenant }
}

// ---------------------------------------------------------------------------
//  SSE transcript helper
// ---------------------------------------------------------------------------

fn usage_json(input: u64, output: u64) -> String {
    format!(
        r#"{{"input_tokens":{input},"output_tokens":{output},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0}},"server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}}"#
    )
}

fn text_only_sse(msg_id: &str, text: &str) -> String {
    let u = usage_json(10, 0);
    let u2 = usage_json(10, 4);
    format!(
        r#"event: message_start
data: {{"type":"message_start","message":{{"id":"{msg_id}","role":"assistant","model":"claude-haiku-4-5-20251001","usage":{u}}}}}

event: content_block_start
data: {{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":""}}}}

event: content_block_delta
data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}

event: content_block_stop
data: {{"type":"content_block_stop","index":0}}

event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{u2}}}

event: message_stop
data: {{"type":"message_stop"}}

"#
    )
}

async fn seed_thread_async(h: &Harness, text: &str) -> ThreadId {
    let thread_id = h
        .store
        .threads
        .create_thread(h.tenant.tenant_id, h.tenant.user_id, h.tenant.workspace_id, Some("phase4"))
        .await
        .expect("create thread");
    let msg = Message::user_text(h.tenant.tenant_id, thread_id, text);
    h.store.threads.append_message(&msg).await.expect("append user");
    h.deps
        .context
        .embed_and_store(h.tenant.tenant_id, msg.id, text, &h.store.threads)
        .await
        .expect("embed user");
    thread_id
}

fn initial_state(h: &Harness, thread_id: ThreadId) -> LoopState {
    let mut tenant = h.tenant.clone();
    tenant.thread_id = thread_id;
    LoopState::new(thread_id, tenant, ModelId::new("placeholder"), 5, 512)
}

async fn collect_until_done(
    mut stream: tokio_stream::wrappers::ReceiverStream<StreamEvent>,
) -> (Vec<StreamEvent>, Option<Terminal>) {
    let mut events = Vec::new();
    let mut terminal = None;
    while let Some(ev) = stream.next().await {
        if let StreamEvent::Done(t) = &ev {
            terminal = Some(t.clone());
            events.push(ev);
            break;
        }
        events.push(ev);
    }
    (events, terminal)
}

// ---------------------------------------------------------------------------
//  Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrieval_surfaces_semantically_matching_old_message() {
    let h = harness(Arc::new(FakeEmbedder) as Arc<dyn Embedder>).await;

    // Seed a thread with an old distinctive message + a recent unrelated
    // "user" turn.
    let thread_id = seed_thread_async(&h, "My favorite color is verdigris.").await;
    // A few fillers.
    for t in ["random filler one", "random filler two", "random filler three"] {
        let msg = Message::user_text(h.tenant.tenant_id, thread_id, t);
        h.store.threads.append_message(&msg).await.unwrap();
        h.deps
            .context
            .embed_and_store(h.tenant.tenant_id, msg.id, t, &h.store.threads)
            .await
            .unwrap();
    }
    // Then the real user query.
    let query = "What color did I mention earlier? Recall verdigris.";
    let msg = Message::user_text(h.tenant.tenant_id, thread_id, query);
    h.store.threads.append_message(&msg).await.unwrap();
    h.deps
        .context
        .embed_and_store(h.tenant.tenant_id, msg.id, query, &h.store.threads)
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(text_only_sse("m1", "Verdigris.").into_bytes(), "text/event-stream"),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id);
    let (handle, stream) = run_loop(state, h.deps.clone(), "w".into(), CancellationToken::new());
    let (_events, terminal) = collect_until_done(stream).await;
    let _ = handle.await;
    assert_eq!(terminal, Some(Terminal::Completed));

    // Verify the first recorded request body included the verdigris message
    // in its context window.
    let received = h.mock.received_requests().await.unwrap();
    assert!(!received.is_empty(), "mock saw no requests");
    let body_text = std::str::from_utf8(&received[0].body).unwrap();
    assert!(
        body_text.contains("verdigris"),
        "request body missing the retrieved message: {body_text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn router_cascades_on_refusal_and_retries_at_higher_tier() {
    let h = harness(Arc::new(FakeEmbedder) as Arc<dyn Embedder>).await;
    // Short simple query → router picks `Fast`, leaving room to escalate on
    // cascade. A prompt that hit the "analyze" keyword would already route
    // to `Premium` and `cascade_check` couldn't go higher.
    let thread_id = seed_thread_async(&h, "hi").await;

    // First call: refusal (triggers cascade).
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("r1", "I cannot do that.").into_bytes(),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount(&h.mock)
        .await;

    // Second call: real answer.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("r2", "Here is the analysis.").into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id);
    let (handle, stream) = run_loop(state, h.deps.clone(), "w".into(), CancellationToken::new());
    let (_events, terminal) = collect_until_done(stream).await;
    let _ = handle.await;
    assert_eq!(terminal, Some(Terminal::Completed));

    // Exactly two LLM calls — one refusal + one escalated response.
    let requests = h.mock.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected one cascade re-issue");

    // Only one assistant message committed (the accepted one).
    let msgs =
        h.store.threads.list_messages(h.tenant.tenant_id, thread_id, 50, None).await.unwrap();
    let assistant_count =
        msgs.iter().filter(|m| matches!(m.role, elena_types::Role::Assistant)).count();
    assert_eq!(assistant_count, 1, "refused turn must not persist");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completing_a_thread_records_an_episode() {
    let h = harness(Arc::new(FakeEmbedder) as Arc<dyn Embedder>).await;
    let thread_id = seed_thread_async(&h, "Help me summarize my week.").await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("e1", "Here's a summary.").into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id);
    let (handle, stream) = run_loop(state, h.deps.clone(), "w".into(), CancellationToken::new());
    let (_, terminal) = collect_until_done(stream).await;
    let _ = handle.await;
    assert_eq!(terminal, Some(Terminal::Completed));

    // Give the fire-and-forget recorder a moment to land.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let eps = h
        .store
        .episodes
        .recent_episodes(h.tenant.tenant_id, h.tenant.workspace_id, 5)
        .await
        .unwrap();
    assert_eq!(eps.len(), 1, "expected one recorded episode");
    assert!(eps[0].task_summary.contains("summarize my week"));
    assert!(matches!(eps[0].outcome, Outcome::Completed));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_injects_prior_episode_into_system_prompt() {
    let h = harness(Arc::new(FakeEmbedder) as Arc<dyn Embedder>).await;

    // Pre-seed an episode in the workspace so recall has something to find.
    let embedding = FakeEmbedder.embed("Help me plan a trip to Tokyo.").await.unwrap();
    let ep = Episode {
        id: EpisodeId::new(),
        tenant_id: h.tenant.tenant_id,
        workspace_id: h.tenant.workspace_id,
        task_summary: "Help me plan a trip to Tokyo.".into(),
        actions: vec![],
        outcome: Outcome::Completed,
        created_at: chrono::Utc::now(),
    };
    h.store.episodes.insert_episode(&ep, &embedding).await.unwrap();

    // Now start a new thread with a similar-themed user query.
    let thread_id = seed_thread_async(&h, "Help me plan a trip to Tokyo, please.").await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("inj", "Let me help.").into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id);
    let (handle, stream) = run_loop(state, h.deps.clone(), "w".into(), CancellationToken::new());
    let (_, terminal) = collect_until_done(stream).await;
    let _ = handle.await;
    assert_eq!(terminal, Some(Terminal::Completed));

    let requests = h.mock.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    let body = std::str::from_utf8(&requests[0].body).unwrap();
    assert!(
        body.contains("Relevant prior sessions in this workspace")
            && body.contains("Help me plan a trip to Tokyo"),
        "system prompt should carry the recalled episode summary"
    );
}
