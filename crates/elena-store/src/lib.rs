//! Persistence layer for Elena.
//!
//! - [`ThreadStore`] — Postgres-backed thread and message persistence.
//! - [`TenantStore`] — Postgres-backed tenant state, budget, and usage.
//! - [`SessionCache`] — Redis-backed hot state: thread claims, loop-state
//!   checkpoints, rate-limit windows.
//!
//! Every public method takes a [`TenantId`](elena_types::TenantId) parameter
//! so multi-tenancy is explicit at every call site. Queries filter at the
//! SQL layer; cross-tenant reads surface as
//! [`StoreError::TenantMismatch`](elena_types::StoreError::TenantMismatch).

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod approvals;
mod audit;
mod cache;
mod episode;
mod pg;
mod plugin_ownership;
mod rate_limit;
mod redis;
mod sql_error;
mod tenant;
mod tenant_credentials;
mod thread;
mod workspace;

use std::sync::Arc;

use elena_config::ElenaConfig;
use elena_types::StoreError;

pub use approvals::ApprovalsStore;
pub use audit::{
    AUDIT_CHANNEL_CAP, AuditEvent, AuditSink, DropCallback, NullAuditSink, PostgresAuditSink,
};
pub use cache::SessionCache;
pub use episode::{Episode, EpisodeStore};
pub use plugin_ownership::PluginOwnershipStore;
pub use rate_limit::{
    RateDecision, RateLimiter, plugin_concurrency_key, provider_concurrency_key,
    tenant_inflight_key, tenant_rpm_key,
};
pub use tenant::{TenantRecord, TenantStore};
pub use tenant_credentials::TenantCredentialsStore;
pub use thread::ThreadStore;
pub use workspace::{WorkspaceRecord, WorkspaceStore};

/// All persistence handles for Elena.
///
/// Construct with [`Store::connect`]; run migrations with
/// [`Store::run_migrations`] before first use.
#[derive(Debug, Clone)]
pub struct Store {
    /// Thread / message persistence.
    pub threads: ThreadStore,
    /// Tenant / budget / usage persistence.
    pub tenants: TenantStore,
    /// Redis-backed session state.
    pub cache: SessionCache,
    /// Per-workspace episodic memory.
    pub episodes: EpisodeStore,
    /// Phase-7 rate limiter — Redis Lua token buckets + inflight counter.
    pub rate_limits: RateLimiter,
    /// Phase-7 approvals store — Cautious/Moderate pause-for-approval.
    pub approvals: ApprovalsStore,
    /// Phase-7 workspace store — global instructions + plugin allow-list.
    pub workspaces: WorkspaceStore,
    /// Phase-7 audit sink — every load-bearing loop action writes a row
    /// here. Production ships [`PostgresAuditSink`]; tests can swap in
    /// [`NullAuditSink`] via [`Store::with_audit`].
    pub audit: Arc<dyn AuditSink>,
    /// Phase-7 plugin ownership — Hannlys creator-isolation filter.
    pub plugin_ownerships: PluginOwnershipStore,
    /// Per-tenant encrypted credentials. Wrapped in [`Option`] so a
    /// deployment that has not provisioned the master key can still boot
    /// (single-tenant connectors fall back to env). Hot-loaded by the
    /// worker before each plugin tool dispatch.
    pub tenant_credentials: Option<TenantCredentialsStore>,
}

impl Store {
    /// Connect to Postgres + Redis and build the handles.
    ///
    /// This does not run migrations — call [`Self::run_migrations`] once per
    /// process lifetime (typically at startup) after connecting.
    pub async fn connect(cfg: &ElenaConfig) -> Result<Self, StoreError> {
        Self::connect_with_audit_drops(cfg, Arc::new(|_| {})).await
    }

    /// Like [`Self::connect`], but threads a drop callback into the
    /// audit sink. Boot paths that have a `LoopMetrics` should pass
    /// `Arc::new(move |n| metrics.audit_drops_total.inc_by(n))` so a
    /// non-zero drop count is visible at `/metrics`.
    pub async fn connect_with_audit_drops(
        cfg: &ElenaConfig,
        on_drop: DropCallback,
    ) -> Result<Self, StoreError> {
        let pg = pg::build_pool(&cfg.postgres).await?;
        let redis = redis::build_pool(&cfg.redis).await?;
        let audit: Arc<dyn AuditSink> =
            Arc::new(PostgresAuditSink::spawn_with_callback(pg.clone(), on_drop));
        // Master key is optional at boot — deployments without the key
        // simply skip per-tenant credential injection and connectors
        // fall back to env defaults.
        let tenant_credentials =
            TenantCredentialsStore::from_env_key(pg.clone(), "ELENA_CREDENTIAL_MASTER_KEY").ok();
        Ok(Self {
            threads: ThreadStore::new(pg.clone()),
            tenants: TenantStore::new(pg.clone()),
            cache: SessionCache::new(redis.clone(), cfg.redis.thread_claim_ttl_ms),
            episodes: EpisodeStore::new(pg.clone()),
            rate_limits: RateLimiter::new(redis),
            approvals: ApprovalsStore::new(pg.clone()),
            workspaces: WorkspaceStore::new(pg.clone()),
            audit,
            plugin_ownerships: PluginOwnershipStore::new(pg),
            tenant_credentials,
        })
    }

    /// Swap in an alternate audit sink (typically [`NullAuditSink`] for
    /// tests). Returns a new `Store` cheaply (everything is `Arc`-shared).
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = audit;
        self
    }

    /// Swap in a tenant-credentials store with an explicit master key.
    /// Used by tests and by deployments that wire the key from a source
    /// other than `ELENA_CREDENTIAL_MASTER_KEY`.
    #[must_use]
    pub fn with_tenant_credentials(mut self, store: TenantCredentialsStore) -> Self {
        self.tenant_credentials = Some(store);
        self
    }

    /// Apply all pending database migrations.
    pub async fn run_migrations(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations")
            .run(self.threads.pool())
            .await
            .map_err(|e| StoreError::Database(e.to_string()))
    }
}
