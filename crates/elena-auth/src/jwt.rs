//! JWT verification + Elena's claim shape.
//!
//! Pre-Q3 this lived in `elena-gateway::auth`; the validator and the
//! claim type are pure logic with no axum dependency, so they belong
//! in the foundation auth crate. The axum extractor that *uses* the
//! validator stays in `elena-gateway::auth` for now (an axum
//! integration concern, not an auth concern).

use std::collections::HashMap;

use elena_types::{SessionId, TenantId, TenantTier, UserId, WorkspaceId};
use jsonwebtoken::{DecodingKey, Validation};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Expected JWT claim shape. Permissions + budget are intentionally not
/// in the JWT — they're loaded from the tenant store when the session
/// starts, so operator-driven policy changes take effect without
/// reissuing tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElenaJwtClaims {
    /// Tenant (application) identifier.
    pub tenant_id: TenantId,
    /// End-user identifier within the tenant.
    pub user_id: UserId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Optional session id; gateway mints one if absent.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Tenant subscription tier.
    pub tier: TenantTier,
    /// Expected `iss`.
    pub iss: String,
    /// Expected `aud`.
    pub aud: String,
    /// Expiration unix seconds.
    pub exp: u64,
    /// Not-before unix seconds.
    #[serde(default)]
    pub nbf: Option<u64>,
}

/// JWT verification configuration. Loaded from the gateway's TOML +
/// env layer.
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
    /// Bridge to `jsonwebtoken`'s enum.
    #[must_use]
    pub fn to_jsonwebtoken(self) -> jsonwebtoken::Algorithm {
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

    /// True for HMAC families (shared-secret).
    #[must_use]
    pub const fn is_symmetric(self) -> bool {
        matches!(self, Self::HS256 | Self::HS384 | Self::HS512)
    }
}

const fn default_leeway() -> u64 {
    60
}

/// JWT verification failures, surfaced as a string-style auth rejection
/// reason for the HTTP layer to translate into 401.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum JwtError {
    /// Claim verification rejected. The string explains why ("token
    /// expired", "wrong audience", etc.) — rendered to the operator
    /// log; clients only see the 401.
    #[error("auth rejected: {reason}")]
    Verification {
        /// Stable short reason tag.
        reason: &'static str,
    },
    /// Building the validator from `JwtConfig` failed — bad PEM or the
    /// like. Surfaces as a 500 / boot error.
    #[error("validator setup: {0}")]
    Setup(String),
}

/// Stateless JWT verifier. Cheap to clone.
#[derive(Clone)]
pub struct JwtValidator {
    key: DecodingKey,
    validation: Validation,
}

impl std::fmt::Debug for JwtValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtValidator").finish_non_exhaustive()
    }
}

impl JwtValidator {
    /// Build a validator from a [`JwtConfig`].
    pub fn from_config(cfg: &JwtConfig) -> Result<Self, JwtError> {
        let alg = cfg.algorithm.to_jsonwebtoken();
        let key = if cfg.algorithm.is_symmetric() {
            DecodingKey::from_secret(cfg.secret_or_public_key.expose_secret().as_bytes())
        } else {
            match cfg.algorithm {
                JwtAlgorithm::RS256 | JwtAlgorithm::RS384 | JwtAlgorithm::RS512 => {
                    DecodingKey::from_rsa_pem(cfg.secret_or_public_key.expose_secret().as_bytes())
                        .map_err(|e| JwtError::Setup(format!("invalid RSA key: {e}")))?
                }
                JwtAlgorithm::ES256 | JwtAlgorithm::ES384 => {
                    DecodingKey::from_ec_pem(cfg.secret_or_public_key.expose_secret().as_bytes())
                        .map_err(|e| JwtError::Setup(format!("invalid EC key: {e}")))?
                }
                _ => unreachable!("symmetric case handled above"),
            }
        };

        let mut validation = Validation::new(alg);
        validation.set_issuer(std::slice::from_ref(&cfg.issuer));
        validation.set_audience(std::slice::from_ref(&cfg.audience));
        validation.leeway = cfg.leeway_seconds;
        validation.validate_exp = true;

        Ok(Self { key, validation })
    }

    /// Verify `token` and return the claims on success.
    pub fn verify(&self, token: &str) -> Result<ElenaJwtClaims, JwtError> {
        let data = jsonwebtoken::decode::<ElenaJwtClaims>(token, &self.key, &self.validation)
            .map_err(|err| map_jwt_err(&err))?;
        Ok(data.claims)
    }

