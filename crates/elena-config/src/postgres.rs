//! Postgres connection configuration.

use secrecy::SecretString;
use serde::Deserialize;

/// Postgres connection settings.
#[derive(Debug, Clone, Deserialize)]
pub struct PostgresConfig {
    /// Full Postgres connection URL including credentials.
    ///
    /// Format: `postgres://user:pass@host:port/database`. The password must
    /// be URL-encoded if it contains special characters.
    pub url: SecretString,

    /// Maximum pool size (default: 20).
    #[serde(default = "default_pool_max")]
    pub pool_max: u32,

    /// Minimum pool size (default: 2).
    #[serde(default = "default_pool_min")]
    pub pool_min: u32,

    /// Connection timeout in milliseconds (default: 5000).
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,

    /// Per-statement timeout in milliseconds (default: 30000).
    ///
    /// Applied via `SET statement_timeout = ?` after each connect. Keeps a
    /// runaway query from holding a connection indefinitely.
    #[serde(default = "default_statement_timeout_ms")]
    pub statement_timeout_ms: u64,
}

const fn default_pool_max() -> u32 {
    20
}
const fn default_pool_min() -> u32 {
    2
}
const fn default_connect_timeout_ms() -> u64 {
    5_000
}
const fn default_statement_timeout_ms() -> u64 {
    30_000
}
