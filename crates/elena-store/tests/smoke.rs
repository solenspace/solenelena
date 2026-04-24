//! Integration tests for `elena-store`.
//!
//! Spins up throwaway `postgres:16` and `redis:7` containers via
//! `testcontainers` — no local services required. Run with:
//!
//! ```text
//! cargo test -p elena-store -- --test-threads=1
//! ```
//!
//! `--test-threads=1` avoids container port contention on CI runners with
//! limited Docker socket throughput; not strictly required otherwise.

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use chrono::Utc;
use elena_config::{
    CacheConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig, PostgresConfig, RedisConfig,
};
use elena_store::{Store, TenantRecord};
use elena_types::{
    BudgetLimits, CacheControl, CacheTtl, ContentBlock, Message, MessageId, MessageKind,
    PermissionSet, Role, StopReason, StoreError, TenantId, TenantTier, ThreadId, ToolCallId,
    ToolResultContent, Usage, UserId, WorkspaceId,
};
use secrecy::SecretString;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

struct Harness {
    _pg: ContainerAsync<GenericImage>,
    _redis: ContainerAsync<GenericImage>,
    store: Store,
}

async fn start_harness() -> Harness {
    // Postgres with pgvector.
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
            thread_claim_ttl_ms: 10_000,
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

    Harness { _pg: pg, _redis: redis, store }
}

