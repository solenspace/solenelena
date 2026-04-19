//! Prompt-cache controls.
//!
//! Anthropic's prompt caching attaches `cache_control` markers to individual
//! content blocks. The cache key is a hash of the system array, tool array,
//! and message prefix, *including* the cache-control markers. Mismatched
//! TTL or scope between requests invalidates the cache, so these types are
//! intentionally strict.
//!
//! See `utils/api.ts:72-78` in the reference TypeScript source for the exact
//! wire shape.

use serde::{Deserialize, Serialize};

/// Cache-control marker attached to a content block.
///
/// Wire shape is `{"type":"ephemeral","ttl":"5m","scope":"global"}`. Only
/// `type` is required; `ttl` and `scope` are provider-gated (eligibility is
/// decided by the LLM client, not this crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CacheControl {
    /// The kind of cache. Always [`CacheControlKind::Ephemeral`] today.
    #[serde(rename = "type")]
    pub kind: CacheControlKind,

    /// Cache TTL. Omitted in requests when the provider default is acceptable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ttl: Option<CacheTtl>,

    /// Cache scope. Omitted means per-account scope.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scope: Option<CacheScope>,
}

impl CacheControl {
    /// Construct an ephemeral cache-control with no TTL or scope overrides.
    #[must_use]
    pub const fn ephemeral() -> Self {
        Self { kind: CacheControlKind::Ephemeral, ttl: None, scope: None }
    }

    /// Set the TTL.
    #[must_use]
    pub const fn with_ttl(mut self, ttl: CacheTtl) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Set the scope.
    #[must_use]
    pub const fn with_scope(mut self, scope: CacheScope) -> Self {
        self.scope = Some(scope);
        self
    }
}

/// The kind of cache marker. Anthropic currently supports only `ephemeral`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheControlKind {
    /// Short-lived cache entry (5 minutes or 1 hour, see [`CacheTtl`]).
    Ephemeral,
}

/// Cache TTL. Wire values are `"5m"` and `"1h"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CacheTtl {
    /// Five-minute TTL (the default ephemeral lifetime).
    #[serde(rename = "5m")]
    FiveMin,

    /// One-hour TTL (requires provider-side eligibility).
    #[serde(rename = "1h")]
    OneHour,
}

/// Cache scope. Wire values are `"global"` and `"org"`.
///
/// `Global` enables cross-account sharing (for public prompts). `Org` is the
/// default broader-than-account scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheScope {
    /// Globally shared — used for stable, non-secret system prompts.
    Global,

    /// Organization-wide scope.
    Org,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_cache_control_omits_optional_fields() {
        let cc = CacheControl::ephemeral();
        let json = serde_json::to_value(cc).unwrap();
        assert_eq!(json, serde_json::json!({"type": "ephemeral"}));
    }

    #[test]
    fn full_cache_control_serializes_all_fields() {
        let cc =
            CacheControl::ephemeral().with_ttl(CacheTtl::OneHour).with_scope(CacheScope::Global);
        let json = serde_json::to_value(cc).unwrap();
        assert_eq!(json, serde_json::json!({"type": "ephemeral", "ttl": "1h", "scope": "global"}));
    }

    #[test]
    fn ttl_wire_values() {
        assert_eq!(serde_json::to_value(CacheTtl::FiveMin).unwrap(), serde_json::json!("5m"));
        assert_eq!(serde_json::to_value(CacheTtl::OneHour).unwrap(), serde_json::json!("1h"));
    }

    #[test]
    fn scope_wire_values() {
        assert_eq!(serde_json::to_value(CacheScope::Global).unwrap(), serde_json::json!("global"));
        assert_eq!(serde_json::to_value(CacheScope::Org).unwrap(), serde_json::json!("org"));
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let cc = CacheControl::ephemeral().with_ttl(CacheTtl::FiveMin).with_scope(CacheScope::Org);
        let json = serde_json::to_string(&cc).unwrap();
        let back: CacheControl = serde_json::from_str(&json).unwrap();
        assert_eq!(cc, back);
    }
}
