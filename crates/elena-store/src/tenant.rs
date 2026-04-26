//! Tenant persistence.
//!
//! Tenants are the primary isolation boundary. Every other store method
//! takes a [`TenantId`] and filters on it; this module is where tenant rows
//! themselves are upserted and fetched.

use chrono::{DateTime, Utc};
use elena_types::{
    AppId, BudgetLimits, PermissionSet, StoreError, TenantId, TenantTier, ThreadId, Usage,
};
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder, Row};
use tracing::instrument;

use crate::sql_error::{classify_serde, classify_sqlx};

/// One tenant row.
#[derive(Debug, Clone, PartialEq)]
pub struct TenantRecord {
    /// Tenant identifier.
    pub id: TenantId,
    /// Human-readable name (for audit logs / display).
    pub name: String,
    /// Subscription tier.
    pub tier: TenantTier,
    /// Budget limits currently applied.
    pub budget: BudgetLimits,
    /// Resolved permission set.
    pub permissions: PermissionSet,
    /// Opaque app-specific metadata.
    pub metadata: std::collections::HashMap<String, Value>,
    /// Plugin allow-list. Empty means "all plugins" for backwards
    /// compatibility; a non-empty list restricts which plugin IDs this
    /// tenant can see in `PluginRegistry::tools_for`.
    pub allowed_plugin_ids: Vec<String>,
    /// Owning app, if any. `None` for tenants that predate the apps
    /// table or were never associated with one.
    pub app_id: Option<AppId>,
    /// Soft-delete marker. `Some` tenants are excluded from active reads;
    /// runtime callers (gateway, worker) treat them as nonexistent.
    pub deleted_at: Option<DateTime<Utc>>,
    /// When the tenant row was first created.
    pub created_at: DateTime<Utc>,
    /// When the tenant row was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Filter passed to [`TenantStore::list_all`].
#[derive(Debug, Clone, Default)]
pub struct TenantListFilter {
    /// Only tenants attached to this app.
    pub app_id: Option<AppId>,
    /// Case-sensitive prefix match against `tenants.name`.
    pub name_prefix: Option<String>,
    /// Page size. Callers should clamp before passing through.
    pub limit: u32,
    /// Pagination offset (small set; offset pagination is fine).
    pub offset: u32,
    /// Include soft-deleted rows. Default: live rows only.
    pub include_deleted: bool,
}

/// Running budget counters for a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetState {
    /// Tokens spent today (rolls over at `day_rollover_at`).
    pub tokens_used_today: u64,
    /// When the daily counter next resets.
    pub day_rollover_at: DateTime<Utc>,
    /// Number of threads currently active for this tenant.
    pub threads_active: u32,
}

/// Handle to the tenant persistence layer.
#[derive(Debug, Clone)]
pub struct TenantStore {
    pool: PgPool,
}

