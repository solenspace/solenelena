//! Workspace store — per-workspace global instructions and plugin
//! allow-list.
//!
//! A workspace is a grouping of threads under a tenant. Two pieces of
//! state are attached to it:
//!
//! - `global_instructions` — a persistent system-prompt fragment injected
//!   on every turn in this workspace (Solen's "workspace instructions"
//!   guardrail).
//! - `allowed_plugin_ids` — an optional allow-list intersected with the
//!   tenant's own allow-list to decide which plugins a thread in this
//!   workspace can call.
//!
//! The `workspaces` table is additive: older threads carry
//! `workspace_id` values that never had a row here, and the gateway
//! treats a missing row as "no overrides".

use chrono::{DateTime, Utc};
use elena_types::{AppId, StoreError, TenantId, WorkspaceId};
use sqlx::{PgPool, QueryBuilder, Row, postgres::PgRow};
use tracing::instrument;

use crate::sql_error::classify_sqlx;

/// Filter passed to [`WorkspaceStore::list_all`].
#[derive(Debug, Clone, Default)]
pub struct WorkspaceListFilter {
    /// Restrict to a single tenant.
    pub tenant_id: Option<TenantId>,
    /// Restrict to workspaces whose owning tenant is attached to this app.
    pub app_id: Option<AppId>,
    /// Page size.
    pub limit: u32,
    /// Pagination offset.
    pub offset: u32,
}

/// Hydrated workspace row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    /// Stable workspace identifier.
    pub id: WorkspaceId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional human-readable name.
    pub name: Option<String>,
    /// System-prompt fragment unconditionally prepended to every turn in
    /// this workspace. Empty string means "no guardrail".
    pub global_instructions: String,
    /// Allow-list of plugin IDs. Empty means "no workspace-level
    /// filter — defer entirely to the tenant's list".
    pub allowed_plugin_ids: Vec<String>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time (bumped by the `workspaces_updated_at` trigger).
    pub updated_at: DateTime<Utc>,
}

/// CRUD wrapper over the `workspaces` table.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    pool: PgPool,
}

