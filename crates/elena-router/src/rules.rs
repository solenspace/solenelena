//! Heuristic routing rules — pure function over [`RoutingContext`].
//!
//! No ML. The rules were chosen to match the signals the architecture doc
//! lists (§ Crate 4, lines 321-331): message length, conversation depth,
//! tool count, recent tools, tenant tier, and error-recovery count. Each
//! signal nudges the tier up or down; the final clamp respects per-tier
//! operator policy (Enterprise pins Premium; Free pins Fast).

use elena_types::{ModelTier, TenantTier};

/// Context the router sees when picking a tier for a turn.
#[derive(Debug, Clone)]
pub struct RoutingContext<'a> {
    /// The user's most recent message (or empty on the first turn if a
    /// template seeded the thread).
    pub user_message: &'a str,
    /// Completed user→assistant turns so far. Zero on the first call.
    pub conversation_depth: u32,
    /// Number of registered tools the model could call.
    pub tools_available: u32,
    /// Names of tools called in the previous turn (empty on text-only turns).
    pub recent_tool_names: &'a [String],
    /// Tenant's tier — overrides the heuristic at the extremes.
    pub tenant_tier: TenantTier,
    /// How many error retries have already happened for this turn.
    pub error_recovery_count: u32,
}

/// Pick a tier. Pure function — trivially testable.
#[must_use]
pub fn route(ctx: &RoutingContext<'_>) -> ModelTier {
    // Tier-level overrides short-circuit the heuristic.
    match ctx.tenant_tier {
        TenantTier::Enterprise => return ModelTier::Premium,
        TenantTier::Free => return ModelTier::Fast,
        TenantTier::Pro | TenantTier::Team => {}
    }

    // Start from a baseline and nudge up / down based on signals.
    let mut tier = ModelTier::Standard;

    let msg_len = ctx.user_message.chars().count();
    let short = msg_len < 50;
    let long = msg_len > 1_000;
    let mentions_complex = contains_any(
        ctx.user_message,
        &[
            "analyze",
            "analysis",
            "design",
            "architect",
            "compare",
            "explain in detail",
            "reason through",
            "plan",
            "trade-offs",
            "trade off",
            "root cause",
            "optimize",
        ],
    );

    // Escalation signals.
    if long || mentions_complex || ctx.conversation_depth > 15 || ctx.tools_available > 10 {
        tier = ModelTier::Premium;
    }

    // De-escalation: very short, no tools, zero depth, no recent tools.
    if short
        && ctx.tools_available == 0
        && ctx.conversation_depth == 0
        && ctx.recent_tool_names.is_empty()
        && !mentions_complex
    {
        tier = ModelTier::Fast;
    }

    // Error recovery bumps us up one tier per attempt (capped by escalate()).
    for _ in 0..ctx.error_recovery_count.min(2) {
        if let Some(next) = tier.escalate() {
            tier = next;
        }
    }

    tier
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(
        user_message: &str,
        conversation_depth: u32,
        tools_available: u32,
        tenant_tier: TenantTier,
    ) -> RoutingContext<'_> {
        RoutingContext {
            user_message,
            conversation_depth,
            tools_available,
            recent_tool_names: &[],
            tenant_tier,
            error_recovery_count: 0,
        }
    }

    #[test]
    fn enterprise_always_premium() {
        assert_eq!(route(&ctx("hi", 0, 0, TenantTier::Enterprise)), ModelTier::Premium);
        assert_eq!(
            route(&ctx("complex analysis", 0, 0, TenantTier::Enterprise)),
            ModelTier::Premium
        );
    }

    #[test]
    fn free_always_fast() {
        assert_eq!(
            route(&ctx("design the whole system", 100, 20, TenantTier::Free)),
            ModelTier::Fast
        );
    }

    #[test]
    fn short_simple_message_routes_fast() {
        assert_eq!(route(&ctx("hi", 0, 0, TenantTier::Pro)), ModelTier::Fast);
    }

    #[test]
    fn complex_keyword_escalates_to_premium() {
        let c = ctx("Please analyze the tradeoffs of approach A vs B", 0, 3, TenantTier::Pro);
        assert_eq!(route(&c), ModelTier::Premium);
    }

    #[test]
    fn long_conversation_escalates() {
        let c = ctx("just another turn", 20, 5, TenantTier::Pro);
        assert_eq!(route(&c), ModelTier::Premium);
    }

    #[test]
    fn many_tools_escalates() {
        let c = ctx("does it matter?", 2, 12, TenantTier::Pro);
        assert_eq!(route(&c), ModelTier::Premium);
    }

    #[test]
    fn error_recovery_bumps_up_one_tier() {
        let mut c = ctx("hi", 0, 0, TenantTier::Pro);
        c.error_recovery_count = 1;
        assert_eq!(route(&c), ModelTier::Standard);
        c.error_recovery_count = 2;
        assert_eq!(route(&c), ModelTier::Premium);
        c.error_recovery_count = 5; // capped at 2
        assert_eq!(route(&c), ModelTier::Premium);
    }

    #[test]
    fn default_middle_ground_is_standard() {
        let c = ctx("can you help me write a function?", 3, 2, TenantTier::Pro);
        assert_eq!(route(&c), ModelTier::Standard);
    }

    #[test]
    fn team_tier_uses_heuristic_like_pro() {
        assert_eq!(route(&ctx("hi", 0, 0, TenantTier::Team)), ModelTier::Fast);
        assert_eq!(
            route(&ctx("please analyze the scheduler", 2, 3, TenantTier::Team)),
            ModelTier::Premium
        );
    }
}
