//! Admin-API auth middleware.
//!
//! `/admin/v1/*` is exposed at the same gateway port as the public
//! WebSocket and client routes — there's no separate listener — so we
//! must gate the admin routes ourselves before they hit a handler.
//!
//! Mechanism: a single shared-secret header check. The operator
//! provisions `ELENA_ADMIN_TOKEN` (typically via Railway env vars);
//! their backend-for-frontend includes the same value as
//! `X-Elena-Admin-Token` on every admin call. The check uses a
//! constant-time comparison so a side-channel attacker can't probe
//! the token byte-by-byte via response timing.
//!
//! Test deployments and the smoke binaries leave the token unset
//! (`AdminState.admin_token == None`) which short-circuits the check —
//! that lets `cargo test` and the `elena-*-smoke` binaries keep
//! exercising the admin surface without setting a header.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use secrecy::{ExposeSecret, SecretString};
use subtle::ConstantTimeEq;

/// HTTP header carrying the shared admin token.
pub const ADMIN_TOKEN_HEADER: &str = "x-elena-admin-token";

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
    let supplied = req
        .headers()
        .get(ADMIN_TOKEN_HEADER)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if bool::from(supplied.as_bytes().ct_eq(expected.expose_secret().as_bytes())) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, middleware, routing::get};
    use http::HeaderValue;
    use tower::ServiceExt as _;

    fn router(token: Option<&str>) -> Router {
        let state: Option<SecretString> = token.map(|t| SecretString::from(t.to_owned()));
        Router::new()
            .route("/protected", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(state, require_admin_token))
    }

    async fn call(app: Router, header: Option<&str>) -> StatusCode {
        let mut req = Request::builder().uri("/protected").body(Body::empty()).unwrap();
        if let Some(value) = header {
            req.headers_mut()
                .insert(ADMIN_TOKEN_HEADER, HeaderValue::from_str(value).unwrap());
        }
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn no_token_configured_means_no_auth_required() {
        // Smoke binaries + tests must keep working with no header set.
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