fn tenant_record(id: TenantId) -> TenantRecord {
    TenantRecord {
        id,
        name: "smoke-test-co".into(),
        tier: TenantTier::Pro,
        budget: BudgetLimits::DEFAULT_PRO,
        permissions: PermissionSet::default(),
        metadata: HashMap::new(),
        allowed_plugin_ids: vec![],
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrations_are_idempotent() {
    let h = start_harness().await;
    h.store.run_migrations().await.expect("second migrate");
    h.store.run_migrations().await.expect("third migrate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_roundtrip_preserves_rich_content() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();

    let user = UserId::new();
    let workspace = WorkspaceId::new();
    let thread =
        h.store.threads.create_thread(tenant, user, workspace, Some("smoke")).await.unwrap();

    // 1. Plain user text.
    let user_msg = Message::user_text(tenant, thread, "What's the weather?");
    let user_msg_id = user_msg.id;
    h.store.threads.append_message(&user_msg).await.unwrap();

    // 2. Assistant message with Thinking (cache-controlled) + ToolUse.
    let call_id = ToolCallId::new();
    let asst = Message {
        id: MessageId::new(),
        thread_id: thread,
        tenant_id: tenant,
        role: Role::Assistant,
        kind: MessageKind::Assistant {
            stop_reason: Some(StopReason::ToolUse),
            is_api_error: false,
        },
        content: vec![
            ContentBlock::Thinking {
                thinking: "User wants weather — call the weather tool.".into(),
                signature: "sig-abc".into(),
                cache_control: Some(CacheControl::ephemeral().with_ttl(CacheTtl::FiveMin)),
            },
            ContentBlock::ToolUse {
                id: call_id,
                name: "weather".into(),
                input: serde_json::json!({"city": "NYC"}),
                cache_control: None,
            },
        ],
        created_at: Utc::now(),
        token_count: Some(42),
        parent_id: Some(user_msg_id),
    };
    h.store.threads.append_message(&asst).await.unwrap();

    // 3. Tool result.
    let tool_result = Message {
        id: MessageId::new(),
        thread_id: thread,
        tenant_id: tenant,
        role: Role::Tool,
        kind: MessageKind::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call_id,
            content: ToolResultContent::text("72F sunny"),
            is_error: false,
            cache_control: None,
        }],
        created_at: Utc::now(),
        token_count: Some(5),
        parent_id: Some(asst.id),
    };
    h.store.threads.append_message(&tool_result).await.unwrap();

    // Read back.
    let listed = h.store.threads.list_messages(tenant, thread, 100, None).await.unwrap();
    assert_eq!(listed.len(), 3, "three messages expected");
    assert!(matches!(listed[1].content[0], ContentBlock::Thinking { .. }));
    match &listed[1].content[0] {
        ContentBlock::Thinking { thinking: _, signature, cache_control } => {
            assert_eq!(signature, "sig-abc");
            assert!(cache_control.is_some());
        }
        _ => panic!("expected thinking block"),
    }
    match &listed[1].content[1] {
        ContentBlock::ToolUse { id, name, input, .. } => {
            assert_eq!(*id, call_id);
            assert_eq!(name, "weather");
            assert_eq!(input, &serde_json::json!({"city": "NYC"}));
        }
        _ => panic!("expected tool_use block"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_isolation_list_returns_empty_for_wrong_tenant() {
    let h = start_harness().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant_a)).await.unwrap();
    h.store.tenants.upsert_tenant(&tenant_record(tenant_b)).await.unwrap();

    let thread = h
        .store
        .threads
        .create_thread(tenant_a, UserId::new(), WorkspaceId::new(), None)
        .await
        .unwrap();
    let msg = Message::user_text(tenant_a, thread, "secret");
    h.store.threads.append_message(&msg).await.unwrap();

    let listed_a = h.store.threads.list_messages(tenant_a, thread, 10, None).await.unwrap();
    assert_eq!(listed_a.len(), 1);

    let listed_b = h.store.threads.list_messages(tenant_b, thread, 10, None).await.unwrap();
    assert!(listed_b.is_empty(), "cross-tenant list must be empty");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_isolation_get_reports_mismatch() {
    let h = start_harness().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant_a)).await.unwrap();
    h.store.tenants.upsert_tenant(&tenant_record(tenant_b)).await.unwrap();

    let thread = h
        .store
        .threads
        .create_thread(tenant_a, UserId::new(), WorkspaceId::new(), None)
        .await
        .unwrap();
    let msg = Message::user_text(tenant_a, thread, "secret");
    h.store.threads.append_message(&msg).await.unwrap();

    let err = h.store.threads.get_message(tenant_b, msg.id).await.unwrap_err();
    assert!(matches!(err, StoreError::TenantMismatch { .. }), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_thread_is_exclusive() {
    let h = start_harness().await;
    let thread = ThreadId::new();

    assert!(h.store.cache.claim_thread(thread, "worker-1").await.unwrap());
    assert!(!h.store.cache.claim_thread(thread, "worker-2").await.unwrap());

    // Right owner can release; wrong owner is a no-op.
    h.store.cache.release_thread(thread, "worker-2").await.unwrap();
    assert_eq!(h.store.cache.claim_owner(thread).await.unwrap(), Some("worker-1".into()));
    h.store.cache.release_thread(thread, "worker-1").await.unwrap();
    assert_eq!(h.store.cache.claim_owner(thread).await.unwrap(), None);

    // Re-claimable after release.
    assert!(h.store.cache.claim_thread(thread, "worker-2").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_thread_is_idempotent_for_same_owner() {
    // Regression: the worker layer claims at message-receive, then run_loop
    // claims again inside drive_loop. Both must succeed for the same worker_id.
    let h = start_harness().await;
    let thread = ThreadId::new();

    assert!(h.store.cache.claim_thread(thread, "worker-1").await.unwrap());
    assert!(h.store.cache.claim_thread(thread, "worker-1").await.unwrap());
    assert!(h.store.cache.claim_thread(thread, "worker-1").await.unwrap());

    // A different worker is still denied.
    assert!(!h.store.cache.claim_thread(thread, "worker-2").await.unwrap());
    assert_eq!(h.store.cache.claim_owner(thread).await.unwrap(), Some("worker-1".into()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_claim_only_succeeds_for_current_owner() {
    let h = start_harness().await;
    let thread = ThreadId::new();
    h.store.cache.claim_thread(thread, "worker-1").await.unwrap();

    assert!(h.store.cache.refresh_claim(thread, "worker-1").await.unwrap());
    assert!(!h.store.cache.refresh_claim(thread, "worker-2").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similar_messages_empty_when_no_embeddings() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();
    let thread = h
        .store
        .threads
        .create_thread(tenant, UserId::new(), WorkspaceId::new(), None)
        .await
        .unwrap();

    let hits =
        h.store.threads.similar_messages(tenant, thread, &vec![0.1_f32; 384], 5).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn append_message_idempotent_is_safe_on_replay() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();
    let thread = h
        .store
        .threads
        .create_thread(tenant, UserId::new(), WorkspaceId::new(), None)
        .await
        .unwrap();

    let msg = Message::user_text(tenant, thread, "replay me");

    let first = h.store.threads.append_message_idempotent(&msg).await.unwrap();
    assert!(first, "first insert should write a row");
    let second = h.store.threads.append_message_idempotent(&msg).await.unwrap();
    assert!(!second, "second insert with same ID should no-op");

    // Only one row is visible in list_messages — proves ON CONFLICT DO NOTHING.
    let listed = h.store.threads.list_messages(tenant, thread, 10, None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, msg.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_embedding_enables_similarity_retrieval() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();
    let thread = h
        .store
        .threads
        .create_thread(tenant, UserId::new(), WorkspaceId::new(), None)
        .await
        .unwrap();

    // Three messages, three distinct unit-length embeddings.
    let vec_x: Vec<f32> = (0..384).map(|i| if i == 0 { 1.0 } else { 0.0 }).collect();
    let vec_y: Vec<f32> = (0..384).map(|i| if i == 1 { 1.0 } else { 0.0 }).collect();
    let vec_z: Vec<f32> = (0..384).map(|i| if i == 2 { 1.0 } else { 0.0 }).collect();

    let msg_x = Message::user_text(tenant, thread, "x-axis message");
    let msg_y = Message::user_text(tenant, thread, "y-axis message");
    let msg_z = Message::user_text(tenant, thread, "z-axis message");
    h.store.threads.append_message(&msg_x).await.unwrap();
    h.store.threads.append_message(&msg_y).await.unwrap();
    h.store.threads.append_message(&msg_z).await.unwrap();
    h.store.threads.set_embedding(tenant, msg_x.id, &vec_x).await.unwrap();
    h.store.threads.set_embedding(tenant, msg_y.id, &vec_y).await.unwrap();
    h.store.threads.set_embedding(tenant, msg_z.id, &vec_z).await.unwrap();

    // Querying with vec_y should rank msg_y first (distance 0).
    let hits = h.store.threads.similar_messages(tenant, thread, &vec_y, 2).await.unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].id, msg_y.id, "nearest match should be the y-axis row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn episodes_roundtrip_and_scope_to_workspace() {
    use elena_store::Episode;
    use elena_types::{EpisodeId, Outcome};

    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();

    let workspace_a = WorkspaceId::new();
    let workspace_b = WorkspaceId::new();

    let vec_shop: Vec<f32> = (0..384).map(|i| if i < 10 { 1.0 } else { 0.0 }).collect();
    let vec_support: Vec<f32> = (0..384).map(|i| if i >= 374 { 1.0 } else { 0.0 }).collect();

    let ep_shop = Episode {
        id: EpisodeId::new(),
        tenant_id: tenant,
        workspace_id: workspace_a,
        task_summary: "Customer asked about order status".into(),
        actions: vec!["shopify_get_order".into()],
        outcome: Outcome::Completed,
        created_at: Utc::now(),
    };
    let ep_support = Episode {
        id: EpisodeId::new(),
        tenant_id: tenant,
        workspace_id: workspace_b,
        task_summary: "Support agent summarized a ticket".into(),
        actions: vec!["gmail_send".into()],
        outcome: Outcome::Completed,
        created_at: Utc::now(),
    };
    h.store.episodes.insert_episode(&ep_shop, &vec_shop).await.unwrap();
    h.store.episodes.insert_episode(&ep_support, &vec_support).await.unwrap();

    // Recall in workspace_a with the shop-aligned embedding must not surface
    // the support episode from workspace_b.
    let hits_a =
        h.store.episodes.similar_episodes(tenant, workspace_a, &vec_shop, 5).await.unwrap();
    assert_eq!(hits_a.len(), 1);
    assert_eq!(hits_a[0].id, ep_shop.id);

    // Same query, different workspace — also isolated to just its own row.
    let hits_b =
        h.store.episodes.similar_episodes(tenant, workspace_b, &vec_shop, 5).await.unwrap();
    assert_eq!(hits_b.len(), 1);
    assert_eq!(hits_b[0].id, ep_support.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn episodes_without_embedding_excluded_from_similarity() {
    use elena_store::Episode;
    use elena_types::{EpisodeId, Outcome};

    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();
    let workspace = WorkspaceId::new();

    let ep = Episode {
        id: EpisodeId::new(),
        tenant_id: tenant,
        workspace_id: workspace,
        task_summary: "No-embedding record".into(),
        actions: vec![],
        outcome: Outcome::Completed,
        created_at: Utc::now(),
    };
    h.store.episodes.insert_episode(&ep, &[]).await.unwrap();

    let hits =
        h.store.episodes.similar_episodes(tenant, workspace, &vec![0.5_f32; 384], 5).await.unwrap();
    assert!(hits.is_empty(), "NULL-embedding row must be skipped");

    // But recent_episodes still surfaces it — acts as a historical log.
    let recent = h.store.episodes.recent_episodes(tenant, workspace, 10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, ep.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_recording_accumulates_and_rolls_over() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&tenant_record(tenant)).await.unwrap();

    let usage = Usage { input_tokens: 100, output_tokens: 50, ..Default::default() };
    h.store.tenants.record_usage(tenant, usage.clone()).await.unwrap();
    h.store.tenants.record_usage(tenant, usage).await.unwrap();

    let state = h.store.tenants.get_budget_state(tenant).await.unwrap();
    assert_eq!(state.tokens_used_today, 300);
}
