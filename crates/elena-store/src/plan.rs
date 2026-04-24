//! Plan persistence — tenant-scoped policy bundles that replace the
//! hard-coded [`elena_types::TenantTier`] enum.
//!
//! Each plan row carries a budget, a rate-limits bundle, an allowed-plugins
//! list, an optional tier-models override, an autonomy default, a cache
//! policy, a max-cascade-escalations cap, and arbitrary app metadata. The
//! per-request projection (`ResolvedPlan`) is built by
//! [`crate::PlanAssignmentStore::resolve`].

use chrono::{DateTime, Utc};
use elena_types::{AutonomyMode, BudgetLimits, Plan, PlanId, PlanSlug, StoreError, TenantId};
use serde_json::Value;
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::sql_error::{classify_serde, classify_sqlx};

/// CRUD over the `plans` table.
#[derive(Debug, Clone)]
pub struct PlanStore {
    pool: PgPool,
}

impl PlanStore {
    /// Build a store from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create or replace a plan.
    ///
    /// Idempotent on `id` — reusing a `PlanId` updates the existing row.
    /// For tenant-scoped slug uniqueness the underlying unique index will
    /// surface as [`StoreError::Conflict`] when an admin tries to assign a
    /// slug already taken by a different plan in the same tenant.
    pub async fn upsert(&self, plan: &Plan) -> Result<(), StoreError> {
        let budget = serde_json::to_value(plan.budget).map_err(|e| classify_serde(&e))?;
        let metadata = serde_json::to_value(&plan.metadata).map_err(|e| classify_serde(&e))?;

        sqlx::query(
            "INSERT INTO plans (
                 id, tenant_id, slug, display_name, is_default,
                 budget, rate_limits, allowed_plugin_ids, tier_models,
                 autonomy_default, cache_policy, max_cascade_escalations, metadata
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             ON CONFLICT (id) DO UPDATE SET
                 tenant_id               = EXCLUDED.tenant_id,
                 slug                    = EXCLUDED.slug,
                 display_name            = EXCLUDED.display_name,
                 is_default              = EXCLUDED.is_default,
                 budget                  = EXCLUDED.budget,
                 rate_limits             = EXCLUDED.rate_limits,
                 allowed_plugin_ids      = EXCLUDED.allowed_plugin_ids,
                 tier_models             = EXCLUDED.tier_models,
                 autonomy_default        = EXCLUDED.autonomy_default,
                 cache_policy            = EXCLUDED.cache_policy,
                 max_cascade_escalations = EXCLUDED.max_cascade_escalations,
                 metadata                = EXCLUDED.metadata",
        )
        .bind(plan.id.as_uuid())
        .bind(plan.tenant_id.as_uuid())
        .bind(plan.slug.as_str())
        .bind(&plan.display_name)
        .bind(plan.is_default)
        .bind(budget)
        .bind(&plan.rate_limits)
        .bind(&plan.allowed_plugin_ids)
        .bind(plan.tier_models.as_ref())
        .bind(plan.autonomy_default.as_str())
        .bind(&plan.cache_policy)
        .bind(i64::from(plan.max_cascade_escalations))
        .bind(metadata)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        Ok(())
    }

