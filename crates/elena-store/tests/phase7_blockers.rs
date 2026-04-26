//! Integration tests for the Phase 7 blocker stores (A1–A5).
//!
//! Spins up `postgres:16` + `redis:7` via `testcontainers`. Each test
//! uses its own harness so they can run in parallel.
//!
//! Run with `cargo test -p elena-store --test phase7_blockers`.

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, ProvidersConfig, RateLimitsConfig, RedisConfig, RouterConfig,
};
use elena_store::{Store, TenantRecord};
use elena_types::{
    ApprovalDecision, ApprovalVerdict, BudgetLimits, PermissionSet, TenantId, TenantTier, ThreadId,
    ToolCallId, UserId, WorkspaceId,
};
use secrecy::SecretString;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

struct Harness {
    pg: ContainerAsync<GenericImage>,
    redis: ContainerAsync<GenericImage>,
    store: Store,
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

async fn start_harness() -> Harness {
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
        context: ContextConfig::default(),
        router: RouterConfig::default(),
        providers: ProvidersConfig::default(),
        rate_limits: RateLimitsConfig::default(),
    };

    let store = Store::connect(&cfg).await.expect("connect");
    store.run_migrations().await.expect("migrations");

    Harness { pg, redis, store }
}

fn bare_tenant(id: TenantId) -> TenantRecord {
    TenantRecord {
        id,
        name: "phase7-blocker-test".into(),
        tier: TenantTier::Pro,
        budget: BudgetLimits::DEFAULT_PRO,
        permissions: PermissionSet::default(),
        metadata: HashMap::new(),
        allowed_plugin_ids: vec![],
        app_id: None,
        deleted_at: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

// ============================================================
// A2 — workspaces: upsert, get, update_instructions, list, instructions helper
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2_workspace_upsert_get_update_instructions_roundtrips() {
    let h = start_harness().await;
    let tenant_id = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_id)).await.unwrap();

    let workspace_id = WorkspaceId::new();
    let initial = h
        .store
        .workspaces
        .upsert(
            workspace_id,
            tenant_id,
            Some("elena-e2e"),
            "Never send payments over $500 without confirmation.",
            &["slack".to_owned(), "stripe".to_owned()],
        )
        .await
        .unwrap();

    assert_eq!(initial.id, workspace_id);
    assert_eq!(initial.name.as_deref(), Some("elena-e2e"));
    assert_eq!(initial.allowed_plugin_ids, vec!["slack".to_owned(), "stripe".to_owned()]);
    assert!(initial.global_instructions.starts_with("Never send payments"));

    // get() with the right tenant works.
    let fetched = h.store.workspaces.get(tenant_id, workspace_id).await.unwrap().unwrap();
    assert_eq!(fetched, initial);

    // get() with the wrong tenant returns TenantMismatch.
    let other_tenant = TenantId::new();
    let err = h.store.workspaces.get(other_tenant, workspace_id).await.unwrap_err();
    assert!(matches!(err, elena_types::StoreError::TenantMismatch { .. }), "got {err:?}");

    // update_instructions bumps the fragment.
    h.store
        .workspaces
        .update_instructions(tenant_id, workspace_id, "New guardrail: be quiet.")
        .await
        .unwrap();
    let ins = h.store.workspaces.instructions(tenant_id, workspace_id).await.unwrap();
    assert_eq!(ins, "New guardrail: be quiet.");

    // instructions() returns empty string on a missing workspace row.
    let missing = WorkspaceId::new();
    let empty = h.store.workspaces.instructions(tenant_id, missing).await.unwrap();
    assert_eq!(empty, "");

    // list_by_tenant returns exactly our one row.
    let list = h.store.workspaces.list_by_tenant(tenant_id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, workspace_id);
}

