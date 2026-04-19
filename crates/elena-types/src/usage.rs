//! Token-usage accounting.
//!
//! Matches the exact shape of Anthropic's `NonNullableUsage` (see
//! `services/api/emptyUsage.ts` in the reference source) so responses can be
//! aggregated field-by-field without transformation.
//!
//! `u64` is used throughout — a busy tenant can exceed `u32::MAX` cache-read
//! tokens in a billing period, and the cost of `u64` is negligible.

use serde::{Deserialize, Serialize};

/// Complete usage breakdown for an LLM request.
///
/// Every field has a zero default so [`Usage::default`] gives you an
/// empty-but-well-formed value suitable for accumulation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    /// Tokens in the prompt (non-cached portion).
    #[serde(default)]
    pub input_tokens: u64,

    /// Tokens emitted by the model.
    #[serde(default)]
    pub output_tokens: u64,

    /// Tokens written to the prompt cache (priced at the higher cache-write rate).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,

    /// Tokens read from the prompt cache (priced at the lower cache-read rate).
    #[serde(default)]
    pub cache_read_input_tokens: u64,

    /// Breakdown of [`Self::cache_creation_input_tokens`] by TTL.
    #[serde(default)]
    pub cache_creation: CacheCreation,

    /// Server-side tool invocations (web search, fetch).
    #[serde(default)]
    pub server_tool_use: ServerToolUse,

    /// Service tier applied to this request.
    #[serde(default)]
    pub service_tier: ServiceTier,

    /// Inference geographic region (e.g., `"us-east-1"`). Empty if unset.
    #[serde(default)]
    pub inference_geo: String,

    /// Heterogeneous per-iteration records (e.g., advisor sub-requests).
    ///
    /// Left as unstructured JSON in Phase 1. Phase 2 (`elena-llm`) will give
    /// this a strongly-typed shape once the call sites that produce iterations
    /// are ported.
    #[serde(default)]
    pub iterations: Vec<serde_json::Value>,

    /// Speed setting used for this request.
    #[serde(default)]
    pub speed: Speed,
}

/// Breakdown of cache-write tokens by TTL.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheCreation {
    /// Tokens written with a 1-hour TTL.
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u64,

    /// Tokens written with a 5-minute TTL.
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u64,
}

/// Server-side tool invocations (billed separately from regular tool use).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerToolUse {
    /// Count of web-search requests executed by the server.
    #[serde(default)]
    pub web_search_requests: u64,

    /// Count of web-fetch requests executed by the server.
    #[serde(default)]
    pub web_fetch_requests: u64,
}

/// Service tier used by the provider for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// Standard priced / standard queue.
    #[default]
    Standard,
    /// Priority queue (higher cost, better availability).
    Priority,
    /// Batch processing tier (lower cost, async delivery).
    Batch,
}

/// Speed setting used for a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Speed {
    /// Standard inference speed.
    #[default]
    Standard,
    /// Optimized (faster) inference, when the selected model supports it.
    Optimized,
}

