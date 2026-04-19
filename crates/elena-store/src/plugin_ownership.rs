//! Per-tenant plugin ownership — the Hannlys creator isolation path.
//!
//! Every Hannlys "Seed Product" is a plugin. Each Seed belongs to one
//! creator (a tenant). A buyer tenant browsing Hannlys sees a filtered
//! set of plugins scoped to their marketplace view. Solen's shared-tool
//! model lives in the same table: a global utility plugin has no
//! ownership rows, which the registry treats as "visible to everyone".

use elena_types::{StoreError, TenantId};
use sqlx::{PgPool, Row};

use crate::sql_error::classify_sqlx;

/// CRUD over `plugin_ownerships`.
#[derive(Debug, Clone)]
pub struct PluginOwnershipStore {
    pool: PgPool,
}

impl PluginOwnershipStore {
    /// Build a store from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Declare a plugin as owned by one or more tenants. Idempotent.
    ///
    /// Empty `owners` means "global" — we remove any existing ownership
    /// rows so the plugin becomes visible to every tenant.
    pub async fn set_owners(&self, plugin_id: &str, owners: &[TenantId]) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(classify_sqlx)?;
        sqlx::query("DELETE FROM plugin_ownerships WHERE plugin_id = $1")
            .bind(plugin_id)
            .execute(&mut *tx)
            .await
            .map_err(classify_sqlx)?;
        for owner in owners {
            sqlx::query(
                "INSERT INTO plugin_ownerships (plugin_id, tenant_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(plugin_id)
            .bind(owner.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(classify_sqlx)?;
        }
        tx.commit().await.map_err(classify_sqlx)?;
        Ok(())
    }

    /// Is this plugin visible to the given tenant?
    ///
    /// Returns `true` when either the plugin has no ownership rows at
    /// all (global) or the tenant is one of its owners.
    pub async fn is_visible(
        &self,
        plugin_id: &str,
        tenant_id: TenantId,
    ) -> Result<bool, StoreError> {
        // Fast path: one row query with a conditional subselect.
        let row = sqlx::query(
            "SELECT
                NOT EXISTS (SELECT 1 FROM plugin_ownerships WHERE plugin_id = $1)
                OR EXISTS (
                    SELECT 1 FROM plugin_ownerships
                    WHERE plugin_id = $1 AND tenant_id = $2
                ) AS visible",
        )
        .bind(plugin_id)
        .bind(tenant_id.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(classify_sqlx)?;
        row.try_get::<bool, _>("visible").map_err(classify_sqlx)
    }

    /// List the plugin IDs visible to a tenant.
    ///
    /// Includes both global plugins (no ownership rows) and
    /// tenant-owned plugins. Used by `PluginRegistry::tools_for` to
    /// scope the tool list for an LLM request.
    pub async fn visible_plugins(
        &self,
        tenant_id: TenantId,
        candidate_plugin_ids: &[String],
    ) -> Result<Vec<String>, StoreError> {
        if candidate_plugin_ids.is_empty() {
            return Ok(Vec::new());
        }
        // For each candidate, visible iff (no rows) OR (matching row for tenant).
        let rows = sqlx::query(
            "SELECT id FROM unnest($1::text[]) AS candidates(id)
             WHERE
                NOT EXISTS (SELECT 1 FROM plugin_ownerships po WHERE po.plugin_id = id)
                OR EXISTS (
                    SELECT 1 FROM plugin_ownerships po
                    WHERE po.plugin_id = id AND po.tenant_id = $2
                )",
        )
        .bind(candidate_plugin_ids)
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(row.try_get::<String, _>("id").map_err(classify_sqlx)?);
        }
        Ok(out)
    }

    /// Clear all ownership rows for a plugin. Moves it back to global.
    pub async fn clear(&self, plugin_id: &str) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM plugin_ownerships WHERE plugin_id = $1")
            .bind(plugin_id)
            .execute(&self.pool)
            .await
            .map_err(classify_sqlx)?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    // Real behaviour is DB-bound; the integration suite covers it.
    // Here we just confirm the module compiles with expected shape.
    use elena_types::TenantId;

    #[test]
    fn tenant_id_can_be_printed() {
        let id = TenantId::new();
        let _ = format!("{id}");
    }
}
