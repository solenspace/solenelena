//! App-defined plans.
//!
//! A *plan* is a tenant-scoped policy bundle: budget, rate limits, allowed
//! plugins, model-tier selection, autonomy default, cache policy, and
//! arbitrary app metadata. Each tenant (Solen, Hannlys, Omnii, ...) defines
//! its own plans via the admin API; users and workspaces get assigned to a
//! plan via [`PlanAssignment`] rows.
//!
//! At request time the gateway resolves a [`ResolvedPlan`] for the
//! `(tenant, user, workspace)` triple using the most-specific-wins order:
//!
//! 1. exact `(tenant, user, workspace)` match in `plan_assignments`
//! 2. `(tenant, user, NULL)` — the user's default across workspaces
//! 3. `(tenant, NULL, workspace)` — the workspace default for any user
//! 4. the tenant default — the [`Plan`] row with `is_default = true`
//!
//! The `rate_limits`, `tier_models`, and `cache_policy` fields are kept as
//! [`serde_json::Value`] in this crate to avoid a circular dep on
//! `elena-config` and `elena-llm`. Consumers in those crates parse the
//! JSON into their own typed structures at use time.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    autonomy::AutonomyMode,
    id::{PlanAssignmentId, PlanId, TenantId, UserId, WorkspaceId},
    tenant::BudgetLimits,
};

/// Tenant-scoped human-readable identifier for a plan.
///
/// Slugs are arbitrary per-tenant strings (`"starter"`, `"creator-annual"`,
/// `"video-pack-10"`, …) and Elena never pattern-matches on them — they
/// exist for the tenant's own UX and admin tooling.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanSlug(pub String);

impl PlanSlug {
    /// Wrap an owned [`String`] as a slug.
    #[must_use]
    pub fn new(slug: impl Into<String>) -> Self {
        Self(slug.into())
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PlanSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlanSlug({})", self.0)
    }
}

