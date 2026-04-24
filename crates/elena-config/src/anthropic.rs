//! Anthropic Messages API configuration.

use secrecy::SecretString;
use serde::Deserialize;

/// Anthropic client settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicConfig {
    /// API key (header `x-api-key`).
    pub api_key: SecretString,

    /// Base URL override. Defaults to `https://api.anthropic.com`.
    ///
    /// Useful for testing against a wiremock server or a local proxy.
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// `anthropic-version` header value. Defaults to `2023-06-01`.
    #[serde(default = "default_api_version")]
    pub api_version: String,

    /// Per-attempt HTTP timeout in milliseconds. `None` = no per-attempt cap.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,

    /// TCP connect timeout in milliseconds (default: 5000).
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,

    /// Maximum retry attempts per request (default: 10).
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// X8 — Maximum idle HTTP connections kept in the per-host pool.
    /// At high concurrency (~128 in-flight loops × 2 providers = 256
    /// active sockets) the pre-X8 hard-coded `8` thrashed connect/close;
    /// 64 is comfortable for any single-pod throughput we'd realistically
    /// run. Tune up for very high-throughput pods.
    #[serde(default = "default_pool_max_idle_per_host")]
    pub pool_max_idle_per_host: usize,
}

fn default_base_url() -> String {
    "https://api.anthropic.com".to_owned()
}

fn default_api_version() -> String {
    "2023-06-01".to_owned()
}

const fn default_connect_timeout_ms() -> u64 {
    5_000
}

const fn default_max_attempts() -> u32 {
    10
}

const fn default_pool_max_idle_per_host() -> usize {
    64
}
