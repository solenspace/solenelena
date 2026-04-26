//! Integration tests for the B1 `plans` and `plan_assignments` stores.
//!
//! Spins up `postgres:16` + `redis:7` via `testcontainers`. Each test
//! uses its own harness so they can run in parallel.
//!
//! Run with `cargo test -p elena-store --test plans`.

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use chrono::Utc;
use elena_config::{
    CacheConfig, ContextConfig, DefaultsConfig, ElenaConfig, LogFormat, LoggingConfig,
    PostgresConfig, ProvidersConfig, RateLimitsConfig, RedisConfig, RouterConfig,
};
use elena_store::{Store, TenantRecord};
use elena_types::{
    AutonomyMode, BudgetLimits, PermissionSet, Plan, PlanId, PlanSlug, StoreError, TenantId,
    TenantTier, UserId, WorkspaceId,
};
use secrecy::SecretString;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, core::WaitFor, runners::AsyncRunner};

struct Harness {
    _pg: ContainerAsync<GenericImage>,
    _redis: ContainerAsync<GenericImage>,
    store: Store,
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

    Harness { _pg: pg, _redis: redis, store }
}

fn bare_tenant(id: TenantId) -> TenantRecord {
    TenantRecord {
        id,
        name: "plans-test-co".into(),
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

fn sample_plan(tenant_id: TenantId, slug: &str, is_default: bool) -> Plan {
    let now = Utc::now();
    Plan {
        id: PlanId::new(),
        tenant_id,
        slug: PlanSlug::new(slug),
        display_name: format!("Plan {slug}"),
        is_default,
        budget: BudgetLimits::DEFAULT_PRO,
        rate_limits: serde_json::json!({}),
        allowed_plugin_ids: vec![],
        tier_models: None,
        autonomy_default: AutonomyMode::Moderate,
        cache_policy: serde_json::json!({}),
        max_cascade_escalations: 1,
        metadata: HashMap::new(),
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_upsert_get_list_roundtrips() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();

    let plan = Plan {
        budget: BudgetLimits {
            max_tokens_per_thread: 250_000,
            max_tokens_per_day: 5_000_000,
            max_threads_concurrent: 25,
            max_tool_calls_per_turn: 32,
        },
        rate_limits: serde_json::json!({"tenant_rpm": 600}),
        allowed_plugin_ids: vec!["slack".into(), "notion".into()],
        tier_models: Some(serde_json::json!({"fast": "groq/llama-3.1-8b"})),
        autonomy_default: AutonomyMode::Cautious,
        cache_policy: serde_json::json!({"ttl_seconds": 300}),
        max_cascade_escalations: 2,
        metadata: HashMap::from([(
            "sku".to_owned(),
            serde_json::Value::String("CREATOR_ANNUAL".to_owned()),
        )]),
        ..sample_plan(tenant, "creator-annual", true)
    };

    h.store.plans.upsert(&plan).await.unwrap();

    let fetched = h.store.plans.get(tenant, plan.id).await.unwrap().expect("plan present");
    assert_eq!(fetched.id, plan.id);
    assert_eq!(fetched.slug, plan.slug);
    assert_eq!(fetched.display_name, plan.display_name);
    assert!(fetched.is_default);
    assert_eq!(fetched.budget, plan.budget);
    assert_eq!(fetched.rate_limits, plan.rate_limits);
    assert_eq!(fetched.allowed_plugin_ids, plan.allowed_plugin_ids);
    assert_eq!(fetched.tier_models, plan.tier_models);
    assert_eq!(fetched.autonomy_default, AutonomyMode::Cautious);
    assert_eq!(fetched.cache_policy, plan.cache_policy);
    assert_eq!(fetched.max_cascade_escalations, 2);
    assert_eq!(fetched.metadata, plan.metadata);

    let listed = h.store.plans.list_by_tenant(tenant).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, plan.id);

    let by_slug = h
        .store
        .plans
        .get_by_slug(tenant, &PlanSlug::new("creator-annual"))
        .await
        .unwrap()
        .expect("by-slug present");
    assert_eq!(by_slug.id, plan.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slug_is_tenant_scoped() {
    let h = start_harness().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_a)).await.unwrap();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_b)).await.unwrap();

    // Same slug under two different tenants is fine.
    h.store.plans.upsert(&sample_plan(tenant_a, "starter", true)).await.unwrap();
    h.store.plans.upsert(&sample_plan(tenant_b, "starter", true)).await.unwrap();

    // Same slug under the same tenant on a different plan id collides.
    let dup = sample_plan(tenant_a, "starter", false);
    let err = h.store.plans.upsert(&dup).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_default_swaps_atomically() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();

    let a = sample_plan(tenant, "starter", true);
    let b = sample_plan(tenant, "creator", false);
    h.store.plans.upsert(&a).await.unwrap();
    h.store.plans.upsert(&b).await.unwrap();

    let default = h.store.plans.default_for_tenant(tenant).await.unwrap();
    assert_eq!(default.id, a.id, "starter is default initially");

    // Swap the default to plan B — the partial unique index forbids two
    // defaults but PG defers the check to end-of-statement, so the single
    // UPDATE is safe.
    h.store.plans.set_default(tenant, b.id).await.unwrap();

    let default = h.store.plans.default_for_tenant(tenant).await.unwrap();
    assert_eq!(default.id, b.id, "creator is default after swap");

    let all = h.store.plans.list_by_tenant(tenant).await.unwrap();
    let defaults: Vec<_> = all.iter().filter(|p| p.is_default).collect();
    assert_eq!(defaults.len(), 1, "exactly one default after swap");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_default_rejects_cross_tenant_plan() {
    let h = start_harness().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_a)).await.unwrap();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_b)).await.unwrap();

    let plan_a = sample_plan(tenant_a, "starter", true);
    let plan_b = sample_plan(tenant_b, "starter", true);
    h.store.plans.upsert(&plan_a).await.unwrap();
    h.store.plans.upsert(&plan_b).await.unwrap();

    // Tenant A asks to make plan_b (owned by tenant B) the default —
    // the WHERE clause filters plan_b out, so the UPDATE silently
    // touches only A's rows. The follow-up verification step catches
    // this and returns Database error rather than silently succeeding.
    let err = h.store.plans.set_default(tenant_a, plan_b.id).await.unwrap_err();
    assert!(matches!(err, StoreError::Database(_)), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_referenced_plan_returns_conflict() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();

    let default = sample_plan(tenant, "starter", true);
    let creator = sample_plan(tenant, "creator", false);
    h.store.plans.upsert(&default).await.unwrap();
    h.store.plans.upsert(&creator).await.unwrap();

    let user = UserId::new();
    h.store.plan_assignments.upsert(tenant, Some(user), None, creator.id).await.unwrap();

    let err = h.store.plans.delete(tenant, creator.id).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "expected Conflict, got {err:?}");

    // Removing the assignment first lets the delete succeed.
    h.store.plan_assignments.delete_for_scope(tenant, Some(user), None).await.unwrap();
    h.store.plans.delete(tenant, creator.id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_reports_tenant_mismatch() {
    let h = start_harness().await;
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_a)).await.unwrap();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant_b)).await.unwrap();

    let plan_a = sample_plan(tenant_a, "starter", true);
    h.store.plans.upsert(&plan_a).await.unwrap();

    let err = h.store.plans.get(tenant_b, plan_a.id).await.unwrap_err();
    assert!(matches!(err, StoreError::TenantMismatch { .. }), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_all_null_rejected_before_db() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();
    let plan = sample_plan(tenant, "starter", true);
    h.store.plans.upsert(&plan).await.unwrap();

    let err = h.store.plan_assignments.upsert(tenant, None, None, plan.id).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assignment_upsert_keeps_id_stable_on_replace() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();
    let plan_a = sample_plan(tenant, "starter", true);
    let plan_b = sample_plan(tenant, "creator", false);
    h.store.plans.upsert(&plan_a).await.unwrap();
    h.store.plans.upsert(&plan_b).await.unwrap();

    let user = UserId::new();
    let workspace = WorkspaceId::new();

    // For each shape, a re-upsert under the same scope should overwrite
    // the plan_id but keep the row's `id` stable — clients that store
    // assignment IDs after the first call must see them survive replace.
    for (uid, wid) in [(Some(user), Some(workspace)), (Some(user), None), (None, Some(workspace))] {
        let first = h.store.plan_assignments.upsert(tenant, uid, wid, plan_a.id).await.unwrap();
        let second = h.store.plan_assignments.upsert(tenant, uid, wid, plan_b.id).await.unwrap();
        assert_eq!(first.id, second.id, "assignment id changed on replace for ({uid:?},{wid:?})");
        assert_eq!(second.plan_id, plan_b.id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_falls_back_to_tenant_default() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();
    let default = sample_plan(tenant, "starter", true);
    h.store.plans.upsert(&default).await.unwrap();

    let resolved = h
        .store
        .plan_assignments
        .resolve(tenant, Some(UserId::new()), Some(WorkspaceId::new()))
        .await
        .unwrap();
    assert_eq!(resolved.plan_id, default.id);
    assert_eq!(resolved.slug, PlanSlug::new("starter"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_uses_most_specific_match() {
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();

    let default = sample_plan(tenant, "starter", true);
    let user_plan = sample_plan(tenant, "user-plan", false);
    let workspace_plan = sample_plan(tenant, "workspace-plan", false);
    let exact_plan = sample_plan(tenant, "exact-plan", false);
    h.store.plans.upsert(&default).await.unwrap();
    h.store.plans.upsert(&user_plan).await.unwrap();
    h.store.plans.upsert(&workspace_plan).await.unwrap();
    h.store.plans.upsert(&exact_plan).await.unwrap();

    let user = UserId::new();
    let workspace = WorkspaceId::new();
    let other_user = UserId::new();
    let other_workspace = WorkspaceId::new();

    // Rule 4 (default) is in effect when no assignments exist.
    let resolved =
        h.store.plan_assignments.resolve(tenant, Some(user), Some(workspace)).await.unwrap();
    assert_eq!(resolved.plan_id, default.id, "rule 4 default");

    // Add a workspace-only assignment → rule 3 takes over for any user
    // in that workspace.
    h.store
        .plan_assignments
        .upsert(tenant, None, Some(workspace), workspace_plan.id)
        .await
        .unwrap();
    let resolved =
        h.store.plan_assignments.resolve(tenant, Some(other_user), Some(workspace)).await.unwrap();
    assert_eq!(resolved.plan_id, workspace_plan.id, "rule 3 workspace-only");

    // Add a user-only assignment → rule 2 outranks rule 3 when the user
    // appears anywhere outside the named workspace.
    h.store.plan_assignments.upsert(tenant, Some(user), None, user_plan.id).await.unwrap();
    let resolved =
        h.store.plan_assignments.resolve(tenant, Some(user), Some(other_workspace)).await.unwrap();
    assert_eq!(resolved.plan_id, user_plan.id, "rule 2 user-only");

    // Rule 1 (exact match) outranks both rule 2 and rule 3.
    h.store
        .plan_assignments
        .upsert(tenant, Some(user), Some(workspace), exact_plan.id)
        .await
        .unwrap();
    let resolved =
        h.store.plan_assignments.resolve(tenant, Some(user), Some(workspace)).await.unwrap();
    assert_eq!(resolved.plan_id, exact_plan.id, "rule 1 exact");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_errors_when_tenant_has_no_default() {
    // Insert a tenant directly via the SQL pool, bypassing the admin
    // path that would seed a default plan. Simulates a misconfigured
    // tenant and verifies the resolver fails loudly rather than
    // returning a phantom plan.
    let h = start_harness().await;
    let tenant = TenantId::new();
    h.store.tenants.upsert_tenant(&bare_tenant(tenant)).await.unwrap();

    let err = h
        .store
        .plan_assignments
        .resolve(tenant, Some(UserId::new()), Some(WorkspaceId::new()))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Database(_)), "got {err:?}");
}
