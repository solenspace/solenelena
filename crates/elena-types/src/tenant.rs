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
    plan::ResolvedPlan,
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
    ///
    /// **Deprecated in favour of [`Self::plan`].** Kept for the B1
    /// transition window so consumers that haven't migrated still
    /// compile. Will be removed in B1.6.
    pub tier: TenantTier,
    /// Resolved app-defined plan for this `(tenant, user, workspace)`
    /// triple. `None` when the gateway couldn't resolve one (e.g. the
    /// store was unreachable, or the request bypassed the gateway in a
    /// test). Consumers that rely on the plan should fall back to
    /// [`Self::tier`] / [`Self::budget`] when this is `None` during the
    /// B1 transition window.
    #[serde(default)]
    pub plan: Option<ResolvedPlan>,
    /// App-specific metadata (opaque to Elena). Kept small — bulk data
    /// belongs in the tenant's own systems.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl TenantContext {
    /// Budget that should actually be enforced for this request.
    ///
    /// Prefers the resolved plan's budget when set; falls back to the
    /// legacy [`Self::budget`] field during the B1 transition window.
    /// Once B1.6 removes [`TenantTier`] and the tier-derived budget
    /// path, this becomes the single source of truth.
    #[must_use]
    pub fn effective_budget(&self) -> BudgetLimits {
        self.plan.as_ref().map_or(self.budget, |p| p.budget)
    }

    /// Plan-level plugin allow-list, or empty if no plan is resolved.
    ///
    /// Empty means "no plan-level filter" — callers intersect this with
    /// the tenant + workspace allow-lists. The empty case naturally
    /// falls through to the prior behaviour (tenant + workspace are the
    /// only filters) when [`Self::plan`] is `None`.
    #[must_use]
    pub fn plan_allowed_plugins(&self) -> &[String] {
        self.plan.as_ref().map_or(&[] as &[String], |p| &p.allowed_plugin_ids)
    }
}

/// Tenant subscription tier.
///
/// Tiers map to default budget policies and routing preferences. Exact
/// semantics are set by the operator (Elena does not hard-code which tier
/// gets which models).
#[deprecated(note = "B1 — use admin-defined Plans (elena_types::Plan / ResolvedPlan) instead. \
            TenantTier survives only for the JWT-claim transition window and will \
            be removed once BFFs migrate to plan-aware claims.")]
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
    #[deprecated(note = "B1 — operators define their own budgets per Plan. \
                The DEFAULT_FREE/PRO constants survive only for tests and the \
                pre-B1 default-budget-for-tier path; they will be removed once \
                every consumer reads from the resolved plan.")]
    pub const DEFAULT_FREE: Self = Self {
        max_tokens_per_thread: 100_000,
        max_tokens_per_day: 1_000_000,
        max_threads_concurrent: 5,
        max_tool_calls_per_turn: 32,
    };

    /// Default limits for the pro tier.
    #[deprecated(note = "B1 — see DEFAULT_FREE")]
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
    use crate::id::PlanId;
    use crate::plan::{PlanSlug, ResolvedPlan};

    use super::*;

    fn empty_ctx() -> TenantContext {
        TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::DEFAULT_FREE,
            tier: TenantTier::Free,
            plan: None,
            metadata: HashMap::new(),
        }
    }

    fn pro_plan() -> ResolvedPlan {
        ResolvedPlan {
            plan_id: PlanId::new(),
            slug: PlanSlug::new("creator"),
            budget: BudgetLimits::DEFAULT_PRO,
            rate_limits: serde_json::json!({}),
            allowed_plugin_ids: vec!["slack".into(), "notion".into()],
            tier_models: None,
            autonomy_default: crate::AutonomyMode::Moderate,
            cache_policy: serde_json::json!({}),
            max_cascade_escalations: 1,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn effective_budget_falls_back_to_legacy_field_when_no_plan() {
        let ctx = empty_ctx();
        assert_eq!(ctx.effective_budget(), BudgetLimits::DEFAULT_FREE);
    }

    #[test]
    fn effective_budget_prefers_plan_when_resolved() {
        let mut ctx = empty_ctx();
        ctx.plan = Some(pro_plan());
        // Plan budget overrides the legacy free-tier budget.
        assert_eq!(ctx.effective_budget(), BudgetLimits::DEFAULT_PRO);
    }

    #[test]
    fn plan_allowed_plugins_is_empty_without_plan() {
        let ctx = empty_ctx();
        assert!(ctx.plan_allowed_plugins().is_empty());
    }

    #[test]
    fn plan_allowed_plugins_returns_plan_list() {
        let mut ctx = empty_ctx();
        ctx.plan = Some(pro_plan());
        assert_eq!(ctx.plan_allowed_plugins(), &["slack".to_owned(), "notion".to_owned()]);
    }

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
