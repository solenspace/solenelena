//! JWKS-backed JWT validator.
//!
//! Periodically refreshes a JSON Web Key Set from an issuer URL and
//! verifies JWTs against the current key material. Backward-compatible
//! with static-key callers: wrap a single `DecodingKey` in a one-shot
//! validator via [`JwksValidator::static_key`].
//!
//! Supports `RS256` / `ES256` keys with explicit `kid` mapping. HS\*
//! (shared-secret) stays in `elena-gateway::auth` where it already
//! lives — rotating a shared secret is an operator action, not an
//! in-process refresh.

use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;

/// Operator-facing JWKS validator config.
#[derive(Debug, Clone, Deserialize)]
pub struct JwksValidatorConfig {
    /// JWKS endpoint URL (e.g. `https://auth.example.com/.well-known/jwks.json`).
    pub jwks_url: String,
    /// Refresh interval. Default 10 minutes.
    #[serde(default = "default_refresh_secs")]
    pub refresh_interval_secs: u64,
    /// Required issuer claim.
    pub issuer: String,
    /// Required audience claim.
    pub audience: String,
    /// Leeway (seconds) for `exp` / `nbf`.
    #[serde(default = "default_leeway_secs")]
    pub leeway_seconds: u64,
    /// Allowed algorithms. Default `["RS256"]`.
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
}

const fn default_refresh_secs() -> u64 {
    600
}
const fn default_leeway_secs() -> u64 {
    60
}
fn default_algorithms() -> Vec<String> {
    vec!["RS256".to_owned()]
}

/// Errors raised by [`JwksValidator`].
#[derive(Debug, thiserror::Error)]
pub enum JwksError {
    /// HTTP fetch of the JWKS endpoint failed.
    #[error("fetch JWKS failed: {0}")]
    Fetch(String),
    /// JWKS body didn't parse.
    #[error("parse JWKS failed: {0}")]
    Parse(String),
    /// Validation config referenced an unknown algorithm.
    #[error("unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
    /// The JWT's `kid` didn't match any loaded key.
    #[error("no JWKS key matches kid {0:?}")]
    UnknownKid(String),
    /// Downstream `jsonwebtoken` decode failed.
    #[error("JWT verify failed: {0}")]
    Verify(String),
}

/// Parsed JWKS entry. `kid` is the lookup key.
struct Jwk {
    kid: String,
    key: DecodingKey,
    alg: Algorithm,
}

impl std::fmt::Debug for Jwk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jwk").field("kid", &self.kid).field("alg", &self.alg).finish()
    }
}

/// Cheap-to-clone handle to the rotating JWKS.
#[derive(Clone)]
pub struct JwksValidator {
    inner: Arc<RwLock<ValidatorInner>>,
}

struct ValidatorInner {
    keys: Vec<Jwk>,
    validation: Validation,
}

impl std::fmt::Debug for JwksValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.inner.read().ok();
        let count = guard.as_ref().map(|g| g.keys.len()).unwrap_or(0);
        f.debug_struct("JwksValidator").field("key_count", &count).finish()
    }
}

impl JwksValidator {
    /// Build a single-key validator (no refresh). Useful when the operator
    /// supplies a PEM directly instead of a JWKS URL.
    pub fn static_key(
        key: DecodingKey,
        alg: Algorithm,
        kid: impl Into<String>,
        issuer: &str,
        audience: &str,
        leeway_seconds: u64,
    ) -> Self {
        let mut validation = Validation::new(alg);
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        validation.leeway = leeway_seconds;
        Self {
            inner: Arc::new(RwLock::new(ValidatorInner {
                keys: vec![Jwk { kid: kid.into(), key, alg }],
                validation,
            })),
        }
    }

