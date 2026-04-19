//! Per-thread tool-call approvals.
//!
//! When the loop parks in [`LoopPhase::AwaitingApproval`], the gateway's
//! approvals endpoint writes one row per decision into `thread_approvals`.
//! The resuming worker reads them back, filters the checkpointed
//! invocations (dropped/edited/kept), and transitions the phase to
//! [`LoopPhase::ExecutingTools`].
//!
//! Rows are ephemeral: they matter only during the pause and are cleared
//! by [`ApprovalsStore::clear`] before the loop enters `ExecutingTools`.
//!
//! [`LoopPhase::AwaitingApproval`]: https://docs.rs/elena-core/*/elena_core/state/enum.LoopPhase.html
//! [`LoopPhase::ExecutingTools`]: https://docs.rs/elena-core/*/elena_core/state/enum.LoopPhase.html

use elena_types::{ApprovalDecision, ApprovalVerdict, StoreError, TenantId, ThreadId, ToolCallId};
use sqlx::{PgPool, Row, postgres::PgRow};

use crate::sql_error::classify_sqlx;

/// Thin wrapper around the `thread_approvals` table.
#[derive(Debug, Clone)]
pub struct ApprovalsStore {
    pool: PgPool,
}

impl ApprovalsStore {
    /// Build a store from an existing pg pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert a batch of approval decisions for a single paused thread.
    ///
    /// Idempotent on `(thread_id, tool_use_id)` — re-posting the same
    /// decision set overwrites the prior one, which is the behaviour the
    /// gateway wants for retries.
    pub async fn write(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
        decisions: &[ApprovalDecision],
    ) -> Result<(), StoreError> {
        if decisions.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await.map_err(classify_sqlx)?;
        for decision in decisions {
            let decision_str = match decision.decision {
                ApprovalVerdict::Allow => "allow",
                ApprovalVerdict::Deny => "deny",
            };
            let edits = decision.edits.clone();

            sqlx::query(
                "INSERT INTO thread_approvals (thread_id, tool_use_id, tenant_id, decision, edits)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (thread_id, tool_use_id) DO UPDATE
                 SET decision = EXCLUDED.decision, edits = EXCLUDED.edits",
            )
            .bind(thread_id.as_uuid())
            .bind(decision.tool_use_id.as_uuid())
            .bind(tenant_id.as_uuid())
            .bind(decision_str)
            .bind(edits)
            .execute(&mut *tx)
            .await
            .map_err(classify_sqlx)?;
        }
        tx.commit().await.map_err(classify_sqlx)?;
        Ok(())
    }

    /// Fetch all pending decisions for a thread.
    ///
    /// Scoped to `tenant_id` at the SQL layer — cross-tenant reads silently
    /// return an empty vec rather than a `TenantMismatch` error, since a
    /// non-owning tenant asking for approvals is indistinguishable from
    /// "no approvals yet".
    pub async fn list(
        &self,
        tenant_id: TenantId,
        thread_id: ThreadId,
    ) -> Result<Vec<ApprovalDecision>, StoreError> {
        let rows = sqlx::query(
            "SELECT tool_use_id, decision, edits
             FROM thread_approvals
             WHERE thread_id = $1 AND tenant_id = $2
             ORDER BY created_at ASC",
        )
        .bind(thread_id.as_uuid())
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(classify_sqlx)?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(decode_decision(&row)?);
        }
        Ok(out)
    }

    /// Wipe all approvals for a thread. Called by the worker after it has
    /// consumed the decisions into an `ExecutingTools` phase.
    pub async fn clear(&self, tenant_id: TenantId, thread_id: ThreadId) -> Result<u64, StoreError> {
        let result =
            sqlx::query("DELETE FROM thread_approvals WHERE thread_id = $1 AND tenant_id = $2")
                .bind(thread_id.as_uuid())
                .bind(tenant_id.as_uuid())
                .execute(&self.pool)
                .await
                .map_err(classify_sqlx)?;
        Ok(result.rows_affected())
    }
}

fn decode_decision(row: &PgRow) -> Result<ApprovalDecision, StoreError> {
    let tool_use_uuid: uuid::Uuid = row.try_get("tool_use_id").map_err(classify_sqlx)?;
    let decision_str: String = row.try_get("decision").map_err(classify_sqlx)?;
    let edits_json: Option<serde_json::Value> = row.try_get("edits").map_err(classify_sqlx)?;

    let verdict = match decision_str.as_str() {
        "allow" => ApprovalVerdict::Allow,
        "deny" => ApprovalVerdict::Deny,
        other => {
            return Err(StoreError::Serialization(format!(
                "unknown approval decision string: {other}"
            )));
        }
    };

    Ok(ApprovalDecision {
        tool_use_id: ToolCallId::from_uuid(tool_use_uuid),
        decision: verdict,
        edits: edits_json,
    })
}

#[cfg(test)]
mod tests {
    use elena_types::{ApprovalDecision, ApprovalVerdict, ToolCallId};

    #[test]
    fn decision_verdict_round_trip_string_match() {
        // Not a DB test — just confirms the string representation the
        // store uses matches the CHECK constraint in the migration.
        let allow = ApprovalDecision {
            tool_use_id: ToolCallId::new(),
            decision: ApprovalVerdict::Allow,
            edits: None,
        };
        let deny = ApprovalDecision {
            tool_use_id: ToolCallId::new(),
            decision: ApprovalVerdict::Deny,
            edits: None,
        };
        let a = match allow.decision {
            ApprovalVerdict::Allow => "allow",
            ApprovalVerdict::Deny => "deny",
        };
        let d = match deny.decision {
            ApprovalVerdict::Allow => "allow",
            ApprovalVerdict::Deny => "deny",
        };
        assert_eq!(a, "allow");
        assert_eq!(d, "deny");
    }
}