impl Usage {
    /// Total token count across prompt + output + cache.
    ///
    /// Useful for quick budget checks; for precise cost accounting use the
    /// individual fields against per-tier rates.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
    }

    /// Sum two [`Usage`] values field-by-field.
    ///
    /// [`Self::service_tier`], [`Self::speed`], and [`Self::inference_geo`]
    /// are taken from `other` (the latest reported value wins); iterations
    /// are concatenated.
    #[must_use]
    pub fn combine(mut self, other: Self) -> Self {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;

        self.cache_creation.ephemeral_1h_input_tokens +=
            other.cache_creation.ephemeral_1h_input_tokens;
        self.cache_creation.ephemeral_5m_input_tokens +=
            other.cache_creation.ephemeral_5m_input_tokens;

        self.server_tool_use.web_search_requests += other.server_tool_use.web_search_requests;
        self.server_tool_use.web_fetch_requests += other.server_tool_use.web_fetch_requests;

        self.service_tier = other.service_tier;
        self.speed = other.speed;
        if !other.inference_geo.is_empty() {
            self.inference_geo = other.inference_geo;
        }
        self.iterations.extend(other.iterations);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_zeros() {
        let u = Usage::default();
        assert_eq!(u.total(), 0);
        assert_eq!(u.service_tier, ServiceTier::Standard);
        assert_eq!(u.speed, Speed::Standard);
        assert_eq!(u.inference_geo, "");
        assert!(u.iterations.is_empty());
    }

    #[test]
    fn combine_adds_token_counts() {
        let a = Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: 25,
            cache_read_input_tokens: 75,
            cache_creation: CacheCreation {
                ephemeral_1h_input_tokens: 10,
                ephemeral_5m_input_tokens: 15,
            },
            server_tool_use: ServerToolUse { web_search_requests: 1, web_fetch_requests: 0 },
            ..Default::default()
        };
        let b = Usage {
            input_tokens: 200,
            output_tokens: 100,
            cache_read_input_tokens: 25,
            server_tool_use: ServerToolUse { web_search_requests: 2, web_fetch_requests: 3 },
            ..Default::default()
        };
        let sum = a.combine(b);
        assert_eq!(sum.input_tokens, 300);
        assert_eq!(sum.output_tokens, 150);
        assert_eq!(sum.cache_creation_input_tokens, 25);
        assert_eq!(sum.cache_read_input_tokens, 100);
        assert_eq!(sum.cache_creation.ephemeral_1h_input_tokens, 10);
        assert_eq!(sum.server_tool_use.web_search_requests, 3);
        assert_eq!(sum.server_tool_use.web_fetch_requests, 3);
    }

    #[test]
    fn combine_preserves_nonzero_geo() {
        let a = Usage { inference_geo: "us-east-1".into(), ..Default::default() };
        let b = Usage::default();
        let sum = a.combine(b);
        assert_eq!(sum.inference_geo, "us-east-1");
    }

    #[test]
    fn combine_latest_wins_for_geo_when_set() {
        let a = Usage { inference_geo: "us-east-1".into(), ..Default::default() };
        let b = Usage { inference_geo: "us-west-2".into(), ..Default::default() };
        let sum = a.combine(b);
        assert_eq!(sum.inference_geo, "us-west-2");
    }

    #[test]
    fn full_wire_shape_matches_reference() {
        let json = serde_json::json!({
            "input_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "output_tokens": 0,
            "server_tool_use": { "web_search_requests": 0, "web_fetch_requests": 0 },
            "service_tier": "standard",
            "cache_creation": {
                "ephemeral_1h_input_tokens": 0,
                "ephemeral_5m_input_tokens": 0,
            },
            "inference_geo": "",
            "iterations": [],
            "speed": "standard",
        });
        let usage: Usage = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(usage, Usage::default());
    }

    #[test]
    fn combine_is_commutative_on_numeric_fields() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 5,
            ..Default::default()
        };
        let b = Usage {
            input_tokens: 3,
            output_tokens: 7,
            cache_read_input_tokens: 2,
            ..Default::default()
        };
        let ab = a.clone().combine(b.clone());
        let ba = b.combine(a);
        assert_eq!(ab.input_tokens, ba.input_tokens);
        assert_eq!(ab.output_tokens, ba.output_tokens);
        assert_eq!(ab.cache_read_input_tokens, ba.cache_read_input_tokens);
    }

    #[test]
    fn iterations_are_concatenated() {
        let a = Usage { iterations: vec![serde_json::json!({"n": 1})], ..Default::default() };
        let b = Usage {
            iterations: vec![serde_json::json!({"n": 2}), serde_json::json!({"n": 3})],
            ..Default::default()
        };
        let sum = a.combine(b);
        assert_eq!(sum.iterations.len(), 3);
    }
}