impl TenantStore {
    pub(crate) fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create or update a tenant row.
    #[instrument(skip(self, tenant), fields(tenant_id = %tenant.id))]
    pub async fn upsert_tenant(&self, tenant: &TenantRecord) -> Result<(), StoreError> {
        let budget = serde_json::to_value(tenant.budget).map_err(|e| classify_serde(&e))?;
        let permissions =
            serde_json::to_value(&tenant.permissions).map_err(|e| classify_serde(&e))?;
        let metadata = serde_json::to_value(&tenant.metadata).map_err(|e| classify_serde(&e))?;

        sqlx::query(
            "INSERT INTO tenants (
                 id, name, tier, budget, permissions, metadata,
                 allowed_plugin_ids, app_id
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO UPDATE SET
                name               = EXCLUDED.name,
                tier               = EXCLUDED.tier,
                budget             = EXCLUDED.budget,
                permissions        = EXCLUDED.permissions,
                metadata           = EXCLUDED.metadata,
                allowed_plugin_ids = EXCLUDED.allowed_plugin_ids,
                app_id             = EXCLUDED.app_id",
        )
        .bind(tenant.id.as_uuid())
        .bind(&tenant.name)
        .bind(tier_str(tenant.tier))
        .bind(budget)
        .bind(permissions)
        .bind(metadata)
        .bind(&tenant.allowed_plugin_ids)
        .bind(tenant.app_id.map(|id| id.as_uuid()))
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        // Make sure the tenant has a budget_state row so usage recording
        // works without an extra precondition.
        sqlx::query(
            "INSERT INTO budget_state (tenant_id)
             VALUES ($1)
             ON CONFLICT (tenant_id) DO NOTHING",
        )
        .bind(tenant.id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        Ok(())
    }

    /// Fetch a tenant record. Soft-deleted rows surface as
    /// [`StoreError::TenantNotFound`] — callers that need to inspect deleted
    /// rows should go through [`Self::list_all`] with `include_deleted = true`.
    #[instrument(skip(self))]
    pub async fn get_tenant(&self, id: TenantId) -> Result<TenantRecord, StoreError> {
        let row = sqlx::query(SELECT_TENANT_BY_ID)
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(classify_sqlx)?;

        let Some(row) = row else { return Err(StoreError::TenantNotFound(id)) };
        decode_tenant(&row)
    }

    /// Replace just the `allowed_plugin_ids` column. Soft-deleted tenants
    /// are treated as nonexistent.
    #[instrument(skip(self, allowed_plugin_ids))]
    pub async fn update_allowed_plugins(
        &self,
        id: TenantId,
        allowed_plugin_ids: &[String],
    ) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE tenants SET allowed_plugin_ids = $1
             WHERE id = $2 AND deleted_at IS NULL",
        )
        .bind(allowed_plugin_ids)
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::TenantNotFound(id));
        }
        Ok(())
    }

    /// List tenants with optional filtering by app, name prefix, and
    /// soft-delete status.
    #[instrument(skip(self, filter), fields(app_id = ?filter.app_id, limit = filter.limit))]
    pub async fn list_all(
        &self,
        filter: &TenantListFilter,
    ) -> Result<Vec<TenantRecord>, StoreError> {
        let mut qb = QueryBuilder::<sqlx::Postgres>::new(SELECT_TENANT_COLUMNS);
        qb.push(" FROM tenants WHERE 1 = 1");

        if !filter.include_deleted {
            qb.push(" AND deleted_at IS NULL");
        }
        if let Some(app_id) = filter.app_id {
            qb.push(" AND app_id = ").push_bind(app_id.as_uuid());
        }
        if let Some(ref prefix) = filter.name_prefix {
            // ILIKE with a literal prefix — the `%` is appended on the bind
            // side so any wildcards inside the prefix are escaped by the
            // driver, not interpreted.
            qb.push(" AND name ILIKE ").push_bind(format!("{prefix}%"));
        }

        qb.push(" ORDER BY created_at DESC, id DESC LIMIT ")
            .push_bind(i64::from(filter.limit))
            .push(" OFFSET ")
            .push_bind(i64::from(filter.offset));

        let rows = qb.build().fetch_all(&self.pool).await.map_err(classify_sqlx)?;
        rows.iter().map(decode_tenant).collect()
    }

    /// Soft-delete a tenant. Sets `deleted_at = now()` so future
    /// reads (`get_tenant`, `update_allowed_plugins`, `list_all` without
    /// `include_deleted`) treat it as nonexistent.
    ///
    /// Idempotent in the no-op direction: re-soft-deleting an already
    /// soft-deleted tenant returns `StoreError::TenantNotFound` so admins
    /// don't accidentally bump the timestamp.
    #[instrument(skip(self))]
    pub async fn soft_delete(&self, id: TenantId) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE tenants SET deleted_at = now()
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::TenantNotFound(id));
        }
        Ok(())
    }

    /// Re-attach (or detach with `None`) a tenant to an app. The FK on
    /// `tenants.app_id` enforces app existence; missing apps surface as
    /// [`StoreError::Conflict`] from the FK violation.
    #[instrument(skip(self))]
    pub async fn set_app(&self, id: TenantId, app_id: Option<AppId>) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE tenants SET app_id = $1
             WHERE id = $2 AND deleted_at IS NULL",
        )
        .bind(app_id.map(|id| id.as_uuid()))
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::TenantNotFound(id));
        }
        Ok(())
    }

    /// Replace the per-tenant admin-scope hash. Pass `None` to clear
    /// (the tenant then inherits the global admin token).
    ///
    /// The hash bytes are stored verbatim — callers must SHA-256 the
    /// raw token before invoking this, and the DB CHECK enforces
    /// `octet_length = 32`.
    #[instrument(skip(self, hash))]
    pub async fn set_admin_scope_hash(
        &self,
        id: TenantId,
        hash: Option<&[u8; 32]>,
    ) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE tenants SET admin_scope_hash = $1
             WHERE id = $2 AND deleted_at IS NULL",
        )
        .bind(hash.map(<[u8; 32]>::as_slice))
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::TenantNotFound(id));
        }
        Ok(())
    }

    /// Fetch the per-tenant admin-scope hash. `Ok(None)` when the
    /// tenant inherits the global admin token (no per-tenant scope set).
    #[instrument(skip(self))]
    pub async fn get_admin_scope_hash(&self, id: TenantId) -> Result<Option<[u8; 32]>, StoreError> {
        let row = sqlx::query(
            "SELECT admin_scope_hash FROM tenants
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        let Some(row) = row else { return Err(StoreError::TenantNotFound(id)) };
        let raw: Option<Vec<u8>> = row.try_get("admin_scope_hash").map_err(classify_sqlx)?;
        match raw {
            None => Ok(None),
            Some(bytes) => {
                let arr: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
                    StoreError::Database(format!(
                        "admin_scope_hash for {id} has wrong length: {} bytes",
                        v.len()
                    ))
                })?;
                Ok(Some(arr))
            }
        }
    }

    /// Fetch current budget state. Returns a fresh zero'd state if no row
    /// exists yet (equivalent to "nothing has been billed to this tenant").
    pub async fn get_budget_state(&self, id: TenantId) -> Result<BudgetState, StoreError> {
        let row = sqlx::query(
            "SELECT tokens_used_today, day_rollover_at, threads_active
             FROM budget_state WHERE tenant_id = $1",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else {
            return Ok(BudgetState {
                tokens_used_today: 0,
                day_rollover_at: Utc::now(),
                threads_active: 0,
            });
        };

        let used: i64 = row.try_get("tokens_used_today").map_err(classify_sqlx)?;
        let rollover: DateTime<Utc> = row.try_get("day_rollover_at").map_err(classify_sqlx)?;
        let active: i32 = row.try_get("threads_active").map_err(classify_sqlx)?;

        Ok(BudgetState {
            tokens_used_today: u64::try_from(used).unwrap_or(0),
            day_rollover_at: rollover,
            threads_active: u32::try_from(active).unwrap_or(0),
        })
    }

    /// Add tokens used to the tenant's running day counter.
    ///
    /// If the rollover timestamp has passed, the counter is reset and the
    /// next rollover is scheduled 24h out.
    pub async fn record_usage(&self, id: TenantId, usage: Usage) -> Result<(), StoreError> {
        let total = i64::try_from(usage.total()).unwrap_or(i64::MAX);

        sqlx::query(
            "INSERT INTO budget_state (tenant_id, tokens_used_today)
             VALUES ($1, $2)
             ON CONFLICT (tenant_id) DO UPDATE SET
                tokens_used_today = CASE
                    WHEN budget_state.day_rollover_at <= now()
                        THEN EXCLUDED.tokens_used_today
                    ELSE budget_state.tokens_used_today + EXCLUDED.tokens_used_today
                END,
                day_rollover_at = CASE
                    WHEN budget_state.day_rollover_at <= now()
                        THEN date_trunc('day', now() + interval '1 day')
                    ELSE budget_state.day_rollover_at
                END",
        )
        .bind(id.as_uuid())
        .bind(total)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        Ok(())
    }

    /// C2 — Add this turn's usage to the cumulative per-thread token
    /// counter. The counter is what `get_thread_usage` returns; the
    /// worker loads it at fresh-start so the per-thread budget cap
    /// evaluates against the real total.
    pub async fn record_thread_usage(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
        usage: Usage,
    ) -> Result<(), StoreError> {
        let total = i64::try_from(usage.total()).unwrap_or(i64::MAX);
        sqlx::query(
            "INSERT INTO thread_usage (tenant_id, thread_id, tokens_used)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, thread_id) DO UPDATE SET
                 tokens_used = thread_usage.tokens_used + EXCLUDED.tokens_used",
        )
        .bind(tenant_id.as_uuid())
        .bind(thread_id.as_uuid())
        .bind(total)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        Ok(())
    }

    /// Return the cumulative tokens spent on a given thread. `Ok(0)`
    /// when no row exists (fresh thread).
    pub async fn get_thread_usage(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
    ) -> Result<u64, StoreError> {
        let row = sqlx::query(
            "SELECT tokens_used FROM thread_usage
             WHERE tenant_id = $1 AND thread_id = $2",
        )
        .bind(tenant_id.as_uuid())
        .bind(thread_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        let Some(row) = row else { return Ok(0) };
        let used: i64 = row.try_get("tokens_used").map_err(classify_sqlx)?;
        Ok(u64::try_from(used).unwrap_or(0))
    }
}