// ============================================================
// A1 — approvals: write/list/clear + mixed allow/deny batch
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a1_approvals_roundtrip_and_clear() {
    let h = start_harness().await;
    let tenant_id = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_id)).await.unwrap();

    let user_id = UserId::new();
    let workspace_id = WorkspaceId::new();
    let thread_id: ThreadId = h
        .store
        .threads
        .create_thread(tenant_id, user_id, workspace_id, Some("a1-approvals"))
        .await
        .unwrap();

    let allow_id = ToolCallId::new();
    let deny_id = ToolCallId::new();
    let batch = vec![
        ApprovalDecision {
            tool_use_id: allow_id,
            decision: ApprovalVerdict::Allow,
            edits: Some(serde_json::json!({"channel": "#overridden"})),
        },
        ApprovalDecision { tool_use_id: deny_id, decision: ApprovalVerdict::Deny, edits: None },
    ];

    h.store.approvals.write(tenant_id, thread_id, &batch).await.unwrap();

    // list() returns them both, ordered by created_at (insertion order).
    let decisions = h.store.approvals.list(tenant_id, thread_id).await.unwrap();
    assert_eq!(decisions.len(), 2);
    let allow = decisions.iter().find(|d| d.tool_use_id == allow_id).unwrap();
    let deny = decisions.iter().find(|d| d.tool_use_id == deny_id).unwrap();
    assert_eq!(allow.decision, ApprovalVerdict::Allow);
    assert_eq!(allow.edits, Some(serde_json::json!({"channel": "#overridden"})));
    assert_eq!(deny.decision, ApprovalVerdict::Deny);
    assert!(deny.edits.is_none());

    // Cross-tenant read returns empty — never leaks.
    let other_tenant = TenantId::new();
    let cross = h.store.approvals.list(other_tenant, thread_id).await.unwrap();
    assert!(cross.is_empty());

    // Re-write with the same tool_use_id + different decision upserts.
    let updated = vec![ApprovalDecision {
        tool_use_id: allow_id,
        decision: ApprovalVerdict::Deny,
        edits: None,
    }];
    h.store.approvals.write(tenant_id, thread_id, &updated).await.unwrap();
    let after = h.store.approvals.list(tenant_id, thread_id).await.unwrap();
    let allow_now = after.iter().find(|d| d.tool_use_id == allow_id).unwrap();
    assert_eq!(allow_now.decision, ApprovalVerdict::Deny);

    // clear() wipes both rows.
    let removed = h.store.approvals.clear(tenant_id, thread_id).await.unwrap();
    assert_eq!(removed, 2);
    assert!(h.store.approvals.list(tenant_id, thread_id).await.unwrap().is_empty());
}