    /// Fetch a plan by id, scoped to its owning tenant.
    ///
    /// Returns `Ok(None)` if no row exists; returns
    /// [`StoreError::TenantMismatch`] if a row exists under a different
    /// tenant — surfaces a cross-tenant lookup attempt rather than a
    /// silent "not found".
    pub async fn get(
        &self,
        tenant_id: TenantId,
        plan_id: PlanId,
    ) -> Result<Option<Plan>, StoreError> {
        let row = sqlx::query(SELECT_PLAN_COLUMNS_BY_ID)
            .bind(plan_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(classify_sqlx)?;

        let Some(row) = row else { return Ok(None) };

        let owner_uuid: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
        let owner = TenantId::from_uuid(owner_uuid);
        if owner != tenant_id {
            return Err(StoreError::TenantMismatch { owner, requested: tenant_id });
        }

        Ok(Some(decode_plan(&row)?))
    }

    /// Fetch a plan by tenant-scoped slug. Returns `Ok(None)` for unknown
    /// slugs — no tenant-mismatch path because slugs are scoped to the
    /// tenant by the WHERE clause.
    pub async fn get_by_slug(
        &self,
        tenant_id: TenantId,
        slug: &PlanSlug,
    ) -> Result<Option<Plan>, StoreError> {
        let row = sqlx::query(
            "SELECT id, tenant_id, slug, display_name, is_default,
                    budget, rate_limits, allowed_plugin_ids, tier_models,
                    autonomy_default, cache_policy, max_cascade_escalations,
                    metadata, created_at, updated_at
             FROM plans WHERE tenant_id = $1 AND slug = $2",
        )
        .bind(tenant_id.as_uuid())
        .bind(slug.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        row.as_ref().map(decode_plan).transpose()
    }

    /// List every plan owned by a tenant, ordered by creation time.
    pub async fn list_by_tenant(&self, tenant_id: TenantId) -> Result<Vec<Plan>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, slug, display_name, is_default,
                    budget, rate_limits, allowed_plugin_ids, tier_models,
                    autonomy_default, cache_policy, max_cascade_escalations,
                    metadata, created_at, updated_at
             FROM plans WHERE tenant_id = $1
             ORDER BY created_at ASC",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(decode_plan).collect()
    }

    /// Delete a plan. Surfaces [`StoreError::Conflict`] when the plan is
    /// referenced by an assignment (the FK on `plan_assignments.plan_id`
    /// is `ON DELETE RESTRICT`), so admins must reassign before deleting.
    pub async fn delete(&self, tenant_id: TenantId, plan_id: PlanId) -> Result<(), StoreError> {
        let rows = sqlx::query("DELETE FROM plans WHERE id = $1 AND tenant_id = $2")
            .bind(plan_id.as_uuid())
            .bind(tenant_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(classify_sqlx)?;

        if rows.rows_affected() == 0 {
            return Err(StoreError::Database(format!("no plan {plan_id} for tenant {tenant_id}")));
        }
        Ok(())
    }

    /// Fetch the tenant's default plan (the row with `is_default = true`).
    ///
    /// Returns [`StoreError::Database`] if the tenant has no default —
    /// this should be impossible after the B1.1 backfill, so a missing
    /// default is treated as a hard data-integrity failure rather than
    /// silently degraded.
    pub async fn default_for_tenant(&self, tenant_id: TenantId) -> Result<Plan, StoreError> {
        let row = sqlx::query(
            "SELECT id, tenant_id, slug, display_name, is_default,
                    budget, rate_limits, allowed_plugin_ids, tier_models,
                    autonomy_default, cache_policy, max_cascade_escalations,
                    metadata, created_at, updated_at
             FROM plans WHERE tenant_id = $1 AND is_default = true",
        )
        .bind(tenant_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else {
            return Err(StoreError::Database(format!("tenant {tenant_id} has no default plan")));
        };
        decode_plan(&row)
    }

    /// Atomically swap the default to `plan_id`.
    ///
    /// Implemented as a single `UPDATE ... SET is_default = (id = $1)`:
    /// Postgres defers unique-constraint checks until end-of-statement
    /// for in-place row updates, so the partial unique index
    /// `plans_one_default_per_tenant` is respected even when the previous
    /// default row and the new default row both transition during the
    /// same statement.
    pub async fn set_default(
        &self,
        tenant_id: TenantId,
        plan_id: PlanId,
    ) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE plans
             SET is_default = (id = $1)
             WHERE tenant_id = $2",
        )
        .bind(plan_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        if rows.rows_affected() == 0 {
            return Err(StoreError::Database(format!("no plans for tenant {tenant_id}")));
        }

        // Verify the requested plan actually flipped to true — guards
        // against the case where `plan_id` belongs to a different tenant
        // (the WHERE filtered it out, so the update succeeded for other
        // rows but the target is still false).
        let confirmed: Option<bool> =
            sqlx::query("SELECT is_default FROM plans WHERE id = $1 AND tenant_id = $2")
                .bind(plan_id.as_uuid())
                .bind(tenant_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(classify_sqlx)?
                .map(|r| r.try_get("is_default"))
                .transpose()
                .map_err(classify_sqlx)?;

        match confirmed {
            Some(true) => Ok(()),
            Some(false) => Err(StoreError::Database(format!(
                "set_default raced and lost: plan {plan_id} is not default"
            ))),
            None => Err(StoreError::Database(format!("no plan {plan_id} for tenant {tenant_id}"))),
        }
    }
}

const SELECT_PLAN_COLUMNS_BY_ID: &str = "SELECT id, tenant_id, slug, display_name, is_default,
            budget, rate_limits, allowed_plugin_ids, tier_models,
            autonomy_default, cache_policy, max_cascade_escalations,
            metadata, created_at, updated_at
     FROM plans WHERE id = $1";

pub(crate) fn decode_plan(row: &PgRow) -> Result<Plan, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let tenant_id: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let slug: String = row.try_get("slug").map_err(classify_sqlx)?;
    let display_name: String = row.try_get("display_name").map_err(classify_sqlx)?;
    let is_default: bool = row.try_get("is_default").map_err(classify_sqlx)?;
    let budget: Value = row.try_get("budget").map_err(classify_sqlx)?;
    let rate_limits: Value = row.try_get("rate_limits").map_err(classify_sqlx)?;
    let allowed_plugin_ids: Vec<String> =
        row.try_get("allowed_plugin_ids").map_err(classify_sqlx)?;
    let tier_models: Option<Value> = row.try_get("tier_models").map_err(classify_sqlx)?;
    let autonomy_default: String = row.try_get("autonomy_default").map_err(classify_sqlx)?;
    let cache_policy: Value = row.try_get("cache_policy").map_err(classify_sqlx)?;
    let max_cascade_escalations: i32 =
        row.try_get("max_cascade_escalations").map_err(classify_sqlx)?;
    let metadata: Value = row.try_get("metadata").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(classify_sqlx)?;

    let budget: BudgetLimits = serde_json::from_value(budget).map_err(|e| classify_serde(&e))?;
    let metadata = serde_json::from_value(metadata).map_err(|e| classify_serde(&e))?;
    let max_cascade_escalations = u32::try_from(max_cascade_escalations).map_err(|_| {
        StoreError::Serialization(format!(
            "max_cascade_escalations {max_cascade_escalations} out of u32 range"
        ))
    })?;

    Ok(Plan {
        id: PlanId::from_uuid(id),
        tenant_id: TenantId::from_uuid(tenant_id),
        slug: PlanSlug::new(slug),
        display_name,
        is_default,
        budget,
        rate_limits,
        allowed_plugin_ids,
        tier_models,
        autonomy_default: AutonomyMode::from_str_or_default(&autonomy_default),
        cache_policy,
        max_cascade_escalations,
        metadata,
        created_at,
        updated_at,
    })
}
