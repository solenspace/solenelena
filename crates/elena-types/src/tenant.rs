//! Tenant context, budgets, and tier definitions.
//!
//! Elena is application-agnostic. A tenant represents one app connected to
//! the backend; the app supplies a [`TenantContext`] with every request, and
//! Elena uses it for isolation, budgeting, permissions, and metadata
//! propagation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    id::{SessionId, TenantId, ThreadId, UserId, WorkspaceId},
    permission::PermissionSet,
};

/// Per-request tenant context.
///
/// Gateway builds this from the client's mTLS cert + JWT and passes it
/// through to the worker and tools. Every store operation and permission
/// check consults this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantContext {
    /// Which tenant (app) this request belongs to.
    pub tenant_id: TenantId,
    /// The end user the request is attributed to.
    pub user_id: UserId,
    /// The workspace the request operates in.
    pub workspace_id: WorkspaceId,
    /// The thread being worked on.
    pub thread_id: ThreadId,
    /// The gateway session that initiated the request.
    pub session_id: SessionId,
    /// Resolved permission rules for this tenant/user/workspace triple.
    pub permissions: PermissionSet,
    /// Budget limits applied to this request.
    pub budget: BudgetLimits,
    /// The tenant's current tier.
    pub tier: TenantTier,
    /// App-specific metadata (opaque to Elena). Kept small — bulk data
    /// belongs in the tenant's own systems.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Tenant subscription tier.
///
/// Tiers map to default budget policies and routing preferences. Exact
/// semantics are set by the operator (Elena does not hard-code which tier
/// gets which models).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TenantTier {
    /// Free / evaluation tier.
    #[default]
    Free,
    /// Paid individual tier.
    Pro,
    /// Paid team tier.
    Team,
    /// Enterprise / custom tier.
    Enterprise,
}

/// Per-tenant budget limits.
///
/// All limits are hard caps. Exceeding a limit surfaces as
/// [`ElenaError::BudgetExceeded`](crate::error::ElenaError::BudgetExceeded)
/// to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetLimits {
    /// Max total tokens (in + out + cache) per thread.
    pub max_tokens_per_thread: u64,
    /// Max total tokens per tenant per day.
    pub max_tokens_per_day: u64,
    /// Max concurrent threads per tenant.
    pub max_threads_concurrent: u32,
    /// Max tool calls per turn.
    pub max_tool_calls_per_turn: u32,
}

impl BudgetLimits {
    /// Default conservative limits for the free tier.
    pub const DEFAULT_FREE: Self = Self {
        max_tokens_per_thread: 100_000,
        max_tokens_per_day: 1_000_000,
        max_threads_concurrent: 5,
        max_tool_calls_per_turn: 32,
    };

    /// Default limits for the pro tier.
    pub const DEFAULT_PRO: Self = Self {
        max_tokens_per_thread: 1_000_000,
        max_tokens_per_day: 20_000_000,
        max_threads_concurrent: 50,
        max_tool_calls_per_turn: 64,
    };
}

impl Default for BudgetLimits {
    fn default() -> Self {
        Self::DEFAULT_FREE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_defaults_to_free() {
        assert_eq!(TenantTier::default(), TenantTier::Free);
    }

    #[test]
    fn budget_defaults_to_free_tier() {
        assert_eq!(BudgetLimits::default(), BudgetLimits::DEFAULT_FREE);
    }

    #[test]
    fn tier_wire_values() {
        assert_eq!(serde_json::to_value(TenantTier::Free).unwrap(), serde_json::json!("free"));
        assert_eq!(serde_json::to_value(TenantTier::Pro).unwrap(), serde_json::json!("pro"));
        assert_eq!(serde_json::to_value(TenantTier::Team).unwrap(), serde_json::json!("team"));
        assert_eq!(
            serde_json::to_value(TenantTier::Enterprise).unwrap(),
            serde_json::json!("enterprise")
        );
    }
}
