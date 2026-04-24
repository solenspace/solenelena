//! Plan-assignment persistence.
//!
//! Holds the rules that map a `(tenant, user?, workspace?)` shape to a
//! [`PlanId`]. Resolution is most-specific-wins, with the tenant default
//! (the [`crate::PlanStore::default_for_tenant`] row) as the final
//! fallback.
//!
//! The `(None, None)` row would shadow the tenant default and is rejected
//! both by a DB-level CHECK constraint and by [`PlanAssignmentStore::upsert`].

use chrono::{DateTime, Utc};
use elena_types::{
    PlanAssignment, PlanAssignmentId, PlanId, ResolvedPlan, StoreError, TenantId, UserId,
    WorkspaceId,
};
use sqlx::{PgPool, Row};

use crate::plan::decode_plan;
use crate::sql_error::classify_sqlx;

/// CRUD over the `plan_assignments` table plus the resolver that the
/// gateway calls per request.
#[derive(Debug, Clone)]
pub struct PlanAssignmentStore {
    pool: PgPool,
}

impl PlanAssignmentStore {
    /// Build a store from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create or replace an assignment.
    ///
    /// Dispatches to the right partial-index conflict target based on
    /// which scopes are set. Rejects the all-NULL row up front rather
    /// than relying on the DB CHECK constraint (faster failure, cleaner
    /// error message).
    pub async fn upsert(
        &self,
        tenant_id: TenantId,
        user_id: Option<UserId>,
        workspace_id: Option<WorkspaceId>,
        plan_id: PlanId,
    ) -> Result<PlanAssignment, StoreError> {
        match (user_id, workspace_id) {
            (None, None) => Err(StoreError::Conflict(
                "plan_assignment requires at least one of user_id or workspace_id".into(),
            )),
            (Some(uid), Some(wid)) => {
                self.upsert_user_workspace(tenant_id, uid, wid, plan_id).await
            }
            (Some(uid), None) => self.upsert_user_only(tenant_id, uid, plan_id).await,
            (None, Some(wid)) => self.upsert_workspace_only(tenant_id, wid, plan_id).await,
        }
    }

