//! Model tiers and opaque model identifiers.
//!
//! [`ModelId`] is intentionally opaque — Anthropic, Bedrock, Vertex, and
//! Azure Foundry all use different model-name conventions (e.g.,
//! `claude-sonnet-4-6` vs. `anthropic.claude-sonnet-4-6-20260214-v1:0`), and
//! provider-specific parsing belongs in the LLM client, not here.
//!
//! [`ModelTier`] is the routing-level classification the agentic loop uses.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Model capability tier, low to high. Used by the router to select a model
/// for a request and to escalate on cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    /// Cheap/fast models (Haiku-class). Used for simple classification and
    /// formatting tasks.
    Fast,
    /// General-purpose models (Sonnet-class). Default for most tasks.
    Standard,
    /// High-capability models (Opus-class). Used for complex reasoning.
    Premium,
}

impl ModelTier {
    /// Return the next-higher tier, or `None` if already at the top.
    #[must_use]
    pub const fn escalate(self) -> Option<Self> {
        match self {
            Self::Fast => Some(Self::Standard),
            Self::Standard => Some(Self::Premium),
            Self::Premium => None,
        }
    }

    /// Return the next-lower tier, or `None` if already at the bottom.
    #[must_use]
    pub const fn deescalate(self) -> Option<Self> {
        match self {
            Self::Premium => Some(Self::Standard),
            Self::Standard => Some(Self::Fast),
            Self::Fast => None,
        }
    }
}

/// Opaque provider-specific model identifier.
///
/// Parsing / normalization happens in the LLM client crate. This type
/// guarantees nothing beyond "non-empty string."
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    /// Construct a [`ModelId`] from any string-like value.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow as `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for ModelId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for ModelId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalation_is_monotonic() {
        assert_eq!(ModelTier::Fast.escalate(), Some(ModelTier::Standard));
        assert_eq!(ModelTier::Standard.escalate(), Some(ModelTier::Premium));
        assert_eq!(ModelTier::Premium.escalate(), None);
    }

    #[test]
    fn deescalation_is_monotonic() {
        assert_eq!(ModelTier::Premium.deescalate(), Some(ModelTier::Standard));
        assert_eq!(ModelTier::Standard.deescalate(), Some(ModelTier::Fast));
        assert_eq!(ModelTier::Fast.deescalate(), None);
    }

    #[test]
    fn ord_matches_capability() {
        assert!(ModelTier::Fast < ModelTier::Standard);
        assert!(ModelTier::Standard < ModelTier::Premium);
    }

    #[test]
    fn tier_wire_values() {
        assert_eq!(serde_json::to_value(ModelTier::Fast).unwrap(), serde_json::json!("fast"));
        assert_eq!(
            serde_json::to_value(ModelTier::Standard).unwrap(),
            serde_json::json!("standard")
        );
        assert_eq!(serde_json::to_value(ModelTier::Premium).unwrap(), serde_json::json!("premium"));
    }

    #[test]
    fn model_id_is_transparent_string() {
        let id = ModelId::new("claude-sonnet-4-6");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"claude-sonnet-4-6\"");
        let back: ModelId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}
