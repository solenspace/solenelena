//! JWT validator + tenant-context extraction.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use elena_auth::JwtError;
use elena_store::Store;
use elena_types::{BudgetLimits, PermissionSet, TenantContext, TenantTier, ThreadId};

use crate::error::GatewayError;

/// Q3 — Validator + claims live in elena-auth; re-exported here so
/// existing `elena_gateway::JwtValidator` / `ElenaJwtClaims` imports
/// keep compiling without elena-auth becoming an explicit dep at
/// every call site.
pub use elena_auth::{ElenaJwtClaims, JwtValidator};

fn default_budget_for_tier(tier: TenantTier) -> BudgetLimits {
    match tier {
        TenantTier::Free => BudgetLimits::DEFAULT_FREE,
        TenantTier::Pro | TenantTier::Team | TenantTier::Enterprise => BudgetLimits::DEFAULT_PRO,
    }
}

impl From<JwtError> for GatewayError {
    fn from(err: JwtError) -> Self {
        match err {
            JwtError::Verification { reason } => Self::Unauthorized { reason },
            JwtError::Setup(message) => Self::Internal(message),
        }
    }
}

/// Verify + project claims into a fresh [`TenantContext`]. Budget +
/// permissions default to safe values; callers overlay store-resolved
/// policy via [`verify_to_context_with_plan`].
///
/// `plan` is left `None` here — the synchronous path is kept for
/// callers that don't have a `Store` handy. Production callers should
/// use [`verify_to_context_with_plan`].
pub fn verify_to_context(
    validator: &JwtValidator,
    token: &str,
) -> Result<TenantContext, GatewayError> {
    let claims = validator.verify(token)?;
    Ok(TenantContext {
        tenant_id: claims.tenant_id,
        user_id: claims.user_id,
        workspace_id: claims.workspace_id,
        thread_id: ThreadId::new(),
        session_id: claims.session_id.unwrap_or_else(elena_types::SessionId::new),
        permissions: PermissionSet::default(),
        budget: default_budget_for_tier(claims.tier),
        tier: claims.tier,
        plan: None,
        metadata: HashMap::new(),
    })
}

/// Verify + resolve the per-(tenant, user, workspace) [`elena_types::ResolvedPlan`]
/// from the plan-assignments store, then stamp it onto the [`TenantContext`].
///
/// On store error the plan is left `None` (graceful degradation —
/// downstream code falls back to the legacy `tier` + `budget` during
/// the B1 transition). The failure is logged at WARN so operators
/// notice if every request is degrading.
pub async fn verify_to_context_with_plan(
    validator: &JwtValidator,
    token: &str,
    store: &Store,
) -> Result<TenantContext, GatewayError> {
    let mut ctx = verify_to_context(validator, token)?;
    match store
        .plan_assignments
        .resolve(ctx.tenant_id, Some(ctx.user_id), Some(ctx.workspace_id))
        .await
    {
        Ok(plan) => ctx.plan = Some(plan),
        Err(err) => {
            tracing::warn!(
                tenant_id = %ctx.tenant_id,
                user_id = %ctx.user_id,
                workspace_id = %ctx.workspace_id,
                error = ?err,
                "plan resolution failed; falling back to tier-derived defaults"
            );
        }
    }
    Ok(ctx)
}

/// Extract `TenantContext` from an incoming request's Authorization header.
///
/// Two acceptable token sources:
/// 1. `Authorization: Bearer <jwt>` — the standard HTTP path.
/// 2. `Sec-WebSocket-Protocol: elena.bearer.<jwt>` — browser-friendly
///    WS upgrade path (browsers can't set arbitrary headers on
///    `new WebSocket(...)`).
///
/// The legacy `?token=<jwt>` query-string fallback was removed in S7
/// to drop the hand-rolled urldecode helper.
///
/// When the surrounding state exposes both [`JwtValidator`] and
/// `Arc<Store>` (the gateway always does), the extractor resolves the
/// caller's [`elena_types::ResolvedPlan`] from `plan_assignments` and
/// stamps it onto the returned context. Store errors degrade gracefully
/// to `plan = None` so a transient DB blip doesn't 401 every client.
#[derive(Debug, Clone)]
pub struct AuthedTenant(pub TenantContext);

impl<S> FromRequestParts<S> for AuthedTenant
where
    S: Send + Sync,
    JwtValidator: FromRef<S>,
    Arc<Store>: FromRef<S>,
{
    type Rejection = GatewayError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let validator = JwtValidator::from_ref(state);
        let store = <Arc<Store>>::from_ref(state);
        let token =
            extract_bearer(parts).ok_or(GatewayError::Unauthorized { reason: "missing token" })?;
        let ctx = verify_to_context_with_plan(&validator, &token, &store).await?;
        Ok(Self(ctx))
    }
}

