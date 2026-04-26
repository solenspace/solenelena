//! End-to-end retrieval test against throwaway Postgres (with pgvector).
//!
//! Uses `FakeEmbedder` — no ONNX runtime required. Verifies that:
//!
//! 1. `embed_and_store` writes a usable embedding (round-trip via
//!    `similar_messages` returns the right row).
//! 2. `build_context` degrades to a pure recency window when retrieval is
//!    disabled (`NullEmbedder`), and to retrieval-augmented ordering when
//!    enabled (`FakeEmbedder`).

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, RedisConfig, RouterConfig,
};
use elena_context::{ContextManager, ContextManagerOptions, Embedder, FakeEmbedder, NullEmbedder};
use elena_store::{Store, TenantRecord};
use elena_types::{
    BudgetLimits, Message, PermissionSet, TenantId, TenantTier, UserId, WorkspaceId,
};
use secrecy::SecretString;
use testcontainers::{GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

async fn harness() -> (Arc<Store>, TenantId, elena_types::ThreadId) {
    let pg = GenericImage::new("pgvector/pgvector", "pg16")
        .with_wait_for(WaitFor::message_on_stderr("database system is ready to accept connections"))
        .with_env_var("POSTGRES_PASSWORD", "elena")
        .with_env_var("POSTGRES_USER", "elena")
        .with_env_var("POSTGRES_DB", "elena")
        .start()
        .await
        .expect("pg");
    // Hold the container in a leaked Box so the test runtime keeps it alive.
    let pg_port = pg.get_host_port_ipv4(5432).await.expect("pg port");
    Box::leak(Box::new(pg));

    let redis = GenericImage::new("redis", "7-alpine")
        .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
        .start()
        .await
        .expect("redis");
    let redis_port = redis.get_host_port_ipv4(6379).await.expect("redis port");
    Box::leak(Box::new(redis));

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
    store
        .tenants
        .upsert_tenant(&TenantRecord {
            id: tenant_id,
            name: "retrieval-test".into(),
            tier: TenantTier::Pro,
            budget: BudgetLimits::DEFAULT_PRO,
            permissions: PermissionSet::default(),
            metadata: std::collections::HashMap::new(),
            allowed_plugin_ids: vec![],
            app_id: None,
            deleted_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .await
        .expect("upsert tenant");
    let thread_id = store
        .threads
        .create_thread(tenant_id, UserId::new(), WorkspaceId::new(), Some("retrieval"))
        .await
        .expect("create thread");
    (Arc::new(store), tenant_id, thread_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embed_and_store_enables_similarity_retrieval() {
    let (store, tenant_id, thread_id) = harness().await;
    let cm = ContextManager::new(
        Arc::new(FakeEmbedder) as Arc<dyn Embedder>,
        ContextManagerOptions::default(),
    );

    // Three diverse messages.
    for text in [
        "how do I revoke a shopify token",
        "remind me about yesterday's meeting",
        "what was the weather on tuesday",
    ] {
        let msg = Message::user_text(tenant_id, thread_id, text);
        store.threads.append_message(&msg).await.unwrap();
        cm.embed_and_store(tenant_id, msg.id, text, &store.threads).await.unwrap();
    }

    // Query with a string semantically identical (same fake hash) to the
    // first message — it should come back first.
    let query = "how do I revoke a shopify token";
    let embedding = FakeEmbedder.embed(query).await.unwrap();
    let hits = store.threads.similar_messages(tenant_id, thread_id, &embedding, 3).await.unwrap();
    assert_eq!(hits.len(), 3);
    match &hits[0].content[0] {
        elena_types::ContentBlock::Text { text, .. } => assert_eq!(text, query),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_context_without_embedder_returns_recency_window() {
    let (store, tenant_id, thread_id) = harness().await;
    let cm = ContextManager::new(
        Arc::new(NullEmbedder) as Arc<dyn Embedder>,
        ContextManagerOptions {
            recency_window: 3,
            retrieval_top_k: 0,
            default_budget_tokens: 10_000,
        },
    );

    for i in 0..6 {
        let msg = Message::user_text(tenant_id, thread_id, format!("message {i}"));
        store.threads.append_message(&msg).await.unwrap();
    }
    let out = cm
        .build_context(tenant_id, thread_id, "doesn't matter", &store.threads, 10_000)
        .await
        .unwrap();
    // Recency window was 3, all 3 fit in budget.
    assert_eq!(out.len(), 3);
    let expected_last: Vec<_> = (3..6).map(|i| format!("message {i}")).collect();
    let got: Vec<_> = out
        .iter()
        .filter_map(|m| match &m.content[0] {
            elena_types::ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(got, expected_last);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_context_with_embedder_returns_union_of_recent_and_similar() {
    let (store, tenant_id, thread_id) = harness().await;
    let cm = ContextManager::new(
        Arc::new(FakeEmbedder) as Arc<dyn Embedder>,
        ContextManagerOptions {
            recency_window: 2,
            retrieval_top_k: 3,
            default_budget_tokens: 10_000,
        },
    );

    // 5 messages — first is the hit we want to retrieve, last 2 are tail.
    let target_text = "hidden gem";
    let target = Message::user_text(tenant_id, thread_id, target_text);
    store.threads.append_message(&target).await.unwrap();
    cm.embed_and_store(tenant_id, target.id, target_text, &store.threads).await.unwrap();

    for i in 1..5 {
        let t = format!("filler {i}");
        let msg = Message::user_text(tenant_id, thread_id, &t);
        store.threads.append_message(&msg).await.unwrap();
        cm.embed_and_store(tenant_id, msg.id, &t, &store.threads).await.unwrap();
    }

    let out =
        cm.build_context(tenant_id, thread_id, target_text, &store.threads, 10_000).await.unwrap();
    // Union should contain the retrieved target plus the last 2 messages.
    let texts: Vec<_> = out
        .iter()
        .filter_map(|m| match &m.content[0] {
            elena_types::ContentBlock::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&target_text.to_owned()), "target should be in output: {texts:?}");
    assert!(texts.contains(&"filler 3".to_owned()));
    assert!(texts.contains(&"filler 4".to_owned()));
}
