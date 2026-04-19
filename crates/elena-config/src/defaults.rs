//! Default policies applied when a tenant does not override them.
//!
//! Elena ships with one sensible default per tier; operators override via
//! config at deployment time.

use elena_types::{BudgetLimits, ModelId};
use serde::Deserialize;

/// Operator-supplied defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct DefaultsConfig {
    /// Default budget for the free tier.
    #[serde(default = "default_free_budget")]
    pub free_tier_budget: BudgetLimits,

    /// Default budget for the pro tier.
    #[serde(default = "default_pro_budget")]
    pub pro_tier_budget: BudgetLimits,

    /// Default budget for the team tier.
    #[serde(default = "default_team_budget")]
    pub team_tier_budget: BudgetLimits,

    /// Default budget for the enterprise tier.
    #[serde(default = "default_enterprise_budget")]
    pub enterprise_tier_budget: BudgetLimits,

    /// Max concurrent-safe tools that may run in parallel within one batch.
    ///
    /// Matches the reference TS default (10). Tenants can override per
    /// request — this is the floor.
    #[serde(default = "default_max_concurrent_tools")]
    pub max_concurrent_tools: usize,

    /// Default max-turn cap for agentic loops when the caller doesn't
    /// specify.
    #[serde(default = "default_max_turns")]
    pub default_max_turns: u32,

    /// How many recent messages to include when building the LLM context.
    ///
    /// Phase 3 is a simple recency window; Phase 4 replaces this with
    /// embedding-based retrieval.
    #[serde(default = "default_context_window_messages")]
    pub context_window_messages: u32,

    /// Maps [`ModelTier`](elena_types::ModelTier) to a concrete
    /// [`ModelId`](elena_types::ModelId). The router translates its tier
    /// decision to a wire model name through this map.
    #[serde(default)]
    pub tier_models: TierModels,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            free_tier_budget: default_free_budget(),
            pro_tier_budget: default_pro_budget(),
            team_tier_budget: default_team_budget(),
            enterprise_tier_budget: default_enterprise_budget(),
            max_concurrent_tools: default_max_concurrent_tools(),
            default_max_turns: default_max_turns(),
            context_window_messages: default_context_window_messages(),
            tier_models: TierModels::default(),
        }
    }
}

/// Tier → (provider, [`ModelId`]) mapping consulted by
/// [`elena-router`](elena_router).
///
/// Each tier carries its own provider name so operators can, for example,
/// route `fast` to Groq while keeping `premium` on Anthropic. Provider
/// names must match keys in
/// [`ProvidersConfig`](crate::ProvidersConfig).
///
/// Defaults ship with Anthropic's current lineup for backward compat.
/// Operators override at deployment via env or TOML:
///
/// ```sh
/// ELENA_DEFAULTS__TIER_MODELS__FAST__PROVIDER=groq
/// ELENA_DEFAULTS__TIER_MODELS__FAST__MODEL=llama-3.3-70b-versatile
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct TierModels {
    /// Haiku-class model for simple / fast tasks.
    #[serde(default = "default_fast_tier")]
    pub fast: TierEntry,
    /// Sonnet-class model — the everyday default.
    #[serde(default = "default_standard_tier")]
    pub standard: TierEntry,
    /// Opus-class model reserved for complex reasoning.
    #[serde(default = "default_premium_tier")]
    pub premium: TierEntry,
}

/// One tier's routing target: which provider serves it, and which model.
#[derive(Debug, Clone, Deserialize)]
pub struct TierEntry {
    /// Provider name. Matches a key in
    /// [`ProvidersConfig`](crate::ProvidersConfig).
    pub provider: String,
    /// Model identifier (provider-specific wire name).
    pub model: ModelId,
}

impl Default for TierModels {
    fn default() -> Self {
        Self {
            fast: default_fast_tier(),
            standard: default_standard_tier(),
            premium: default_premium_tier(),
        }
    }
}

fn default_fast_tier() -> TierEntry {
    TierEntry { provider: "anthropic".into(), model: default_fast_model() }
}
fn default_standard_tier() -> TierEntry {
    TierEntry { provider: "anthropic".into(), model: default_standard_model() }
}
fn default_premium_tier() -> TierEntry {
    TierEntry { provider: "anthropic".into(), model: default_premium_model() }
}

fn default_fast_model() -> ModelId {
    ModelId::new("claude-haiku-4-5-20251001")
}

fn default_standard_model() -> ModelId {
    ModelId::new("claude-sonnet-4-6")
}

fn default_premium_model() -> ModelId {
    ModelId::new("claude-opus-4-7")
}

const fn default_max_concurrent_tools() -> usize {
    10
}

const fn default_max_turns() -> u32 {
    20
}

const fn default_context_window_messages() -> u32 {
    100
}

fn default_free_budget() -> BudgetLimits {
    BudgetLimits::DEFAULT_FREE
}

fn default_pro_budget() -> BudgetLimits {
    BudgetLimits::DEFAULT_PRO
}

fn default_team_budget() -> BudgetLimits {
    BudgetLimits {
        max_tokens_per_thread: 5_000_000,
        max_tokens_per_day: 100_000_000,
        max_threads_concurrent: 500,
        max_tool_calls_per_turn: 128,
    }
}

fn default_enterprise_budget() -> BudgetLimits {
    BudgetLimits {
        max_tokens_per_thread: u64::MAX / 4,
        max_tokens_per_day: u64::MAX / 4,
        max_threads_concurrent: 100_000,
        max_tool_calls_per_turn: 256,
    }
}