/// `Sec-WebSocket-Protocol` value prefix that wraps a bearer JWT for
/// browser WS upgrades. Clients send
/// `Sec-WebSocket-Protocol: elena.bearer.<jwt>`; the gateway extracts
/// the JWT and verifies as if it had arrived via `Authorization`.
const WS_BEARER_PROTOCOL_PREFIX: &str = "elena.bearer.";

fn extract_bearer(parts: &Parts) -> Option<String> {
    // 1. Authorization: Bearer <jwt> — standard HTTP path.
    if let Some(value) = parts.headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            if let Some(rest) = s.strip_prefix("Bearer ") {
                let trimmed = rest.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_owned());
                }
            }
        }
    }
    // 2. Sec-WebSocket-Protocol: elena.bearer.<jwt> — browser WS path.
    //    Header is comma-separated; pick the first sub-protocol that
    //    matches our prefix.
    if let Some(value) = parts.headers.get(axum::http::header::SEC_WEBSOCKET_PROTOCOL) {
        if let Ok(s) = value.to_str() {
            for proto in s.split(',') {
                let proto = proto.trim();
                if let Some(jwt) = proto.strip_prefix(WS_BEARER_PROTOCOL_PREFIX) {
                    if !jwt.is_empty() {
                        return Some(jwt.to_owned());
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! Q3 — Pure-JWT tests (valid token, expiry, audience, signature,
    //! custom issuer, C5.5 stateless contract) live in
    //! `elena_auth::jwt::tests`. The tests below cover only the
    //! gateway-specific axum integration: the
    //! `JwtError → GatewayError` mapping, `verify_to_context`'s
    //! `plan = None` invariant, and `extract_bearer`'s header parsing.
    use elena_auth::JwtConfig;
    use elena_types::{TenantId, UserId, WorkspaceId};
    use jsonwebtoken::{EncodingKey, Header, encode};

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
    fn jwt_error_maps_to_gateway_error() {
        // Verification failures become 401 Unauthorized with the same reason tag.
        let unauth: GatewayError = JwtError::Verification { reason: "token expired" }.into();
        match unauth {
            GatewayError::Unauthorized { reason } => assert_eq!(reason, "token expired"),
            other => panic!("expected Unauthorized, got {other:?}"),
        }
        // Setup failures become 500 Internal — bad PEM at boot is an
        // operator misconfiguration, not a client problem.
        let setup: GatewayError = JwtError::Setup("bad PEM".into()).into();
        assert!(matches!(setup, GatewayError::Internal(_)), "got {setup:?}");
    }

    #[test]
    fn verify_to_context_populates_fields() {
        let v = validator();
        let c = claims(3_600);
        let token = sign(&c, "elena-dev-secret-not-for-production");
        let ctx = verify_to_context(&v, &token).unwrap();
        assert_eq!(ctx.tenant_id, c.tenant_id);
        assert_eq!(ctx.user_id, c.user_id);
        assert_eq!(ctx.workspace_id, c.workspace_id);
        assert_eq!(ctx.tier, TenantTier::Pro);
        // The synchronous path doesn't have a Store handle, so plan
        // resolution is the caller's responsibility — verify it's left
        // None so the contract with downstream readers stays clear.
        assert!(ctx.plan.is_none(), "verify_to_context must leave plan = None");
    }

    fn parts_with_headers(headers: &[(&'static str, &str)]) -> axum::http::request::Parts {
        let mut req = axum::http::Request::builder().uri("/whatever");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.body(()).unwrap().into_parts().0
    }

    #[test]
    fn extract_bearer_from_authorization_header() {
        let parts = parts_with_headers(&[("authorization", "Bearer abc.def.ghi")]);
        assert_eq!(extract_bearer(&parts), Some("abc.def.ghi".to_owned()));
    }

    #[test]
    fn extract_bearer_from_ws_subprotocol() {
        let parts = parts_with_headers(&[("sec-websocket-protocol", "elena.bearer.abc.def.ghi")]);
        assert_eq!(extract_bearer(&parts), Some("abc.def.ghi".to_owned()));
    }

    #[test]
    fn extract_bearer_from_ws_subprotocol_with_other_protocols_present() {
        let parts = parts_with_headers(&[(
            "sec-websocket-protocol",
            "json, elena.bearer.abc.def.ghi, other",
        )]);
        assert_eq!(extract_bearer(&parts), Some("abc.def.ghi".to_owned()));
    }

    #[test]
    fn extract_bearer_rejects_empty_authorization() {
        let parts = parts_with_headers(&[("authorization", "Bearer ")]);
        assert_eq!(extract_bearer(&parts), None);
    }

    #[test]
    fn extract_bearer_query_string_no_longer_accepted() {
        // S7 — `?token=` is gone. A request with a token only in the query
        // string must be rejected so an attacker can't get around the
        // header-only contract by smuggling a JWT in URL params.
        let parts = axum::http::Request::builder()
            .uri("/v1/threads/abc/stream?token=abc.def.ghi")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        assert_eq!(extract_bearer(&parts), None);
    }
}