impl WorkspaceStore {
    /// Build a store from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create (or upsert) a workspace.
    ///
    /// Idempotent on the `id` — calling `create` twice with the same
    /// `WorkspaceId` updates the existing row in place. Admins re-run the
    /// `register-tenant` script after tweaking instructions and expect
    /// the second call to succeed.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        tenant_id: TenantId,
        name: Option<&str>,
        global_instructions: &str,
        allowed_plugin_ids: &[String],
    ) -> Result<WorkspaceRecord, StoreError> {
        sqlx::query(
            "INSERT INTO workspaces (id, tenant_id, name, global_instructions, allowed_plugin_ids)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO UPDATE
             SET tenant_id           = EXCLUDED.tenant_id,
                 name                = EXCLUDED.name,
                 global_instructions = EXCLUDED.global_instructions,
                 allowed_plugin_ids  = EXCLUDED.allowed_plugin_ids",
        )
        .bind(workspace_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .bind(name)
        .bind(global_instructions)
        .bind(allowed_plugin_ids)
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        self.get(tenant_id, workspace_id).await?.ok_or(StoreError::Database(format!(
            "workspace {workspace_id} vanished immediately after upsert"
        )))
    }

    /// Fetch a workspace by id, scoped to its owning tenant.
    ///
    /// Returns `Ok(None)` if no row exists (caller treats as "no overrides").
    /// Returns `TenantMismatch` if the row exists but belongs to a
    /// different tenant — a cross-tenant read attempt.
    pub async fn get(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
    ) -> Result<Option<WorkspaceRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT id, tenant_id, name, global_instructions, allowed_plugin_ids,
                    created_at, updated_at
             FROM workspaces WHERE id = $1",
        )
        .bind(workspace_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let owner_uuid: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
        let owner = TenantId::from_uuid(owner_uuid);
        if owner != tenant_id {
            return Err(StoreError::TenantMismatch { owner, requested: tenant_id });
        }

        Ok(Some(decode_workspace(&row)?))
    }

    /// Fetch just the system-prompt fragment for a workspace. Returns an
    /// empty string on missing rows or empty instructions — callers are
    /// expected to concatenate unconditionally.
    pub async fn instructions(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
    ) -> Result<String, StoreError> {
        Ok(self
            .get(tenant_id, workspace_id)
            .await?
            .map(|w| w.global_instructions)
            .unwrap_or_default())
    }

    /// Patch just the `global_instructions` column. Used by the admin
    /// `PATCH /admin/v1/workspaces/:id/instructions` route.
    pub async fn update_instructions(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        global_instructions: &str,
    ) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE workspaces SET global_instructions = $1
             WHERE id = $2 AND tenant_id = $3",
        )
        .bind(global_instructions)
        .bind(workspace_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::Database(format!(
                "no workspace {workspace_id} for tenant {tenant_id}"
            )));
        }
        Ok(())
    }

    /// Replace the `allowed_plugin_ids` list. Used by the admin
    /// `PATCH /admin/v1/workspaces/:id/allowed-plugins` route (A4).
    pub async fn update_allowed_plugins(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
        allowed_plugin_ids: &[String],
    ) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "UPDATE workspaces SET allowed_plugin_ids = $1
             WHERE id = $2 AND tenant_id = $3",
        )
        .bind(allowed_plugin_ids)
        .bind(workspace_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::Database(format!(
                "no workspace {workspace_id} for tenant {tenant_id}"
            )));
        }
        Ok(())
    }

    /// List all workspaces for a tenant, ordered by creation time.
    #[instrument(skip(self))]
    pub async fn list_by_tenant(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<WorkspaceRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, name, global_instructions, allowed_plugin_ids,
                    created_at, updated_at
             FROM workspaces WHERE tenant_id = $1
             ORDER BY created_at ASC",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        rows.iter().map(decode_workspace).collect()
    }

    /// List workspaces with optional filtering by tenant and app.
    #[instrument(skip(self, filter))]
    pub async fn list_all(
        &self,
        filter: &WorkspaceListFilter,
    ) -> Result<Vec<WorkspaceRecord>, StoreError> {
        let mut qb = QueryBuilder::<sqlx::Postgres>::new(
            "SELECT w.id, w.tenant_id, w.name, w.global_instructions, w.allowed_plugin_ids,
                    w.created_at, w.updated_at
             FROM workspaces w",
        );

        if filter.app_id.is_some() {
            qb.push(" JOIN tenants t ON t.id = w.tenant_id");
        }

        qb.push(" WHERE 1 = 1");

        if let Some(tenant_id) = filter.tenant_id {
            qb.push(" AND w.tenant_id = ").push_bind(tenant_id.as_uuid());
        }
        if let Some(app_id) = filter.app_id {
            qb.push(" AND t.app_id = ").push_bind(app_id.as_uuid());
            qb.push(" AND t.deleted_at IS NULL");
        }

        qb.push(" ORDER BY w.created_at DESC, w.id DESC LIMIT ")
            .push_bind(i64::from(filter.limit))
            .push(" OFFSET ")
            .push_bind(i64::from(filter.offset));

        let rows = qb.build().fetch_all(&self.pool).await.map_err(classify_sqlx)?;
        rows.iter().map(decode_workspace).collect()
    }

    /// Delete a workspace, scoped to its owning tenant. Returns
    /// [`StoreError::Database`] when no row matched (the handler maps
    /// that to a 404).
    #[instrument(skip(self))]
    pub async fn delete(
        &self,
        tenant_id: TenantId,
        workspace_id: WorkspaceId,
    ) -> Result<(), StoreError> {
        let rows = sqlx::query(
            "DELETE FROM workspaces WHERE id = $1 AND tenant_id = $2",
        )
        .bind(workspace_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        if rows.rows_affected() == 0 {
            return Err(StoreError::Database(format!(
                "no workspace {workspace_id} for tenant {tenant_id}"
            )));
        }
        Ok(())
    }
}

fn decode_workspace(row: &PgRow) -> Result<WorkspaceRecord, StoreError> {
    let id: uuid::Uuid = row.try_get("id").map_err(classify_sqlx)?;
    let tenant_id: uuid::Uuid = row.try_get("tenant_id").map_err(classify_sqlx)?;
    let name: Option<String> = row.try_get("name").map_err(classify_sqlx)?;
    let global_instructions: String = row.try_get("global_instructions").map_err(classify_sqlx)?;
    let allowed_plugin_ids: Vec<String> =
        row.try_get("allowed_plugin_ids").map_err(classify_sqlx)?;
    let created_at: DateTime<Utc> = row.try_get("created_at").map_err(classify_sqlx)?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at").map_err(classify_sqlx)?;

    Ok(WorkspaceRecord {
        id: WorkspaceId::from_uuid(id),
        tenant_id: TenantId::from_uuid(tenant_id),
        name,
        global_instructions,
        allowed_plugin_ids,
        created_at,
        updated_at,
    })
}

#[cfg(test)]
mod tests {
    use elena_types::{TenantId, WorkspaceId};

    use super::WorkspaceRecord;

    #[test]
    fn workspace_record_eq_on_identical_rows() {
        // Minimal construct-and-compare; any full DB testing lives in
        // the integration suite (testcontainers). Keeps the unit build
        // dep-free on running Postgres.
        let id = WorkspaceId::new();
        let tenant_id = TenantId::new();
        let now = chrono::Utc::now();

        let a = WorkspaceRecord {
            id,
            tenant_id,
            name: Some("elena-e2e".into()),
            global_instructions: "be concise".into(),
            allowed_plugin_ids: vec![],
            created_at: now,
            updated_at: now,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