// ============================================================
// A3 — audit: PostgresAuditSink writes land in audit_events
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a3_audit_event_is_persisted() {
    let h = start_harness().await;
    let tenant_id = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_id)).await.unwrap();

    let thread_id = ThreadId::new();
    let event = elena_store::AuditEvent::new(
        tenant_id,
        "tool_use",
        serde_json::json!({"tool_name": "echo_reverse"}),
    )
    .with_thread(thread_id);

    h.store.audit.emit(event).await;

    // Flusher is async — poll for the row up to ~2s.
    for _ in 0..40 {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT kind FROM audit_events WHERE tenant_id = $1 AND thread_id = $2")
                .bind(tenant_id.as_uuid())
                .bind(thread_id.as_uuid())
                .fetch_optional(h.store.threads.pool_for_test())
                .await
                .unwrap_or(None);
        if let Some((kind,)) = row {
            assert_eq!(kind, "tool_use");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("audit row never landed in audit_events");
}

// ============================================================
// A4 — tenants.allowed_plugin_ids persists and updates
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a4_tenant_allow_list_persists_and_updates() {
    let h = start_harness().await;
    let tenant_id = TenantId::new();

    let mut t = bare_tenant(tenant_id);
    t.allowed_plugin_ids = vec!["echo".to_owned(), "slack".to_owned()];
    h.store.tenants.upsert_tenant(&t).await.unwrap();

    let fetched = h.store.tenants.get_tenant(tenant_id).await.unwrap();
    assert_eq!(fetched.allowed_plugin_ids, vec!["echo".to_owned(), "slack".to_owned()]);

    h.store.tenants.update_allowed_plugins(tenant_id, &["stripe".to_owned()]).await.unwrap();
    let after = h.store.tenants.get_tenant(tenant_id).await.unwrap();
    assert_eq!(after.allowed_plugin_ids, vec!["stripe".to_owned()]);

    // Updating a non-existent tenant is a clear error.
    let ghost = TenantId::new();
    let err = h.store.tenants.update_allowed_plugins(ghost, &[]).await.unwrap_err();
    assert!(matches!(err, elena_types::StoreError::TenantNotFound(_)), "got {err:?}");
}

// ============================================================
// A5 — plugin_ownerships: is_visible + visible_plugins semantics
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a5_plugin_ownerships_visibility_rules() {
    let h = start_harness().await;
    let creator_a = TenantId::new();
    let creator_b = TenantId::new();
    let buyer = TenantId::new();
    for t in [creator_a, creator_b, buyer] {
        h.store.tenants.upsert_tenant(&bare_tenant(t)).await.unwrap();
    }

    // Global plugin (no ownership rows yet).
    assert!(h.store.plugin_ownerships.is_visible("echo", buyer).await.unwrap());

    // Creator A's private Seed.
    h.store.plugin_ownerships.set_owners("workout_builder", &[creator_a]).await.unwrap();
    assert!(h.store.plugin_ownerships.is_visible("workout_builder", creator_a).await.unwrap());
    assert!(!h.store.plugin_ownerships.is_visible("workout_builder", creator_b).await.unwrap());
    assert!(!h.store.plugin_ownerships.is_visible("workout_builder", buyer).await.unwrap());

    // Shared plugin across two creators.
    h.store.plugin_ownerships.set_owners("shared_utility", &[creator_a, creator_b]).await.unwrap();
    assert!(h.store.plugin_ownerships.is_visible("shared_utility", creator_a).await.unwrap());
    assert!(h.store.plugin_ownerships.is_visible("shared_utility", creator_b).await.unwrap());
    assert!(!h.store.plugin_ownerships.is_visible("shared_utility", buyer).await.unwrap());

    // visible_plugins intersects candidate set with the rules.
    let visible = h
        .store
        .plugin_ownerships
        .visible_plugins(
            creator_a,
            &[
                "echo".to_owned(),
                "workout_builder".to_owned(),
                "shared_utility".to_owned(),
                "ghost".to_owned(),
            ],
        )
        .await
        .unwrap();
    assert!(visible.contains(&"echo".to_owned())); // global
    assert!(visible.contains(&"workout_builder".to_owned())); // owned
    assert!(visible.contains(&"shared_utility".to_owned())); // co-owned
    assert!(visible.contains(&"ghost".to_owned())); // no rows → global

    // set_owners with empty list restores global.
    h.store.plugin_ownerships.set_owners("workout_builder", &[]).await.unwrap();
    assert!(h.store.plugin_ownerships.is_visible("workout_builder", buyer).await.unwrap());

    // clear() also restores global.
    h.store.plugin_ownerships.set_owners("shared_utility", &[creator_a]).await.unwrap();
    h.store.plugin_ownerships.clear("shared_utility").await.unwrap();
    assert!(h.store.plugin_ownerships.is_visible("shared_utility", buyer).await.unwrap());
}

// ============================================================
// C7.1 — RateLimiter integration: the Redis Lua token bucket
// actually rejects under load and reports a sane retry_after.
// Inflight counter caps and releases.
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c71_token_bucket_denies_after_burst_with_retry_after() {
    let h = start_harness().await;
    let tenant_id = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_id)).await.unwrap();
    let key = elena_store::tenant_rpm_key(tenant_id);

    // Slow refill (1/s), tiny burst (3) — easy to overrun.
    let rate_per_sec = 1;
    let burst = 3;

    // First three calls succeed (allow).
    for n in 0..burst {
        let decision = h.store.rate_limits.try_take(&key, rate_per_sec, burst).await.unwrap();
        assert!(
            matches!(decision, elena_store::RateDecision::Allow { .. }),
            "call #{n}: expected Allow, got {decision:?}"
        );
    }

    // Fourth call is denied with a finite retry_after.
    let denied = h.store.rate_limits.try_take(&key, rate_per_sec, burst).await.unwrap();
    match denied {
        elena_store::RateDecision::Deny { retry_after_ms } => {
            assert!(
                retry_after_ms > 0 && retry_after_ms <= 2_000,
                "retry_after_ms looks wrong: {retry_after_ms}"
            );
        }
        other @ elena_store::RateDecision::Allow { .. } => {
            panic!("expected Deny after burst, got {other:?}")
        }
    }

    // Wait a beat and let the refill restore at least one token.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let after_refill = h.store.rate_limits.try_take(&key, rate_per_sec, burst).await.unwrap();
    assert!(
        matches!(after_refill, elena_store::RateDecision::Allow { .. }),
        "expected refill to allow another call, got {after_refill:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c71_inflight_counter_caps_and_releases() {
    let h = start_harness().await;
    let tenant_id = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_id)).await.unwrap();
    let key = elena_store::tenant_inflight_key(tenant_id);

    let max = 2;
    // Two acquires succeed.
    let first = h.store.rate_limits.acquire_inflight(&key, max).await.unwrap();
    let second = h.store.rate_limits.acquire_inflight(&key, max).await.unwrap();
    assert!(matches!(first, elena_store::RateDecision::Allow { .. }), "first: {first:?}");
    assert!(matches!(second, elena_store::RateDecision::Allow { .. }), "second: {second:?}");

    // Third acquire is denied (counter already at max).
    let third = h.store.rate_limits.acquire_inflight(&key, max).await.unwrap();
    assert!(matches!(third, elena_store::RateDecision::Deny { .. }), "third deny: {third:?}");

    // Release one and the next acquire should succeed.
    h.store.rate_limits.release_inflight(&key).await.unwrap();
    let after = h.store.rate_limits.acquire_inflight(&key, max).await.unwrap();
    assert!(
        matches!(after, elena_store::RateDecision::Allow { .. }),
        "post-release acquire: {after:?}"
    );
}