const SELECT_TENANT_COLUMNS: &str = "SELECT id, name, tier, budget, permissions, metadata,
            allowed_plugin_ids, app_id, deleted_at, created_at, updated_at";

const SELECT_TENANT_BY_ID: &str = "SELECT id, name, tier, budget, permissions, metadata,
            allowed_plugin_ids, app_id, deleted_at, created_at, updated_at
     FROM tenants WHERE id = $1 AND deleted_at IS NULL";

fn decode_tenant(row: &sqlx::postgres::PgRow) -> Result<TenantRecord, StoreError> {
    let id_val: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let name: String = row.try_get("name").map_err(classify_sqlx)?;
    let tier: String = row.try_get("tier").map_err(classify_sqlx)?;
    let budget: Value = row.try_get("budget").map_err(classify_sqlx)?;
    let permissions: Value = row.try_get("permissions").map_err(classify_sqlx)?;
    let metadata: Value = row.try_get("metadata").map_err(classify_sqlx)?;
    let allowed_plugin_ids: Vec<String> =
        row.try_get("allowed_plugin_ids").map_err(classify_sqlx)?;
    let app_id: Option<uuid::Uuid> = row.try_get("app_id").map_err(classify_sqlx)?;
    let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(classify_sqlx)?;

    Ok(TenantRecord {
        id: TenantId::from_uuid(id_val),
        name,
        tier: tier_from_str(&tier)?,
        budget: serde_json::from_value(budget).map_err(|e| classify_serde(&e))?,
        permissions: serde_json::from_value(permissions).map_err(|e| classify_serde(&e))?,
        metadata: serde_json::from_value(metadata).map_err(|e| classify_serde(&e))?,
        allowed_plugin_ids,
        app_id: app_id.map(AppId::from_uuid),
        deleted_at,
        created_at,
        updated_at,
    })
}

fn tier_str(t: TenantTier) -> &'static str {
    match t {
        TenantTier::Free => "free",
        TenantTier::Pro => "pro",
        TenantTier::Team => "team",
        TenantTier::Enterprise => "enterprise",
    }
}

fn tier_from_str(s: &str) -> Result<TenantTier, StoreError> {
    match s {
        "free" => Ok(TenantTier::Free),
        "pro" => Ok(TenantTier::Pro),
        "team" => Ok(TenantTier::Team),
        "enterprise" => Ok(TenantTier::Enterprise),
        other => Err(StoreError::Serialization(format!("unknown tenant tier: {other}"))),
    }
}