    /// Stamp claims onto a serializable shape and emit defaults for
    /// the per-request fields the JWT doesn't carry. Callers (axum
    /// extractors in elena-gateway) overlay store-resolved policy
    /// (plan, permissions, ...) before handing the result downstream.
    ///
    /// Returns a tuple instead of a `TenantContext` so this crate
    /// stays free of `elena-types::TenantContext`'s axum-shaped
    /// neighbours.
    #[allow(clippy::type_complexity)]
    pub fn verify_to_claims_with_session(
        &self,
        token: &str,
    ) -> Result<(ElenaJwtClaims, SessionId, HashMap<String, serde_json::Value>), JwtError> {
        let claims = self.verify(token)?;
        let session_id = claims.session_id.unwrap_or_else(SessionId::new);
        Ok((claims, session_id, HashMap::new()))
    }
}

fn map_jwt_err(err: &jsonwebtoken::errors::Error) -> JwtError {
    use jsonwebtoken::errors::ErrorKind;
    let reason = match err.kind() {
        ErrorKind::ExpiredSignature => "token expired",
        ErrorKind::ImmatureSignature => "token not yet valid",
        ErrorKind::InvalidAudience => "wrong audience",
        ErrorKind::InvalidIssuer => "wrong issuer",
        ErrorKind::InvalidSignature => "bad signature",
        ErrorKind::InvalidToken | ErrorKind::Base64(_) | ErrorKind::Json(_) => "malformed token",
        _ => "auth rejected",
    };
    JwtError::Verification { reason }
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{EncodingKey, Header, encode};
    use secrecy::SecretString;

    use super::*;

    fn claims(offset: i64) -> ElenaJwtClaims {
        let now = chrono::Utc::now().timestamp();
        ElenaJwtClaims {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            session_id: None,
            tier: TenantTier::Pro,
            iss: "elena-dev".into(),
            aud: "elena-clients".into(),
            exp: u64::try_from(now + offset).unwrap_or(0),
            nbf: None,
        }
    }

    fn sign(claims: &ElenaJwtClaims, secret: &str) -> String {
        encode(
            &Header::new(jsonwebtoken::Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    fn validator() -> JwtValidator {
        JwtValidator::from_config(&JwtConfig::dev_hs256()).unwrap()
    }

    #[test]
    fn valid_token_is_accepted() {
        let v = validator();
        let token = sign(&claims(3_600), "elena-dev-secret-not-for-production");
        let out = v.verify(&token).unwrap();
        assert_eq!(out.iss, "elena-dev");
    }

    #[test]
    fn expired_token_rejected() {
        let v = validator();
        let token = sign(&claims(-3_600), "elena-dev-secret-not-for-production");
        match v.verify(&token).unwrap_err() {
            JwtError::Verification { reason } => assert_eq!(reason, "token expired"),
            other @ JwtError::Setup(_) => panic!("expected Verification, got {other:?}"),
        }
    }

    #[test]
    fn wrong_audience_rejected() {
        let v = validator();
        let mut c = claims(3_600);
        c.aud = "someone-else".into();
        let token = sign(&c, "elena-dev-secret-not-for-production");
        match v.verify(&token).unwrap_err() {
            JwtError::Verification { reason } => assert_eq!(reason, "wrong audience"),
            other @ JwtError::Setup(_) => panic!("expected Verification, got {other:?}"),
        }
    }

    #[test]
    fn bad_signature_rejected() {
        let v = validator();
        let token = sign(&claims(3_600), "wrong-secret");
        match v.verify(&token).unwrap_err() {
            JwtError::Verification { reason } => assert_eq!(reason, "bad signature"),
            other @ JwtError::Setup(_) => panic!("expected Verification, got {other:?}"),
        }
    }

    #[test]
    fn custom_validator_with_different_issuer() {
        let cfg = JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from("other-secret"),
            issuer: "other-issuer".into(),
            audience: "other-audience".into(),
            leeway_seconds: 10,
        };
        let v = JwtValidator::from_config(&cfg).unwrap();
        let mut c = claims(3_600);
        c.iss = "other-issuer".into();
        c.aud = "other-audience".into();
        let token = sign(&c, "other-secret");
        assert!(v.verify(&token).is_ok());
    }
}
