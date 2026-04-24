//! Admin-API auth middleware.
//!
//! `/admin/v1/*` is exposed at the same gateway port as the public
//! WebSocket and client routes — there's no separate listener — so we
//! must gate the admin routes ourselves before they hit a handler.
//!
//! Two layers:
//! 1. [`require_admin_token`] — middleware that 401s any admin
//!    request whose `X-Elena-Admin-Token` header doesn't
//!    constant-time-match the operator's shared secret.
//! 2. [`require_tenant_scope`] — per-route helper called by handlers
//!    that take `Path<TenantId>`. When the tenant has provisioned a
//!    per-tenant scope hash, the handler additionally requires
//!    `X-Elena-Tenant-Scope` to match.
//!
//! Test deployments leave the global token unset (the middleware is
//! mounted with `state = None`), which short-circuits the check —
//! that lets `cargo test` and the smoke binaries exercise the admin
//! surface without setting a header.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use elena_store::Store;
use elena_types::{StoreError, TenantId};
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;

use crate::scope::hash_scope_token;

/// HTTP header carrying the shared admin token.
pub const ADMIN_TOKEN_HEADER: &str = "x-elena-admin-token";

/// HTTP header carrying the per-tenant admin scope token.
///
/// When a tenant has provisioned a scope hash via
/// `PUT /admin/v1/tenants/{id}/admin-scope`, every per-tenant route
/// (anything taking `Path<TenantId>`) requires this header in addition
/// to `X-Elena-Admin-Token`. NULL stored hash falls back to "global
/// token suffices" for legacy single-operator deployments.
pub const TENANT_SCOPE_HEADER: &str = "x-elena-tenant-scope";

/// Axum middleware that 401s any admin request whose
/// `X-Elena-Admin-Token` header doesn't constant-time-match the
/// configured token. When `state` is `None` the middleware is a
/// no-op — used in tests and smoke binaries.
pub async fn require_admin_token(
    State(expected): State<Option<SecretString>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = expected else {
        return Ok(next.run(req).await);
    };
    let supplied =
        req.headers().get(ADMIN_TOKEN_HEADER).and_then(|h| h.to_str().ok()).unwrap_or("");
    if bool::from(supplied.as_bytes().ct_eq(expected.expose_secret().as_bytes())) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Verify the caller is authorised to act on `tenant_id`.
///
/// Two-layer model:
/// 1. The global `require_admin_token` middleware has already gated
///    the request — only callers holding the operator's shared secret
///    reach a handler at all.
/// 2. If the tenant has provisioned a per-tenant scope hash (via
///    `PUT /admin/v1/tenants/{id}/admin-scope`), this helper additionally
///    requires `X-Elena-Tenant-Scope` to hash to the stored bytes.
///    Tenants without a scope (NULL `admin_scope_hash`) inherit the
///    global token for back-compat with single-operator deployments.
///
/// Errors map to HTTP status codes:
/// - `StatusCode::UNAUTHORIZED` — scope required but header missing or
///   wrong.
/// - `StatusCode::NOT_FOUND` — tenant doesn't exist (mirrors the
///   no-information-leak posture of `get_workspace`).
/// - `StatusCode::INTERNAL_SERVER_ERROR` — DB outage; the helper
///   refuses to fall through (fail closed).
pub async fn require_tenant_scope(
    store: &Store,
    tenant_id: TenantId,
    headers: &HeaderMap,
) -> Result<(), StatusCode> {
    let stored = match store.tenants.get_admin_scope_hash(tenant_id).await {
        Ok(h) => h,
        Err(StoreError::TenantNotFound(_)) => return Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!(?e, %tenant_id, "tenant scope lookup failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let Some(stored) = stored else {
        // No per-tenant scope provisioned — inherit the global token.
        return Ok(());
    };
    let supplied = headers.get(TENANT_SCOPE_HEADER).and_then(|h| h.to_str().ok()).unwrap_or("");
    if supplied.is_empty() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let supplied_hash = hash_scope_token(supplied);
    if bool::from(supplied_hash.ct_eq(&stored)) { Ok(()) } else { Err(StatusCode::UNAUTHORIZED) }
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Body, http::Request, middleware, routing::get};
    use http::HeaderValue;
    use tower::ServiceExt as _;

    use super::*;

    fn router(token: Option<&str>) -> Router {
        let state: Option<SecretString> = token.map(|t| SecretString::from(t.to_owned()));
        Router::new()
            .route("/protected", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(state, require_admin_token))
    }

    async fn call(app: Router, header: Option<&str>) -> StatusCode {
        let mut req = Request::builder().uri("/protected").body(Body::empty()).unwrap();
        if let Some(value) = header {
            req.headers_mut().insert(ADMIN_TOKEN_HEADER, HeaderValue::from_str(value).unwrap());
        }
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn no_token_configured_means_no_auth_required() {
        let app = router(None);
        assert_eq!(call(app, None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_header_when_token_required_returns_401() {
        let app = router(Some("the-secret"));
        assert_eq!(call(app, None).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_header_value_returns_401() {
        let app = router(Some("the-secret"));
        assert_eq!(call(app, Some("not-the-secret")).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn matching_header_passes_through() {
        let app = router(Some("the-secret"));
        assert_eq!(call(app, Some("the-secret")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn header_check_is_byte_exact_not_prefix() {
        // Defensive: never match a prefix; a leaked first byte must
        // not let an attacker progressively guess the token.
        let app = router(Some("the-secret"));
        assert_eq!(call(app, Some("the-secret-and-more")).await, StatusCode::UNAUTHORIZED);
    }
}
