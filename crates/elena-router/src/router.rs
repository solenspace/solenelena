//! [`ModelRouter`] — ties [`rules::route`] + [`cascade::cascade_check`] +
//! tier→`ModelId` resolution into a cohesive object the loop can call.

use elena_config::{TierEntry, TierModels};
use elena_types::ModelTier;

use crate::cascade::{CascadeDecision, CascadeInputs, cascade_check};
use crate::rules::{RoutingContext, route};

/// High-level router the loop consults every turn.
#[derive(Debug, Clone)]
pub struct ModelRouter {
    tier_models: TierModels,
    max_cascade_escalations: u32,
}

impl ModelRouter {
    /// Build a router with an explicit tier→model map and cascade budget.
    #[must_use]
    pub fn new(tier_models: TierModels, max_cascade_escalations: u32) -> Self {
        Self { tier_models, max_cascade_escalations }
    }

    /// Pick the [`ModelTier`] for a turn given its signals.
    #[must_use]
    pub fn route(&self, ctx: &RoutingContext<'_>) -> ModelTier {
        route(ctx)
    }

    /// Consult the cascade signals and decide whether to re-issue at a
    /// higher tier.
    #[must_use]
    pub fn cascade_check(&self, inp: CascadeInputs<'_>) -> CascadeDecision {
        // The inputs carry `max_escalations`, but enforce the router's
        // configured cap here in case a caller left the default.
        let effective = CascadeInputs {
            max_escalations: inp.max_escalations.min(self.max_cascade_escalations),
            ..inp
        };
        cascade_check(effective)
    }

    /// Resolve a tier to its configured `(provider, model)` pair.
    #[must_use]
    pub fn resolve(&self, tier: ModelTier) -> TierEntry {
        match tier {
            ModelTier::Fast => self.tier_models.fast.clone(),
            ModelTier::Standard => self.tier_models.standard.clone(),
            ModelTier::Premium => self.tier_models.premium.clone(),
        }
    }

    /// The underlying tier→model map (for diagnostics / smoke tests).
    #[must_use]
    pub const fn tier_models(&self) -> &TierModels {
        &self.tier_models
    }
}

#[cfg(test)]
mod tests {
    use elena_types::{ModelId, ModelTier, TenantTier};

    use super::*;

    fn router() -> ModelRouter {
        ModelRouter::new(
            TierModels {
                fast: TierEntry { provider: "groq".into(), model: ModelId::new("fast-model") },
                standard: TierEntry {
                    provider: "anthropic".into(),
                    model: ModelId::new("standard-model"),
                },
                premium: TierEntry {
                    provider: "anthropic".into(),
                    model: ModelId::new("premium-model"),
                },
            },
            2,
        )
    }

    #[test]
    fn resolve_returns_correct_entry_per_tier() {
        let r = router();
        let fast = r.resolve(ModelTier::Fast);
        assert_eq!(fast.provider, "groq");
        assert_eq!(fast.model.as_str(), "fast-model");
        let std = r.resolve(ModelTier::Standard);
        assert_eq!(std.provider, "anthropic");
        assert_eq!(std.model.as_str(), "standard-model");
    }

    #[test]
    fn route_delegates_to_heuristic() {
        let r = router();
        let tier = r.route(&RoutingContext {
            user_message: "hi",
            conversation_depth: 0,
            tools_available: 0,
            recent_tool_names: &[],
            tenant_tier: TenantTier::Pro,
            error_recovery_count: 0,
        });
        assert_eq!(tier, ModelTier::Fast);
    }

    #[test]
    fn cascade_check_enforces_configured_cap() {
        let r = router();
        // Caller asked for 5 but the router caps at 2.
        let out = r.cascade_check(CascadeInputs {
            assistant_text: "I cannot.",
            tool_calls: &[],
            registered_tool_names: &[],
            tools_present: false,
            current_tier: ModelTier::Fast,
            escalations_so_far: 2,
            max_escalations: 5,
        });
        assert_eq!(out, CascadeDecision::Accept);
    }
}
