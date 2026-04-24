//! End-to-end agentic-loop round-trip tests.
//!
//! Spins up throwaway Postgres (with pgvector) and Redis via testcontainers;
//! mocks Anthropic with wiremock. Exercises the full
//! Received → Streaming → ExecutingTools → PostProcessing → Completed
//! cycle against a real store and a fake LLM.
//!
//! Run serially — each test creates a fresh harness, and Docker's socket
//! doesn't love parallel test-container spin-ups.

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
    clippy::doc_markdown,
    clippy::unnested_or_patterns
)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use elena_config::{
    CacheConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig, PostgresConfig,
    RedisConfig, TierModels,
};
use elena_context::EpisodicMemory;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_core::{LoopState, run_loop};
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_router::ModelRouter;
use elena_store::{Store, TenantRecord};
use elena_tools::{Tool, ToolContext, ToolOutput, ToolRegistry};
use elena_types::{
    BudgetLimits, ModelId, PermissionSet, SessionId, StreamEvent, TenantContext, TenantId,
    TenantTier, Terminal, ThreadId, ToolCallId, ToolError, ToolResultContent, UserId, WorkspaceId,
};
use futures::StreamExt;
use secrecy::SecretString;
use serde_json::{Value, json};
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
    _pg: ContainerAsync<GenericImage>,
    _redis: ContainerAsync<GenericImage>,
    mock: MockServer,
    store: Arc<Store>,
    deps: Arc<elena_core::LoopDeps>,
    tenant: TenantContext,
}

async fn start_harness() -> Harness {
    start_harness_with_budget(BudgetLimits::DEFAULT_PRO, 20).await
}

async fn start_harness_with_budget(budget: BudgetLimits, max_turns: u32) -> Harness {
    let pg = GenericImage::new("pgvector/pgvector", "pg16")
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
        .with_env_var("POSTGRES_PASSWORD", "elena")
        .with_env_var("POSTGRES_USER", "elena")
        .with_env_var("POSTGRES_DB", "elena")
        .start()
        .await
        .expect("start postgres");
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");

    let redis = GenericImage::new("redis", "7-alpine")
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .start()
        .await
        .expect("start redis");
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
        context: elena_config::ContextConfig::default(),
        router: elena_config::RouterConfig::default(),
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
        budget,
        tier: TenantTier::Pro,
        plan: None,
        metadata: HashMap::new(),
    };
    let record = TenantRecord {
        id: tenant_id,
        name: "round-trip-test".into(),
        tier: TenantTier::Pro,
        budget,
        permissions: PermissionSet::default(),
        metadata: HashMap::new(),
        allowed_plugin_ids: vec![],
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    store.tenants.upsert_tenant(&record).await.expect("upsert");

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
            .expect("anthropic client");

    let registry = ToolRegistry::new();
    registry.register(EchoTool::new());

    let mut defaults = DefaultsConfig::default();
    defaults.default_max_turns = max_turns;

    let store_arc = Arc::new(store);
    let embedder: Arc<dyn elena_context::Embedder> = Arc::new(NullEmbedder);
    let context_manager =
        Arc::new(ContextManager::new(Arc::clone(&embedder), ContextManagerOptions::default()));
    let episode_store = Arc::new(store_arc.episodes.clone());
    let memory = Arc::new(EpisodicMemory::new(episode_store));
    let router = Arc::new(ModelRouter::new(TierModels::default(), 2));

    let plugins = Arc::new(elena_plugins::PluginRegistry::empty(registry.clone()));
    let mut mux = elena_llm::LlmMultiplexer::new("anthropic");
    mux.register("anthropic", Arc::new(llm) as Arc<dyn elena_llm::LlmClient>);
    let deps = Arc::new(elena_core::LoopDeps {
        store: store_arc.clone(),
        llm: Arc::new(mux),
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools: registry,
        context: context_manager,
        memory,
        router,
        plugins,
        rate_limits: Arc::new(elena_config::RateLimitsConfig::default()),
        metrics: Arc::new(elena_observability::LoopMetrics::new().expect("metrics")),
        defaults: Arc::new(defaults),
    });

    Harness { _pg: pg, _redis: redis, mock, store: store_arc, deps, tenant }
}

// ---------------------------------------------------------------------------
//  Test tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EchoTool {
    schema: Value,
    description: String,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            description: "Echo back whatever text you give it.".into(),
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
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
        let text = input["text"].as_str().unwrap_or("<missing>").to_owned();
        Ok(ToolOutput {
            content: ToolResultContent::text(format!("echo: {text}")),
            is_error: false,
        })
    }
}

// ---------------------------------------------------------------------------
//  SSE transcript builders
// ---------------------------------------------------------------------------

