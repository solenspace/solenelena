//! Redis connection configuration.

use secrecy::SecretString;
use serde::Deserialize;

/// Redis connection settings.
#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    /// Full Redis connection URL.
    ///
    /// Format: `redis://host:port` or `redis://:password@host:port/db`.
    /// Cluster mode: `redis-cluster://node1:port,node2:port,...`.
    pub url: SecretString,

    /// Client pool size (default: 10).
    #[serde(default = "default_pool_max")]
    pub pool_max: u32,

    /// TTL for a thread claim in milliseconds (default: 60000).
    ///
    /// A worker holding a thread claim must refresh it within this window
    /// or risk losing the claim to another worker.
    #[serde(default = "default_thread_claim_ttl_ms")]
    pub thread_claim_ttl_ms: u64,
}

const fn default_pool_max() -> u32 {
    10
}
const fn default_thread_claim_ttl_ms() -> u64 {
    60_000
}