    async fn upsert_user_workspace(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        workspace_id: WorkspaceId,
        plan_id: PlanId,
    ) -> Result<PlanAssignment, StoreError> {
        let row = sqlx::query(
            "INSERT INTO plan_assignments (tenant_id, user_id, workspace_id, plan_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, user_id, workspace_id)
             WHERE user_id IS NOT NULL AND workspace_id IS NOT NULL
             DO UPDATE SET plan_id = EXCLUDED.plan_id
             RETURNING id, tenant_id, user_id, workspace_id, plan_id,
                       effective_at, created_at, updated_at",
        )
        .bind(tenant_id.as_uuid())
        .bind(user_id.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(plan_id.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        decode_assignment(&row)
    }

    async fn upsert_user_only(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        plan_id: PlanId,
    ) -> Result<PlanAssignment, StoreError> {
        let row = sqlx::query(
            "INSERT INTO plan_assignments (tenant_id, user_id, workspace_id, plan_id)
             VALUES ($1, $2, NULL, $3)
             ON CONFLICT (tenant_id, user_id)
             WHERE user_id IS NOT NULL AND workspace_id IS NULL
             DO UPDATE SET plan_id = EXCLUDED.plan_id
             RETURNING id, tenant_id, user_id, workspace_id, plan_id,
                       effective_at, created_at, updated_at",
        )
        .bind(tenant_id.as_uuid())
        .bind(user_id.as_uuid())
        .bind(plan_id.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        decode_assignment(&row)
    }

    async fn upsert_workspace_only(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        plan_id: PlanId,
    ) -> Result<PlanAssignment, StoreError> {
        let row = sqlx::query(
            "INSERT INTO plan_assignments (tenant_id, user_id, workspace_id, plan_id)
             VALUES ($1, NULL, $2, $3)
             ON CONFLICT (tenant_id, workspace_id)
             WHERE user_id IS NULL AND workspace_id IS NOT NULL
             DO UPDATE SET plan_id = EXCLUDED.plan_id
             RETURNING id, tenant_id, user_id, workspace_id, plan_id,
                       effective_at, created_at, updated_at",
        )
        .bind(tenant_id.as_uuid())
        .bind(workspace_id.as_uuid())
        .bind(plan_id.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        decode_assignment(&row)
    }

    /// List every assignment for a tenant. Useful for admin UIs and
    /// debugging "which plan resolves for user X in workspace Y" without
    /// running the resolver.
    pub async fn list_by_tenant(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<PlanAssignment>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, user_id, workspace_id, plan_id,
                    effective_at, created_at, updated_at
             FROM plan_assignments WHERE tenant_id = $1
             ORDER BY created_at ASC",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(decode_assignment).collect()
    }

    /// Delete an assignment by its scope. Idempotent — deleting a
    /// non-existent shape is `Ok(false)`.
    pub async fn delete_for_scope(
        &self,
        tenant_id: TenantId,
        user_id: Option<UserId>,
        workspace_id: Option<WorkspaceId>,
    ) -> Result<bool, StoreError> {
        let rows = sqlx::query(
            "DELETE FROM plan_assignments
             WHERE tenant_id = $1
               AND user_id IS NOT DISTINCT FROM $2
               AND workspace_id IS NOT DISTINCT FROM $3",
        )
        .bind(tenant_id.as_uuid())
        .bind(user_id.map(|u| u.as_uuid()))
        .bind(workspace_id.map(|w| w.as_uuid()))
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        Ok(rows.rows_affected() > 0)
    }

    /// Resolve the most-specific [`ResolvedPlan`] for the triple.
    ///
    /// Order of preference:
    /// 1. exact `(tenant, user, workspace)` match
    /// 2. `(tenant, user, NULL)` — user default across workspaces
    /// 3. `(tenant, NULL, workspace)` — workspace default for any user
    /// 4. the tenant default plan (`plans.is_default = true`)
    ///
    /// Returns [`StoreError::Database`] if no assignment matches *and*
    /// the tenant has no default plan. Post-B1.1 every tenant has a
    /// default seeded, so this should only fire for misconfigured
    /// tenants created outside the admin API.
    pub async fn resolve(
        &self,
        tenant_id: TenantId,
        user_id: Option<UserId>,
        workspace_id: Option<WorkspaceId>,
    ) -> Result<ResolvedPlan, StoreError> {
        let row = sqlx::query(
            "WITH chosen AS (
                 SELECT plan_id, 1 AS priority FROM plan_assignments
                  WHERE tenant_id = $1 AND user_id = $2 AND workspace_id = $3
                 UNION ALL
                 SELECT plan_id, 2 FROM plan_assignments
                  WHERE tenant_id = $1 AND user_id = $2 AND workspace_id IS NULL
                 UNION ALL
                 SELECT plan_id, 3 FROM plan_assignments
                  WHERE tenant_id = $1 AND user_id IS NULL AND workspace_id = $3
                 UNION ALL
                 SELECT id, 4 FROM plans
                  WHERE tenant_id = $1 AND is_default = true
                 ORDER BY priority ASC
                 LIMIT 1
             )
             SELECT p.id, p.tenant_id, p.slug, p.display_name, p.is_default,
                    p.budget, p.rate_limits, p.allowed_plugin_ids, p.tier_models,
                    p.autonomy_default, p.cache_policy, p.max_cascade_escalations,
                    p.metadata, p.created_at, p.updated_at
             FROM plans p
             JOIN chosen c ON c.plan_id = p.id",
        )
        .bind(tenant_id.as_uuid())
        .bind(user_id.map(|u| u.as_uuid()))
        .bind(workspace_id.map(|w| w.as_uuid()))
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else {
            return Err(StoreError::Database(format!(
                "tenant {tenant_id} has no plan to resolve (no assignment, no default)"
            )));
        };

        let plan = decode_plan(&row)?;
        Ok(ResolvedPlan::from_plan(&plan))
    }
}

fn decode_assignment(row: &sqlx::postgres::PgRow) -> Result<PlanAssignment, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let tenant_id: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let user_id: Option<uuid::Uuid> = row.try_get("user_id").map_err(classify_sqlx)?;
    let workspace_id: Option<uuid::Uuid> = row.try_get("workspace_id").map_err(classify_sqlx)?;
    let plan_id: uuid::Uuid = row.try_get("plan_id").map_err(classify_sqlx)?;
    let effective_at: DateTime<Utc> = row.try_get("effective_at").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(classify_sqlx)?;

    Ok(PlanAssignment {
        id: PlanAssignmentId::from_uuid(id),
        tenant_id: TenantId::from_uuid(tenant_id),
        user_id: user_id.map(UserId::from_uuid),
        workspace_id: workspace_id.map(WorkspaceId::from_uuid),
        plan_id: PlanId::from_uuid(plan_id),
        effective_at,
        created_at,
        updated_at,
    })
}