fn sse(events: &[&str]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str(e);
        out.push_str("\n\n");
    }
    out
}

fn usage_json(input_tokens: u64, output_tokens: u64) -> String {
    format!(
        r#"{{"input_tokens":{input_tokens},"output_tokens":{output_tokens},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0}},"server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}}"#
    )
}

/// A text-only assistant message.
fn text_only_sse(msg_id: &str, text: &str, input_tokens: u64, output_tokens: u64) -> String {
    let u = usage_json(input_tokens, 0);
    let u2 = usage_json(input_tokens, output_tokens);
    sse(&[
        &format!(
            r#"event: message_start
data: {{"type":"message_start","message":{{"id":"{msg_id}","role":"assistant","model":"claude-haiku-4-5-20251001","usage":{u}}}}}"#
        ),
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        &format!(
            r#"event: content_block_delta
data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
        ),
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#,
        &format!(
            r#"event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{u2}}}"#
        ),
        r#"event: message_stop
data: {"type":"message_stop"}"#,
    ])
}

/// An assistant message containing N parallel tool_use blocks, each calling `tool`.
fn tool_use_sse(
    msg_id: &str,
    tool: &str,
    tool_call_ids: &[ToolCallId],
    tool_inputs: &[Value],
    input_tokens: u64,
    output_tokens: u64,
) -> String {
    assert_eq!(tool_call_ids.len(), tool_inputs.len());
    let u = usage_json(input_tokens, 0);
    let u2 = usage_json(input_tokens, output_tokens);

    let mut parts = vec![format!(
        r#"event: message_start
data: {{"type":"message_start","message":{{"id":"{msg_id}","role":"assistant","model":"claude-haiku-4-5-20251001","usage":{u}}}}}"#
    )];

    for (i, (call_id, input)) in tool_call_ids.iter().zip(tool_inputs).enumerate() {
        let input_str = serde_json::to_string(input).unwrap();
        parts.push(format!(
            r#"event: content_block_start
data: {{"type":"content_block_start","index":{i},"content_block":{{"type":"tool_use","id":"{call_id}","name":"{tool}","input":{{}}}}}}"#
        ));
        parts.push(format!(
            r#"event: content_block_delta
data: {{"type":"content_block_delta","index":{i},"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
            serde_json::to_string(&input_str).unwrap()
        ));
        parts.push(format!(
            r#"event: content_block_stop
data: {{"type":"content_block_stop","index":{i}}}"#
        ));
    }

    parts.push(format!(
        r#"event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":"tool_use"}},"usage":{u2}}}"#
    ));
    parts.push(
        r#"event: message_stop
data: {"type":"message_stop"}"#
            .to_owned(),
    );

    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    sse(&refs)
}

fn mount_stream(_server: &MockServer, body: String) -> Mock {
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(body.into_bytes(), "text/event-stream"),
    )
}

// ---------------------------------------------------------------------------
//  Shared helpers
// ---------------------------------------------------------------------------

/// Create a thread and append an initial user message. Returns the thread id.
async fn seed_thread(h: &Harness, text: &str) -> ThreadId {
    let thread_id = h
        .store
        .threads
        .create_thread(
            h.tenant.tenant_id,
            h.tenant.user_id,
            h.tenant.workspace_id,
            Some("round-trip"),
        )
        .await
        .expect("create thread");

    let msg = elena_types::Message::user_text(h.tenant.tenant_id, thread_id, text);
    h.store.threads.append_message(&msg).await.expect("append user");
    thread_id
}

fn initial_state(h: &Harness, thread_id: ThreadId, max_turns: u32) -> LoopState {
    let mut tenant = h.tenant.clone();
    tenant.thread_id = thread_id;
    // Phase 7 introduced AutonomyMode (default: Moderate, which pauses on
    // non-read-only tools). These pre-Phase-7 tests expect auto-execution
    // — pin them to Yolo so the loop runs through to completion without
    // an approval round-trip.
    LoopState::new(thread_id, tenant, ModelId::new("claude-haiku-4-5-20251001"), max_turns, 1_024)
        .with_autonomy(elena_types::AutonomyMode::Yolo)
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
async fn text_only_completion_commits_one_assistant_message() {
    let h = start_harness().await;
    let thread_id = seed_thread(&h, "hi there").await;

    mount_stream(&h.mock, text_only_sse("msg_01", "Hello!", 10, 3)).mount(&h.mock).await;

    let state = initial_state(&h, thread_id, 5);
    let cancel = CancellationToken::new();
    let (handle, stream) = run_loop(state, h.deps.clone(), "worker-1".into(), cancel);
    let (_events, terminal) = collect_until_done(stream).await;
    let returned = handle.await.expect("join");

    assert_eq!(terminal, Some(Terminal::Completed));
    assert_eq!(returned, Terminal::Completed);

    let msgs =
        h.store.threads.list_messages(h.tenant.tenant_id, thread_id, 100, None).await.unwrap();
    assert_eq!(msgs.len(), 2, "user + assistant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_tool_use_round_trip_completes_with_four_messages() {
    let h = start_harness().await;
    let thread_id = seed_thread(&h, "say echo hi").await;

    // First LLM call: emits tool_use.
    let tool_id = ToolCallId::new();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    tool_use_sse("msg_01", "echo", &[tool_id], &[json!({"text": "hi"})], 20, 5)
                        .into_bytes(),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount(&h.mock)
        .await;

    // Second LLM call: final text.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("msg_02", "Done!", 30, 4).into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id, 5);
    let (handle, stream) =
        run_loop(state, h.deps.clone(), "worker-1".into(), CancellationToken::new());
    let (events, terminal) = collect_until_done(stream).await;
    let returned = handle.await.expect("join");

    assert_eq!(terminal, Some(Terminal::Completed));
    assert_eq!(returned, Terminal::Completed);

    // Four messages: user → assistant(tool_use) → tool_result → assistant(text).
    let msgs =
        h.store.threads.list_messages(h.tenant.tenant_id, thread_id, 100, None).await.unwrap();
    assert_eq!(msgs.len(), 4, "got: {msgs:#?}");

    // Events must include a ToolResult.
    let saw_tool_result = events.iter().any(|e| matches!(e, StreamEvent::ToolResult { .. }));
    assert!(saw_tool_result, "expected ToolResult event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_tool_calls_all_execute_and_commit() {
    let h = start_harness().await;
    let thread_id = seed_thread(&h, "three echoes").await;

    let tid_a = ToolCallId::new();
    let tid_b = ToolCallId::new();
    let tid_c = ToolCallId::new();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    tool_use_sse(
                        "msg_01",
                        "echo",
                        &[tid_a, tid_b, tid_c],
                        &[json!({"text": "a"}), json!({"text": "b"}), json!({"text": "c"})],
                        15,
                        6,
                    )
                    .into_bytes(),
                    "text/event-stream",
                ),
        )
        .up_to_n_times(1)
        .mount(&h.mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("msg_02", "All done.", 40, 3).into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id, 5);
    let (handle, stream) =
        run_loop(state, h.deps.clone(), "worker-1".into(), CancellationToken::new());
    let (_events, terminal) = collect_until_done(stream).await;
    let returned = handle.await.expect("join");

    assert_eq!(terminal, Some(Terminal::Completed));
    assert_eq!(returned, Terminal::Completed);

    let msgs =
        h.store.threads.list_messages(h.tenant.tenant_id, thread_id, 100, None).await.unwrap();
    // user + assistant(tool_use×3) + 3 tool_result + assistant(text) = 6
    assert_eq!(msgs.len(), 6, "got: {msgs:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_turns_reached_terminates_cleanly() {
    let h = start_harness_with_budget(BudgetLimits::DEFAULT_PRO, 1).await;
    let thread_id = seed_thread(&h, "loop forever").await;

    // Every call returns tool_use — but max_turns=1 caps after the first batch.
    let tid = ToolCallId::new();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    tool_use_sse("msg_loop", "echo", &[tid], &[json!({"text": "again"})], 10, 5)
                        .into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id, 1);
    let (handle, stream) =
        run_loop(state, h.deps.clone(), "worker-1".into(), CancellationToken::new());
    let (_events, terminal) = collect_until_done(stream).await;
    let _returned = handle.await.expect("join");

    match terminal {
        Some(Terminal::MaxTurns { turn_count }) => {
            assert_eq!(turn_count, 1);
        }
        other => panic!("expected MaxTurns, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_exhausted_terminates_with_blocking_limit() {
    // Budget of 5 tokens; mock LLM reports 100 input tokens.
    let tiny = BudgetLimits {
        max_tokens_per_thread: 5,
        max_tokens_per_day: 1_000_000,
        max_threads_concurrent: 5,
        max_tool_calls_per_turn: 16,
    };
    let h = start_harness_with_budget(tiny, 20).await;
    let thread_id = seed_thread(&h, "hi").await;

    mount_stream(&h.mock, text_only_sse("msg_budget", "over", 100, 1)).mount(&h.mock).await;

    let state = initial_state(&h, thread_id, 20);
    let (handle, stream) =
        run_loop(state, h.deps.clone(), "worker-1".into(), CancellationToken::new());
    let (events, terminal) = collect_until_done(stream).await;
    let _returned = handle.await.expect("join");

    assert_eq!(terminal, Some(Terminal::BlockingLimit));

    let saw_budget_err = events
        .iter()
        .any(|e| matches!(e, StreamEvent::Error(elena_types::ElenaError::BudgetExceeded { .. })));
    assert!(saw_budget_err, "expected BudgetExceeded error event");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_mid_stream_terminates_with_aborted() {
    let h = start_harness().await;
    let thread_id = seed_thread(&h, "long request").await;

    // Delay the mock response so cancel fires before we receive anything.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(400))
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    text_only_sse("msg_slow", "never seen", 10, 3).into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id, 5);
    let cancel = CancellationToken::new();
    let (handle, stream) = run_loop(state, h.deps.clone(), "worker-1".into(), cancel.clone());

    // Cancel shortly after kicking off.
    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        cancel.cancel();
    });

    let (_events, terminal) = collect_until_done(stream).await;
    let _returned = handle.await.expect("join");
    cancel_task.await.ok();

    match terminal {
        Some(Terminal::AbortedStreaming) | Some(Terminal::AbortedTools) => {}
        other => panic!("expected Aborted*, got {other:?}"),
    }
}

/// C2.1 — Mid-stream connection drop.
///
/// The mock LLM emits a half-formed SSE body: `message_start` +
/// `content_block_start` + a partial `content_block_delta`, then closes
/// the connection without `message_stop`. The streaming layer should
/// detect end-of-stream-without-terminator and surface the loop with a
/// terminal failure, NOT silently treat the partial delta as a complete
/// turn.
///
/// Acceptance: terminal is non-success and the stream emitted at least
/// one error event. The loop driver maps the underlying SSE error to a
/// `Terminal::ModelError` (or any non-success Terminal); we match
/// loosely because the exact mapping depends on retry/cascade settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c21_truncated_sse_stream_terminates_with_failure() {
    let h = start_harness().await;
    let thread_id = seed_thread(&h, "tell me a story").await;

    let truncated = sse(&[
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_cut","role":"assistant","model":"claude-haiku-4-5-20251001","usage":{"input_tokens":5,"output_tokens":0}}}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        // Partial delta — no message_stop follows. wiremock closes the
        // connection at body-end after this chunk.
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Once upon"}}"#,
    ]);

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(truncated.into_bytes(), "text/event-stream"),
        )
        .mount(&h.mock)
        .await;

    let state = initial_state(&h, thread_id, 5);
    let (handle, stream) =
        run_loop(state, h.deps.clone(), "worker-1".into(), CancellationToken::new());
    let (events, terminal) = collect_until_done(stream).await;
    let _returned = handle.await.expect("join");

    // The terminal must NOT be `Completed` — the stream was cut short.
    let term = terminal.expect("expected a Done event with a terminal");
    assert!(
        !matches!(term, Terminal::Completed),
        "truncated stream must not be reported as Completed; got {term:?}"
    );

    // At least one Error event should have surfaced before the Done.
    let saw_error = events.iter().any(|e| matches!(e, StreamEvent::Error(_)));
    assert!(saw_error, "expected at least one StreamEvent::Error from the truncated stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_from_checkpoint_continues_at_streaming() {
    let h = start_harness().await;
    let thread_id = seed_thread(&h, "hello resumer").await;

    // Pre-seed a checkpoint: phase=Streaming with turn_count=0. The driver
    // should load it, then make exactly one LLM call and complete.
    let mut resumed = initial_state(&h, thread_id, 5);
    resumed.phase = elena_core::LoopPhase::Streaming;
    elena_core::save_loop_state(&h.store.cache, &resumed).await.expect("save checkpoint");

    mount_stream(&h.mock, text_only_sse("msg_resume", "resumed!", 10, 3)).mount(&h.mock).await;

    // Start with a state whose phase is `Received` — if the driver resumes
    // correctly, it will override this with the checkpointed `Streaming`.
    let fresh = initial_state(&h, thread_id, 5);
    let (handle, stream) =
        run_loop(fresh, h.deps.clone(), "worker-resumer".into(), CancellationToken::new());
    let (_events, terminal) = collect_until_done(stream).await;
    let _returned = handle.await.expect("join");

    assert_eq!(terminal, Some(Terminal::Completed));

    let msgs =
        h.store.threads.list_messages(h.tenant.tenant_id, thread_id, 100, None).await.unwrap();
    assert_eq!(msgs.len(), 2, "user + assistant only");

    // Checkpoint should be cleaned up.
    let remains = h.store.cache.load_loop_state(thread_id).await.unwrap();
    assert!(remains.is_none(), "checkpoint should be dropped after clean termination");
}