// ============================================================
// E5 — Hannlys two-creator isolation: A4 (tenant allow-list) ∩ A5
// (per-tenant ownership) actually yield the right per-tenant
// plugin sets at the store layer. Mirrors the rule
// `filter_schemas_for_tenant` enforces in step.rs.
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e5_two_creator_marketplace_isolation() {
    let h = start_harness().await;
    // Three tenants: two Hannlys creators and one buyer.
    let creator_a = TenantId::new();
    let creator_b = TenantId::new();
    let buyer = TenantId::new();
    for t in [creator_a, creator_b, buyer] {
        h.store.tenants.upsert_tenant(&bare_tenant(t)).await.unwrap();
    }

    // Each creator's Seed plugin is owned only by that creator. A
    // shared "echo" utility is global (no ownership rows). The buyer
    // is scoped to creator A's marketplace via tenant allow-list.
    h.store.plugin_ownerships.set_owners("seed_workout_a", &[creator_a]).await.unwrap();
    h.store.plugin_ownerships.set_owners("seed_workout_b", &[creator_b]).await.unwrap();
    // `echo` has no ownership rows → global.

    // Buyer is configured to only see Creator A's Seed + the global
    // echo utility. Creator B's Seed is not in their allow-list
    // (A4 layer).
    h.store
        .tenants
        .update_allowed_plugins(buyer, &["seed_workout_a".to_owned(), "echo".to_owned()])
        .await
        .unwrap();

    let candidates =
        vec!["seed_workout_a".to_owned(), "seed_workout_b".to_owned(), "echo".to_owned()];

    // Step 1 — A5 visibility filter (ownership intersection).
    let visible_to_buyer =
        h.store.plugin_ownerships.visible_plugins(buyer, &candidates).await.unwrap();
    // Buyer is NOT an owner of either Seed; only `echo` (global) is
    // visible at the ownership layer.
    assert!(visible_to_buyer.contains(&"echo".to_owned()));
    assert!(
        !visible_to_buyer.contains(&"seed_workout_a".to_owned()),
        "A5 should hide creator A's Seed from a non-owner buyer; got {visible_to_buyer:?}"
    );
    assert!(
        !visible_to_buyer.contains(&"seed_workout_b".to_owned()),
        "A5 should hide creator B's Seed from a non-owner buyer; got {visible_to_buyer:?}"
    );

    // Step 2 — A4 allow-list intersection (Rust side, mirrors
    // filter_schemas_for_tenant). Buyer's allow-list intersected with
    // ownership-visible set.
    let buyer_record = h.store.tenants.get_tenant(buyer).await.unwrap();
    let final_set: Vec<String> = visible_to_buyer
        .into_iter()
        .filter(|p| buyer_record.allowed_plugin_ids.contains(p))
        .collect();
    assert_eq!(
        final_set,
        vec!["echo".to_owned()],
        "buyer should see only `echo` after A4 ∩ A5; got {final_set:?}"
    );

    // Step 3 — flip the buyer to the marketplace where Creator A is
    // an explicit owner. Update ownership so the buyer is added to
    // creator A's Seed (e.g., they purchased a license).
    h.store.plugin_ownerships.set_owners("seed_workout_a", &[creator_a, buyer]).await.unwrap();
    h.store
        .tenants
        .update_allowed_plugins(buyer, &["seed_workout_a".to_owned(), "echo".to_owned()])
        .await
        .unwrap();
    let visible_after_purchase =
        h.store.plugin_ownerships.visible_plugins(buyer, &candidates).await.unwrap();
    assert!(visible_after_purchase.contains(&"seed_workout_a".to_owned()));
    assert!(!visible_after_purchase.contains(&"seed_workout_b".to_owned()));

    let buyer_after = h.store.tenants.get_tenant(buyer).await.unwrap();
    let final_after: Vec<String> = visible_after_purchase
        .into_iter()
        .filter(|p| buyer_after.allowed_plugin_ids.contains(p))
        .collect();
    let mut sorted_after = final_after.clone();
    sorted_after.sort();
    assert_eq!(
        sorted_after,
        vec!["echo".to_owned(), "seed_workout_a".to_owned()],
        "post-purchase buyer should see echo + seed_workout_a; got {final_after:?}"
    );

    // Step 4 — Creator B's marketplace stays isolated. Update them
    // to allow their own Seed; verify A's Seed never leaks through
    // even if they accidentally allow-listed it (defense in depth).
    h.store
        .tenants
        .update_allowed_plugins(
            creator_b,
            &["seed_workout_a".to_owned(), "seed_workout_b".to_owned()],
        )
        .await
        .unwrap();
    let visible_to_b =
        h.store.plugin_ownerships.visible_plugins(creator_b, &candidates).await.unwrap();
    let creator_b_record = h.store.tenants.get_tenant(creator_b).await.unwrap();
    let final_b: Vec<String> = visible_to_b
        .into_iter()
        .filter(|p| creator_b_record.allowed_plugin_ids.contains(p))
        .collect();
    assert!(final_b.contains(&"seed_workout_b".to_owned()));
    assert!(
        !final_b.contains(&"seed_workout_a".to_owned()),
        "even when allow-listed, A5 ownership filter must hide creator A's Seed from B; \
         got {final_b:?}"
    );
}