impl std::fmt::Display for PlanSlug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for PlanSlug {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for PlanSlug {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Full plan record as persisted in the `plans` table.
///
/// `Plan` is the admin-facing shape — it carries every field including
/// timestamps and the `is_default` flag. Per-request code uses
/// [`ResolvedPlan`] instead, which omits identity-only fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Stable plan identifier.
    pub id: PlanId,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Tenant-scoped slug (unique with `tenant_id`).
    pub slug: PlanSlug,
    /// Human-readable name for admin UIs.
    pub display_name: String,
    /// True for the tenant's default plan. Exactly one plan per tenant
    /// has this set; enforced by a partial unique index.
    pub is_default: bool,
    /// Budget limits applied to threads under this plan.
    pub budget: BudgetLimits,
    /// Per-plan rate-limit overrides. Opaque [`serde_json::Value`] —
    /// consumers in `elena-store` (the `RateLimiter`) parse the shape.
    /// An empty object means "use system defaults".
    #[serde(default)]
    pub rate_limits: serde_json::Value,
    /// Plan-level allow-list of plugin IDs. Empty means no plan-level
    /// filter; intersected with the tenant and workspace allow-lists.
    #[serde(default)]
    pub allowed_plugin_ids: Vec<String>,
    /// Per-plan override of the model tier-table. `None` means "inherit
    /// the tenant or system-wide `tier_models`". Opaque to this crate;
    /// consumers in `elena-router` parse it.
    #[serde(default)]
    pub tier_models: Option<serde_json::Value>,
    /// Default autonomy mode for new threads under this plan.
    #[serde(default)]
    pub autonomy_default: AutonomyMode,
    /// Per-plan cache-policy overrides. Opaque [`serde_json::Value`];
    /// consumers in `elena-llm` parse the shape. Empty object means
    /// "use system defaults".
    #[serde(default)]
    pub cache_policy: serde_json::Value,
    /// Maximum cascade escalations the router may perform within one
    /// thread under this plan.
    pub max_cascade_escalations: u32,
    /// App-specific metadata. Opaque to Elena — Hannlys may stash a SKU
    /// id here, Solen a billing-period anchor, Omnii a video-pack count.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
    /// When the row was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Per-request projection of a [`Plan`].
///
/// `ResolvedPlan` is what the gateway attaches to [`crate::TenantContext`]
/// after resolving the `(tenant, user, workspace)` triple. It carries only
/// the policy-relevant fields — no `is_default`, no timestamps, no
/// `tenant_id` (the tenant is identified by `TenantContext.tenant_id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedPlan {
    /// Source plan identifier (for audit and metric attribution).
    pub plan_id: PlanId,
    /// Source plan slug.
    pub slug: PlanSlug,
    /// Budget limits applied to this request.
    pub budget: BudgetLimits,
    /// Rate-limit override bundle (opaque; see [`Plan::rate_limits`]).
    #[serde(default)]
    pub rate_limits: serde_json::Value,
    /// Plan-level plugin allow-list (intersected with tenant/workspace).
    #[serde(default)]
    pub allowed_plugin_ids: Vec<String>,
    /// Tier-model override (opaque; see [`Plan::tier_models`]).
    #[serde(default)]
    pub tier_models: Option<serde_json::Value>,
    /// Default autonomy for threads under this plan.
    #[serde(default)]
    pub autonomy_default: AutonomyMode,
    /// Cache-policy override bundle (opaque; see [`Plan::cache_policy`]).
    #[serde(default)]
    pub cache_policy: serde_json::Value,
    /// Maximum cascade escalations under this plan.
    pub max_cascade_escalations: u32,
    /// App-specific metadata.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl ResolvedPlan {
    /// Project a [`Plan`] down to the per-request shape.
    #[must_use]
    pub fn from_plan(plan: &Plan) -> Self {
        Self {
            plan_id: plan.id,
            slug: plan.slug.clone(),
            budget: plan.budget,
            rate_limits: plan.rate_limits.clone(),
            allowed_plugin_ids: plan.allowed_plugin_ids.clone(),
            tier_models: plan.tier_models.clone(),
            autonomy_default: plan.autonomy_default,
            cache_policy: plan.cache_policy.clone(),
            max_cascade_escalations: plan.max_cascade_escalations,
            metadata: plan.metadata.clone(),
        }
    }
}

/// One row in the `plan_assignments` table.
///
/// At least one of `user_id` and `workspace_id` is `Some` — the all-`None`
/// row would shadow the tenant default and is rejected by a CHECK
/// constraint at the DB layer. Resolution is most-specific-wins; see the
/// module-level docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanAssignment {
    /// Stable assignment identifier.
    pub id: PlanAssignmentId,
    /// Tenant the assignment belongs to.
    pub tenant_id: TenantId,
    /// Optional user scope. `None` means the assignment applies to every
    /// user in the named workspace.
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// Optional workspace scope. `None` means the assignment applies to
    /// the named user across every workspace.
    #[serde(default)]
    pub workspace_id: Option<WorkspaceId>,
    /// Plan this assignment selects.
    pub plan_id: PlanId,
    /// When the assignment first becomes effective. Reserved for future
    /// scheduled rollouts; today the resolver treats every row as live
    /// and `effective_at` is purely informational.
    pub effective_at: DateTime<Utc>,
    /// Row creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time (bumped by the trigger).
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> Plan {
        let now = Utc::now();
        Plan {
            id: PlanId::new(),
            tenant_id: TenantId::new(),
            slug: PlanSlug::new("creator-annual"),
            display_name: "Creator (annual)".into(),
            is_default: false,
            budget: BudgetLimits::DEFAULT_PRO,
            rate_limits: serde_json::json!({"tenant_rpm": 600}),
            allowed_plugin_ids: vec!["slack".into(), "notion".into()],
            tier_models: Some(serde_json::json!({"fast": "groq/llama-3.1-8b"})),
            autonomy_default: AutonomyMode::Cautious,
            cache_policy: serde_json::json!({}),
            max_cascade_escalations: 2,
            metadata: HashMap::from([(
                "sku".to_owned(),
                serde_json::Value::String("CREATOR_ANNUAL".to_owned()),
            )]),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn plan_slug_is_serde_transparent() {
        let slug = PlanSlug::new("starter");
        let json = serde_json::to_string(&slug).expect("serialize");
        assert_eq!(json, "\"starter\"");
        let back: PlanSlug = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(slug, back);
    }

    #[test]
    fn plan_round_trips_through_serde() {
        let plan = sample_plan();
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: Plan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(plan, back);
    }

    #[test]
    fn plan_round_trips_arbitrary_slugs() {
        // Slugs are tenant-defined free-form strings — round-trip a few
        // shapes Elena never blesses with built-in handling.
        for raw in ["video-pack-10", "FREE_TIER_v2", "🚀-launch", "with spaces"] {
            let slug = PlanSlug::new(raw);
            let json = serde_json::to_string(&slug).expect("serialize");
            let back: PlanSlug = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back.as_str(), raw);
        }
    }

    #[test]
    fn resolved_plan_drops_identity_only_fields() {
        let plan = sample_plan();
        let resolved = ResolvedPlan::from_plan(&plan);
        assert_eq!(resolved.plan_id, plan.id);
        assert_eq!(resolved.slug, plan.slug);
        assert_eq!(resolved.budget, plan.budget);
        assert_eq!(resolved.rate_limits, plan.rate_limits);
        assert_eq!(resolved.allowed_plugin_ids, plan.allowed_plugin_ids);
        assert_eq!(resolved.tier_models, plan.tier_models);
        assert_eq!(resolved.autonomy_default, plan.autonomy_default);
        assert_eq!(resolved.cache_policy, plan.cache_policy);
        assert_eq!(resolved.max_cascade_escalations, plan.max_cascade_escalations);
        assert_eq!(resolved.metadata, plan.metadata);
    }

    #[test]
    fn resolved_plan_round_trips() {
        let resolved = ResolvedPlan::from_plan(&sample_plan());
        let json = serde_json::to_string(&resolved).expect("serialize");
        let back: ResolvedPlan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(resolved, back);
    }

    #[test]
    fn plan_assignment_round_trips_with_optional_scopes() {
        let now = Utc::now();
        let base = PlanAssignment {
            id: PlanAssignmentId::new(),
            tenant_id: TenantId::new(),
            user_id: None,
            workspace_id: None,
            plan_id: PlanId::new(),
            effective_at: now,
            created_at: now,
            updated_at: now,
        };
        for (user_id, workspace_id) in [
            (Some(UserId::new()), Some(WorkspaceId::new())),
            (Some(UserId::new()), None),
            (None, Some(WorkspaceId::new())),
        ] {
            let row = PlanAssignment { user_id, workspace_id, ..base.clone() };
            let json = serde_json::to_string(&row).expect("serialize");
            let back: PlanAssignment = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(row, back);
        }
    }

    #[test]
    fn plan_metadata_accepts_arbitrary_app_keys() {
        // App-specific metadata is opaque to Elena — round-trip a payload
        // that mixes shapes Solen/Hannlys/Omnii would each store.
        let mut plan = sample_plan();
        plan.metadata.insert(
            "billing_period".into(),
            serde_json::json!({"start": "2026-04-01", "end": "2026-05-01"}),
        );
        plan.metadata.insert("video_credits_remaining".into(), serde_json::json!(7));
        let json = serde_json::to_string(&plan).expect("serialize");
        let back: Plan = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(plan, back);
    }
}
