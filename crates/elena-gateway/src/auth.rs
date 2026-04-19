//! JWT validator + tenant-context extraction.

use std::collections::HashMap;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use elena_types::{
    BudgetLimits, PermissionSet, SessionId, TenantContext, TenantId, TenantTier, ThreadId, UserId,
    WorkspaceId,
};
use jsonwebtoken::{DecodingKey, Validation};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::config::{JwtAlgorithm, JwtConfig};
use crate::error::GatewayError;

/// Expected JWT claim shape. Permissions + budget are intentionally not in
/// the JWT — they're loaded from [`TenantStore`](elena_store::TenantStore)
/// when the session starts, so operator-driven policy changes take effect
/// without reissuing tokens.
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
    pub fn from_config(cfg: &JwtConfig) -> Result<Self, GatewayError> {
        let alg = cfg.algorithm.to_jsonwebtoken();
        let key = if cfg.algorithm.is_symmetric() {
            DecodingKey::from_secret(cfg.secret_or_public_key.expose_secret().as_bytes())
        } else {
            match cfg.algorithm {
                JwtAlgorithm::RS256 | JwtAlgorithm::RS384 | JwtAlgorithm::RS512 => {
                    DecodingKey::from_rsa_pem(cfg.secret_or_public_key.expose_secret().as_bytes())
                        .map_err(|e| GatewayError::Internal(format!("invalid RSA key: {e}")))?
                }
                JwtAlgorithm::ES256 | JwtAlgorithm::ES384 => {
                    DecodingKey::from_ec_pem(cfg.secret_or_public_key.expose_secret().as_bytes())
                        .map_err(|e| GatewayError::Internal(format!("invalid EC key: {e}")))?
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
    pub fn verify(&self, token: &str) -> Result<ElenaJwtClaims, GatewayError> {
        let data = jsonwebtoken::decode::<ElenaJwtClaims>(token, &self.key, &self.validation)
            .map_err(|err| map_jwt_err(&err))?;
        Ok(data.claims)
    }

    /// Verify + project claims into a fresh [`TenantContext`]. Budget +
    /// permissions default to safe values; callers can overlay from the
    /// store if they want per-tenant specifics.
    pub fn verify_to_context(&self, token: &str) -> Result<TenantContext, GatewayError> {
        let claims = self.verify(token)?;
        Ok(TenantContext {
            tenant_id: claims.tenant_id,
            user_id: claims.user_id,
            workspace_id: claims.workspace_id,
            thread_id: ThreadId::new(),
            session_id: claims.session_id.unwrap_or_else(SessionId::new),
            permissions: PermissionSet::default(),
            budget: default_budget_for_tier(claims.tier),
            tier: claims.tier,
            metadata: HashMap::new(),
        })
    }
}

fn default_budget_for_tier(tier: TenantTier) -> BudgetLimits {
    match tier {
        TenantTier::Free => BudgetLimits::DEFAULT_FREE,
        TenantTier::Pro | TenantTier::Team | TenantTier::Enterprise => BudgetLimits::DEFAULT_PRO,
    }
}

fn map_jwt_err(err: &jsonwebtoken::errors::Error) -> GatewayError {
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
    GatewayError::Unauthorized { reason }
}

/// Extract `TenantContext` from an incoming request's Authorization header
/// (or `?token=...` fallback). Mount this as an axum extractor in your
/// route handlers.
#[derive(Debug, Clone)]
pub struct AuthedTenant(pub TenantContext);

impl<S> FromRequestParts<S> for AuthedTenant
where
    S: Send + Sync,
    JwtValidator: FromRef<S>,
{
    type Rejection = GatewayError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let validator = JwtValidator::from_ref(state);
        let token =
            extract_bearer(parts).ok_or(GatewayError::Unauthorized { reason: "missing token" })?;
        let ctx = validator.verify_to_context(&token)?;
        Ok(Self(ctx))
    }
}

fn extract_bearer(parts: &Parts) -> Option<String> {
    // Authorization: Bearer xyz
    if let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                return Some(rest.trim().to_owned());
            }
        }
    }
    // ?token=... fallback (useful for WebSocket upgrade in browsers that
    // can't set Authorization on WS).
    let query = parts.uri.query()?;
    for pair in query.split('&') {
        if let Some(rest) = pair.strip_prefix("token=") {
            let decoded = urldecode(rest);
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

fn urldecode(s: &str) -> String {
    // Minimal %xx decoder — full percent decoding pulls in `form_urlencoded`
    // which would add a dep for a ~10-line helper. JWTs themselves contain
    // only Base64URL (no percent-escape), so literal `%` is unusual.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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
            GatewayError::Unauthorized { reason } => assert_eq!(reason, "token expired"),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[test]
    fn wrong_audience_rejected() {
        let v = validator();
        let mut c = claims(3_600);
        c.aud = "someone-else".into();
        let token = sign(&c, "elena-dev-secret-not-for-production");
        match v.verify(&token).unwrap_err() {
            GatewayError::Unauthorized { reason } => assert_eq!(reason, "wrong audience"),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[test]
    fn bad_signature_rejected() {
        let v = validator();
        let token = sign(&claims(3_600), "wrong-secret");
        match v.verify(&token).unwrap_err() {
            GatewayError::Unauthorized { reason } => assert_eq!(reason, "bad signature"),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    /// C5.5 — Mid-session JWT expiry contract.
    ///
    /// The gateway's `JwtValidator` verifies tokens **per call**: it
    /// caches no state about prior verifications. So a token that was
    /// valid at the WS-upgrade extractor and later expires will be
    /// rejected by the next `verify()` (e.g., when the client tries to
    /// reconnect or open a new thread).
    ///
    /// In v1.0 the WS handler does NOT re-verify the JWT on every frame
    /// — sessions are long-lived by design. The mitigation is short JWT
    /// TTLs (operators set ~1h) so worst-case session staleness is
    /// bounded. This test pins the contract: the validator is stateless
    /// and never lies about expiry. We use a tight leeway (1s) instead
    /// of the production 60s default so the test runs in ~3s.
    #[test]
    fn c55_validator_is_stateless_and_rejects_token_after_it_expires() {
        let cfg = JwtConfig {
            algorithm: JwtAlgorithm::HS256,
            secret_or_public_key: SecretString::from("elena-dev-secret-not-for-production"),
            issuer: "elena-dev".into(),
            audience: "elena-clients".into(),
            leeway_seconds: 1,
        };
        let v = JwtValidator::from_config(&cfg).unwrap();

        // 1. Mint a token valid for the next 1 second.
        let now = chrono::Utc::now().timestamp();
        let mut c = claims(0);
        c.exp = u64::try_from(now + 1).unwrap_or(0);
        let token = sign(&c, "elena-dev-secret-not-for-production");
        let _ok = v.verify(&token).expect("token must be valid right now");

        // 2. After expiry + leeway window passes, the same validator
        //    rejects the same token. No "I previously accepted this"
        //    cache holds it valid.
        std::thread::sleep(std::time::Duration::from_secs(3));
        match v.verify(&token).unwrap_err() {
            GatewayError::Unauthorized { reason } => assert_eq!(reason, "token expired"),
            other => panic!("expected Unauthorized after expiry, got {other:?}"),
        }
    }

    #[test]
    fn verify_to_context_populates_fields() {
        let v = validator();
        let c = claims(3_600);
        let token = sign(&c, "elena-dev-secret-not-for-production");
        let ctx = v.verify_to_context(&token).unwrap();
        assert_eq!(ctx.tenant_id, c.tenant_id);
        assert_eq!(ctx.user_id, c.user_id);
        assert_eq!(ctx.workspace_id, c.workspace_id);
        assert_eq!(ctx.tier, TenantTier::Pro);
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
