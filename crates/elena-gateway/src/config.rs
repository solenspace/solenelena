//! [`GatewayConfig`] — nested config section loaded from TOML / env.
//!
//! Elena-wide config (`postgres`, `redis`, `anthropic`, ...) lives in
//! `elena-config`. Phase 5 adds a top-level `gateway` section whose shape
//! is defined here.

use std::net::SocketAddr;

use secrecy::SecretString;
use serde::Deserialize;

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
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            nats_url: "nats://127.0.0.1:4222".to_owned(),
            jwt: JwtConfig::dev_hs256(),
        }
    }
}

/// How the gateway verifies JWTs arriving on the
/// `Authorization: Bearer ...` header or `?token=` query parameter.
#[derive(Debug, Clone, Deserialize)]
pub struct JwtConfig {
    /// Symmetric HS* or asymmetric RS*/ES* algorithm.
    pub algorithm: JwtAlgorithm,

    /// Shared secret (HS*) or PEM-encoded public key (RS*/ES*).
    pub secret_or_public_key: SecretString,

    /// Expected `iss` claim.
    pub issuer: String,

    /// Expected `aud` claim.
    pub audience: String,

    /// Clock-skew leeway in seconds. Default 60.
    #[serde(default = "default_leeway")]
    pub leeway_seconds: u64,
}

impl JwtConfig {
    /// Throwaway HS256 config for tests / local dev. NOT for production —
    /// the secret is a well-known placeholder.
    #[must_use]
    pub fn dev_hs256() -> Self {
        Self {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from("elena-dev-secret-not-for-production"),
            issuer: "elena-dev".to_owned(),
            audience: "elena-clients".to_owned(),
            leeway_seconds: default_leeway(),
        }
    }
}

/// Signing algorithms the gateway supports.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
#[allow(clippy::upper_case_acronyms)]
pub enum JwtAlgorithm {
    /// HMAC-SHA256 (symmetric; good for dev + internal tokens).
    HS256,
    /// HMAC-SHA384.
    HS384,
    /// HMAC-SHA512.
    HS512,
    /// RSA-PKCS1-v1_5 with SHA256 (PEM public key).
    RS256,
    /// RSA-PKCS1-v1_5 with SHA384.
    RS384,
    /// RSA-PKCS1-v1_5 with SHA512.
    RS512,
    /// ECDSA P-256 with SHA256.
    ES256,
    /// ECDSA P-384 with SHA384.
    ES384,
}

impl JwtAlgorithm {
    pub(crate) fn to_jsonwebtoken(self) -> jsonwebtoken::Algorithm {
        match self {
            Self::HS256 => jsonwebtoken::Algorithm::HS256,
            Self::HS384 => jsonwebtoken::Algorithm::HS384,
            Self::HS512 => jsonwebtoken::Algorithm::HS512,
            Self::RS256 => jsonwebtoken::Algorithm::RS256,
            Self::RS384 => jsonwebtoken::Algorithm::RS384,
            Self::RS512 => jsonwebtoken::Algorithm::RS512,
            Self::ES256 => jsonwebtoken::Algorithm::ES256,
            Self::ES384 => jsonwebtoken::Algorithm::ES384,
        }
    }

    pub(crate) const fn is_symmetric(self) -> bool {
        matches!(self, Self::HS256 | Self::HS384 | Self::HS512)
    }
}

const fn default_listen_addr() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8080)
}

const fn default_leeway() -> u64 {
    60
}
