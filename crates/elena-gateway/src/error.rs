//! Error types the gateway surfaces to clients + internal callers.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// Errors raised by the gateway layer.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// JWT failed verification (missing, malformed, expired, bad signature,
    /// wrong issuer/audience).
    #[error("unauthorized: {reason}")]
    Unauthorized {
        /// Short, non-revealing reason suitable to log.
        reason: &'static str,
    },

    /// The caller's request was well-formed but rejected on policy.
    #[error("forbidden: {reason}")]
    Forbidden {
        /// Why.
        reason: String,
    },

    /// Request body failed to parse.
    #[error("bad request: {message}")]
    BadRequest {
        /// Human-readable description.
        message: String,
    },

    /// The store (Postgres / Redis) returned an error.
    #[error(transparent)]
    Store(#[from] elena_types::StoreError),

    /// NATS publish / subscribe failed.
    #[error("nats error: {0}")]
    Nats(String),

    /// Catch-all for unexpected internal failures.
    #[error("internal: {0}")]
    Internal(String),
}

impl GatewayError {
    #[must_use]
    pub(crate) const fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            Self::Forbidden { .. } => StatusCode::FORBIDDEN,
            Self::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Self::Store(_) | Self::Nats(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = match &self {
            // Never leak store / nats / internal details to clients.
            Self::Store(_) | Self::Nats(_) | Self::Internal(_) => "internal error".to_owned(),
            other => other.to_string(),
        };
        (status, body).into_response()
    }
}
