//! Model-routing configuration (Phase 4).
//!
//! Only one knob today: how many cascade escalations can happen per turn
//! before the loop accepts the current tier's output as-is.

use serde::Deserialize;

/// Phase-4 router settings.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    /// Maximum reactive tier escalations per turn (e.g. `Fast → Standard →
    /// Premium` = 2). Hard-capped at 2 regardless of higher configured
    /// values because `ModelTier` has only three tiers today.
    #[serde(default = "default_max_cascade_escalations")]
    pub max_cascade_escalations: u32,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self { max_cascade_escalations: default_max_cascade_escalations() }
    }
}

const fn default_max_cascade_escalations() -> u32 {
    2
}
