//! [`GatewayConfig`] — nested config section loaded from TOML / env.
//!
//! Elena-wide config (`postgres`, `redis`, `anthropic`, ...) lives in
//! `elena-config`. The top-level `gateway` section's shape is defined
//! here.

use std::net::SocketAddr;

use serde::Deserialize;

// Q3 — JWT config + algorithm + validator now live in elena-auth.
// Re-exported here so existing elena_gateway::JwtConfig / JwtAlgorithm
// imports keep compiling.
pub use elena_auth::{JwtAlgorithm, JwtConfig};

/// Everything the gateway binary needs to start serving.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    /// `ip:port` to bind the HTTP/WebSocket listener on. Default
    /// `0.0.0.0:8080`.
    #[serde(default = "default_listen_addr")]
    pub listen_addr: SocketAddr,

    /// NATS cluster URL, e.g. `nats://127.0.0.1:4222`.
    pub nats_url: String,

    /// JWT verification settings.
    pub jwt: JwtConfig,

    /// S6 — CORS settings. Empty `allow_origins` (the default) leaves
    /// the gateway with no CORS layer at all — same posture as pre-S6.
    /// Operators that front Elena with a browser-side BFF set explicit
    /// origins here to enable cross-origin requests.
    #[serde(default)]
    pub cors: CorsConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            nats_url: "nats://127.0.0.1:4222".to_owned(),
            jwt: JwtConfig::dev_hs256(),
            cors: CorsConfig::default(),
        }
    }
}

/// CORS allow-origin policy. `allow_origins` is an explicit list of
/// origins (scheme + host + port) that the gateway will echo back in
/// `Access-Control-Allow-Origin`. The wildcard `"*"` is rejected at
/// validation time — operators must enumerate the allowed origins so a
/// browser can't smuggle cookies from an unrelated site.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CorsConfig {
    /// Allowed origins, exact-string match. Empty disables CORS.
    #[serde(default)]
    pub allow_origins: Vec<String>,
}

const fn default_listen_addr() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8080)
}
