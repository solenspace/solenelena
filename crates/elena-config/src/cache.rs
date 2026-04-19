//! Prompt-cache configuration.

use serde::Deserialize;

/// Operator-supplied prompt-cache policy.
///
/// Controls which requests are eligible for the 1-hour TTL variant of
/// Anthropic's ephemeral cache. Elena ships with no entries by default;
/// operators opt apps in via `cache.allowlist` in TOML or
/// `ELENA_CACHE__ALLOWLIST` (JSON array).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CacheConfig {
    /// Allowlist of query-source patterns eligible for 1-hour TTL.
    ///
    /// A trailing `*` means prefix match; otherwise exact match.
    /// Example: `["repl_main_thread*", "sdk"]`.
    #[serde(default)]
    pub allowlist: Vec<String>,
}