    /// Refresh the key material once. Operators typically call
    /// [`Self::spawn_refresher`] instead, which loops this at the configured
    /// interval.
    pub async fn refresh(&self, cfg: &JwksValidatorConfig) -> Result<usize, JwksError> {
        let body = reqwest::get(&cfg.jwks_url)
            .await
            .map_err(|e| JwksError::Fetch(e.to_string()))?
            .text()
            .await
            .map_err(|e| JwksError::Fetch(e.to_string()))?;
        let set: JwkSetWire =
            serde_json::from_str(&body).map_err(|e| JwksError::Parse(e.to_string()))?;

        let mut out: Vec<Jwk> = Vec::with_capacity(set.keys.len());
        for wire in set.keys {
            let alg = alg_from_str(&wire.alg)?;
            let key = match wire.kty.as_str() {
                "RSA" => {
                    let (n, e) = (wire.n.as_deref().unwrap_or(""), wire.e.as_deref().unwrap_or(""));
                    DecodingKey::from_rsa_components(n, e)
                        .map_err(|e| JwksError::Parse(e.to_string()))?
                }
                "EC" => {
                    let (x, y) = (wire.x.as_deref().unwrap_or(""), wire.y.as_deref().unwrap_or(""));
                    DecodingKey::from_ec_components(x, y)
                        .map_err(|e| JwksError::Parse(e.to_string()))?
                }
                other => return Err(JwksError::UnsupportedAlgorithm(other.to_owned())),
            };
            out.push(Jwk { kid: wire.kid, key, alg });
        }

        let mut validation = Validation::new(
            cfg.algorithms
                .first()
                .map(String::as_str)
                .map(alg_from_str)
                .transpose()?
                .unwrap_or(Algorithm::RS256),
        );
        validation.set_issuer(&[cfg.issuer.as_str()]);
        validation.set_audience(&[cfg.audience.as_str()]);
        validation.leeway = cfg.leeway_seconds;

        let loaded = out.len();
        if let Ok(mut inner) = self.inner.write() {
            inner.keys = out;
            inner.validation = validation;
        }
        Ok(loaded)
    }

    /// Spawn a background task that refreshes the JWKS at the configured
    /// interval. Returns immediately; the task exits when `cancel` fires.
    pub fn spawn_refresher(
        &self,
        cfg: JwksValidatorConfig,
        cancel: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(cfg.refresh_interval_secs));
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if let Err(err) = this.refresh(&cfg).await {
                            tracing::warn!(?err, "JWKS refresh failed; keeping prior keys");
                        }
                    }
                }
            }
        })
    }

    /// Verify a JWT. Returns the typed claims on success.
    pub fn verify<C>(&self, token: &str) -> Result<C, JwksError>
    where
        C: for<'de> serde::Deserialize<'de>,
    {
        let header =
            jsonwebtoken::decode_header(token).map_err(|e| JwksError::Verify(e.to_string()))?;
        let guard =
            self.inner.read().map_err(|_| JwksError::Verify("JWKS rwlock poisoned".into()))?;
        let jwk = match header.kid.as_deref() {
            Some(kid) => guard
                .keys
                .iter()
                .find(|k| k.kid == kid)
                .ok_or_else(|| JwksError::UnknownKid(kid.to_owned()))?,
            None => guard.keys.first().ok_or_else(|| JwksError::UnknownKid(String::new()))?,
        };
        let mut validation = guard.validation.clone();
        validation.algorithms = vec![jwk.alg];
        let data = jsonwebtoken::decode::<C>(token, &jwk.key, &validation)
            .map_err(|e| JwksError::Verify(e.to_string()))?;
        Ok(data.claims)
    }
}

fn alg_from_str(s: &str) -> Result<Algorithm, JwksError> {
    match s {
        "RS256" => Ok(Algorithm::RS256),
        "RS384" => Ok(Algorithm::RS384),
        "RS512" => Ok(Algorithm::RS512),
        "ES256" => Ok(Algorithm::ES256),
        "ES384" => Ok(Algorithm::ES384),
        other => Err(JwksError::UnsupportedAlgorithm(other.to_owned())),
    }
}

#[derive(Deserialize)]
struct JwkSetWire {
    keys: Vec<JwkWire>,
}

#[derive(Deserialize)]
struct JwkWire {
    kid: String,
    kty: String,
    alg: String,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alg_from_str_covers_common_aliases() {
        assert_eq!(alg_from_str("RS256").unwrap(), Algorithm::RS256);
        assert_eq!(alg_from_str("ES256").unwrap(), Algorithm::ES256);
        assert!(matches!(alg_from_str("HS256").unwrap_err(), JwksError::UnsupportedAlgorithm(_)));
    }
}
