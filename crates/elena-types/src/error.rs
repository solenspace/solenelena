//! Error taxonomy.
//!
//! [`ElenaError`] is the top-level failure type. It composes four sub-
//! hierarchies ([`LlmApiError`], [`ToolError`], [`StoreError`],
//! [`ConfigError`]) plus a handful of cross-cutting variants (context
//! overflow, budget, permission denied, aborted).
//!
//! Every error type implements `Serialize + Deserialize` because errors
//! travel over NATS / gRPC / WebSocket in Elena's distributed layout. Non-
//! serializable library errors (from `sqlx`, `fred`) are converted to
//! `String` at the boundary in `elena-store`.
//!
//! The [`LlmApiError`] variants are a 1:1 translation of
//! `services/api/errors.ts::classifyAPIError` in the reference TypeScript
//! source. Keeping them distinct lets the agentic loop make different
//! recovery decisions per category without string-matching messages.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::TenantId;

/// Top-level error for Elena.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum ElenaError {
    /// An error from the underlying LLM provider.
    #[error(transparent)]
    LlmApi(#[from] LlmApiError),

    /// A tool execution failure.
    #[error(transparent)]
    Tool(#[from] ToolError),

    /// A persistence-layer failure.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A configuration failure.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The prompt size exceeded the configured context window.
    #[error("context overflow: {tokens} tokens > limit {limit}")]
    ContextOverflow {
        /// How many tokens were in the prompt.
        tokens: u64,
        /// The active context window.
        limit: u64,
    },

    /// The tenant's budget was exhausted.
    #[error("budget exceeded for tenant {tenant_id}")]
    BudgetExceeded {
        /// Which tenant hit its cap.
        tenant_id: TenantId,
    },

    /// A permission check rejected the request.
    #[error("permission denied: {reason}")]
    PermissionDenied {
        /// Human-readable reason.
        reason: String,
    },

    /// The operation was aborted (user cancel, shutdown, etc.).
    #[error("operation aborted")]
    Aborted,
}

/// LLM API errors — a 1:1 translation of `classifyAPIError`.
///
/// Every variant preserves the minimum data needed to make a recovery
/// decision. Use [`LlmApiError::kind`] for metrics and
/// [`LlmApiError::is_retryable`] for retry logic.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum LlmApiError {
    /// Request timed out.
    #[error("API timeout after {elapsed_ms}ms")]
    ApiTimeout {
        /// How long the request ran before we gave up.
        elapsed_ms: u64,
    },

    /// Connection-level error (DNS, TCP reset, TLS failure other than cert).
    #[error("connection error: {message}")]
    ConnectionError {
        /// Underlying cause.
        message: String,
    },

    /// TLS certificate failure.
    #[error("SSL cert error: {message}")]
    SslCertError {
        /// Underlying cause.
        message: String,
    },

    /// Rate limited by the provider (HTTP 429).
    #[error("rate limited; retry after {retry_after_ms:?}ms")]
    RateLimit {
        /// Provider-supplied `Retry-After` header, if any.
        retry_after_ms: Option<u64>,
    },

    /// Provider is overloaded (HTTP 529).
    #[error("server overloaded (529)")]
    ServerOverload,

    /// Multiple consecutive 529s — the loop has given up retrying.
    #[error("repeated 529 overloaded errors")]
    Repeated529,

    /// Prompt exceeded model context window.
    #[error("prompt too long: {actual_tokens:?} > {limit_tokens:?}")]
    PromptTooLong {
        /// How many tokens the prompt had (if reported).
        actual_tokens: Option<u64>,
        /// The model's limit (if reported).
        limit_tokens: Option<u64>,
    },

    /// Image payload was rejected for being too large.
    #[error("image too large")]
    ImageTooLarge,
    /// PDF payload was rejected for being too large.
    #[error("PDF too large")]
    PdfTooLarge,
    /// PDF was password-protected and couldn't be processed.
    #[error("PDF password protected")]
    PdfPasswordProtected,
    /// Image dimensions exceeded provider limits.
    #[error("image dimensions exceeded")]
    ImageDimension,
    /// Overall request body exceeded provider limits.
    #[error("request too large")]
    RequestTooLarge,
    /// The `tool_use` and `tool_result` blocks in the conversation mismatch.
    #[error("tool_use/tool_result mismatch")]
    ToolUseMismatch,
    /// A generic invalid-request error (HTTP 400 with a reason body).
    #[error("invalid request: {message}")]
    InvalidRequest {
        /// Provider-supplied reason.
        message: String,
    },

    /// API key is invalid.
    #[error("invalid API key")]
    InvalidApiKey,
    /// OAuth token has been revoked.
    #[error("OAuth token revoked")]
    TokenRevoked,
    /// OAuth org is not allowed to use the API.
    #[error("org not allowed for OAuth")]
    OauthOrgNotAllowed,
    /// Generic auth failure (not one of the above).
    #[error("authentication error")]
    AuthError,

    /// Tenant/account has insufficient credit.
    #[error("credit balance too low")]
    CreditBalanceLow,
    /// Capacity kill-switch is engaged for this account.
    #[error("capacity off switch engaged")]
    CapacityOffSwitch,

    /// Request was cancelled.
    #[error("aborted")]
    Aborted,
    /// Provider 5xx error (not otherwise classified).
    #[error("server error (status {status})")]
    ServerError {
        /// HTTP status code.
        status: u16,
    },
    /// Provider 4xx error (not otherwise classified).
    #[error("client error (status {status})")]
    ClientError {
        /// HTTP status code.
        status: u16,
    },
    /// Fallback for anything the classifier couldn't bucket.
    #[error("unknown API error: {message}")]
    Unknown {
        /// Captured message for debugging.
        message: String,
    },
}

impl LlmApiError {
    /// Return the classification tag for metrics / logging.
    #[must_use]
    pub const fn kind(&self) -> LlmApiErrorKind {
        match self {
            Self::ApiTimeout { .. } => LlmApiErrorKind::ApiTimeout,
            Self::ConnectionError { .. } => LlmApiErrorKind::ConnectionError,
            Self::SslCertError { .. } => LlmApiErrorKind::SslCertError,
            Self::RateLimit { .. } => LlmApiErrorKind::RateLimit,
            Self::ServerOverload => LlmApiErrorKind::ServerOverload,
            Self::Repeated529 => LlmApiErrorKind::Repeated529,
            Self::PromptTooLong { .. } => LlmApiErrorKind::PromptTooLong,
            Self::ImageTooLarge => LlmApiErrorKind::ImageTooLarge,
            Self::PdfTooLarge => LlmApiErrorKind::PdfTooLarge,
            Self::PdfPasswordProtected => LlmApiErrorKind::PdfPasswordProtected,
            Self::ImageDimension => LlmApiErrorKind::ImageDimension,
            Self::RequestTooLarge => LlmApiErrorKind::RequestTooLarge,
            Self::ToolUseMismatch => LlmApiErrorKind::ToolUseMismatch,
            Self::InvalidRequest { .. } => LlmApiErrorKind::InvalidRequest,
            Self::InvalidApiKey => LlmApiErrorKind::InvalidApiKey,
            Self::TokenRevoked => LlmApiErrorKind::TokenRevoked,
            Self::OauthOrgNotAllowed => LlmApiErrorKind::OauthOrgNotAllowed,
            Self::AuthError => LlmApiErrorKind::AuthError,
            Self::CreditBalanceLow => LlmApiErrorKind::CreditBalanceLow,
            Self::CapacityOffSwitch => LlmApiErrorKind::CapacityOffSwitch,
            Self::Aborted => LlmApiErrorKind::Aborted,
            Self::ServerError { .. } => LlmApiErrorKind::ServerError,
            Self::ClientError { .. } => LlmApiErrorKind::ClientError,
            Self::Unknown { .. } => LlmApiErrorKind::Unknown,
        }
    }

    /// True if a retry with the same payload has a reasonable chance of
    /// succeeding. Transient network/overload errors retry; auth/content
    /// errors do not.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ApiTimeout { .. }
                | Self::ConnectionError { .. }
                | Self::RateLimit { .. }
                | Self::ServerOverload
                | Self::ServerError { .. }
        )
    }

    /// True if the error indicates the prompt itself must be reshaped before
    /// another attempt (compaction, image removal, etc.).
    #[must_use]
    pub const fn requires_reshape(&self) -> bool {
        matches!(
            self,
            Self::PromptTooLong { .. }
                | Self::RequestTooLarge
                | Self::ImageTooLarge
                | Self::ImageDimension
                | Self::PdfTooLarge
                | Self::ToolUseMismatch
        )
    }
}

/// Compact string-tag classification. Hashable, `Copy`, metrics-friendly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmApiErrorKind {
    /// See [`LlmApiError::ApiTimeout`].
    ApiTimeout,
    /// See [`LlmApiError::ConnectionError`].
    ConnectionError,
    /// See [`LlmApiError::SslCertError`].
    SslCertError,
    /// See [`LlmApiError::RateLimit`].
    RateLimit,
    /// See [`LlmApiError::ServerOverload`].
    ServerOverload,
    /// See [`LlmApiError::Repeated529`].
    Repeated529,
    /// See [`LlmApiError::PromptTooLong`].
    PromptTooLong,
    /// See [`LlmApiError::ImageTooLarge`].
    ImageTooLarge,
    /// See [`LlmApiError::PdfTooLarge`].
    PdfTooLarge,
    /// See [`LlmApiError::PdfPasswordProtected`].
    PdfPasswordProtected,
    /// See [`LlmApiError::ImageDimension`].
    ImageDimension,
    /// See [`LlmApiError::RequestTooLarge`].
    RequestTooLarge,
    /// See [`LlmApiError::ToolUseMismatch`].
    ToolUseMismatch,
    /// See [`LlmApiError::InvalidRequest`].
    InvalidRequest,
    /// See [`LlmApiError::InvalidApiKey`].
    InvalidApiKey,
    /// See [`LlmApiError::TokenRevoked`].
    TokenRevoked,
    /// See [`LlmApiError::OauthOrgNotAllowed`].
    OauthOrgNotAllowed,
    /// See [`LlmApiError::AuthError`].
    AuthError,
    /// See [`LlmApiError::CreditBalanceLow`].
    CreditBalanceLow,
    /// See [`LlmApiError::CapacityOffSwitch`].
    CapacityOffSwitch,
    /// See [`LlmApiError::Aborted`].
    Aborted,
    /// See [`LlmApiError::ServerError`].
    ServerError,
    /// See [`LlmApiError::ClientError`].
    ClientError,
    /// See [`LlmApiError::Unknown`].
    Unknown,
}

/// Tool execution failures.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolError {
    /// The requested tool is not registered.
    #[error("tool not found: {name}")]
    NotFound {
        /// Requested tool name.
        name: String,
    },
    /// The tool rejected its input during validation.
    #[error("invalid input: {message}")]
    InvalidInput {
        /// Validation message.
        message: String,
    },
    /// The tool ran and failed.
    #[error("tool execution failed: {message}")]
    Execution {
        /// Failure details.
        message: String,
    },
    /// The tool exceeded its time budget.
    #[error("tool timed out after {timeout_ms}ms")]
    Timeout {
        /// Configured timeout.
        timeout_ms: u64,
    },
    /// The tool's result exceeded the size cap.
    #[error("tool result too large: {size_bytes} bytes exceeds cap {cap_bytes}")]
    ResultTooLarge {
        /// Actual size.
        size_bytes: u64,
        /// Configured maximum.
        cap_bytes: u64,
    },
}

/// Persistence-layer failures.
///
/// Underlying `sqlx`/`fred` errors are converted to [`String`] at the
/// boundary because those crates' error types are neither `Serialize` nor
/// `Clone`. The caller can match on variant category; the string preserves
/// operator debug info.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum StoreError {
    /// A thread with the given ID was not found.
    #[error("thread not found: {0}")]
    ThreadNotFound(crate::id::ThreadId),

    /// A message with the given ID was not found.
    #[error("message not found: {0}")]
    MessageNotFound(crate::id::MessageId),

    /// A tenant with the given ID was not found.
    #[error("tenant not found: {0}")]
    TenantNotFound(TenantId),

    /// A row exists but belongs to a different tenant than requested.
    #[error("tenant mismatch: row belongs to {owner}, not {requested}")]
    TenantMismatch {
        /// Tenant that actually owns the row.
        owner: TenantId,
        /// Tenant that issued the query.
        requested: TenantId,
    },

    /// A unique constraint, foreign key, or CAS check failed.
    #[error("conflict: {0}")]
    Conflict(String),

    /// Database driver error.
    #[error("database error: {0}")]
    Database(String),

    /// Redis driver error.
    #[error("cache error: {0}")]
    Cache(String),

    /// Serialization/deserialization failed at the store boundary.
    #[error("serialization: {0}")]
    Serialization(String),

    /// A required runtime configuration value is missing or malformed —
    /// e.g. a master key env var is absent, the wrong length, or not
    /// valid base64.
    #[error("configuration: {0}")]
    Configuration(String),
}

/// Configuration errors.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum ConfigError {
    /// A required config key is missing.
    #[error("missing required config: {key}")]
    Missing {
        /// The missing key path.
        key: String,
    },
    /// A config value failed validation.
    #[error("invalid config value for {key}: {message}")]
    Invalid {
        /// The offending key path.
        key: String,
        /// Validation message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_error_wraps_children() {
        let err: ElenaError = LlmApiError::RateLimit { retry_after_ms: Some(500) }.into();
        assert!(matches!(err, ElenaError::LlmApi(_)));
    }

    #[test]
    fn llm_api_error_kinds_are_snake_case() {
        assert_eq!(
            serde_json::to_value(LlmApiErrorKind::RateLimit).unwrap(),
            serde_json::json!("rate_limit")
        );
        assert_eq!(
            serde_json::to_value(LlmApiErrorKind::PromptTooLong).unwrap(),
            serde_json::json!("prompt_too_long")
        );
    }

    #[test]
    fn retryable_classification() {
        assert!(LlmApiError::RateLimit { retry_after_ms: None }.is_retryable());
        assert!(LlmApiError::ServerOverload.is_retryable());
        assert!(LlmApiError::ServerError { status: 502 }.is_retryable());
        assert!(!LlmApiError::InvalidApiKey.is_retryable());
        assert!(
            !LlmApiError::PromptTooLong { actual_tokens: None, limit_tokens: None }.is_retryable()
        );
    }

    #[test]
    fn reshape_classification() {
        assert!(
            LlmApiError::PromptTooLong { actual_tokens: None, limit_tokens: None }
                .requires_reshape()
        );
        assert!(LlmApiError::ImageTooLarge.requires_reshape());
        assert!(!LlmApiError::RateLimit { retry_after_ms: None }.requires_reshape());
    }

    #[test]
    fn every_llm_variant_has_a_kind() {
        let variants = [
            LlmApiError::ApiTimeout { elapsed_ms: 1 },
            LlmApiError::ConnectionError { message: "x".into() },
            LlmApiError::SslCertError { message: "x".into() },
            LlmApiError::RateLimit { retry_after_ms: None },
            LlmApiError::ServerOverload,
            LlmApiError::Repeated529,
            LlmApiError::PromptTooLong { actual_tokens: None, limit_tokens: None },
            LlmApiError::ImageTooLarge,
            LlmApiError::PdfTooLarge,
            LlmApiError::PdfPasswordProtected,
            LlmApiError::ImageDimension,
            LlmApiError::RequestTooLarge,
            LlmApiError::ToolUseMismatch,
            LlmApiError::InvalidRequest { message: "x".into() },
            LlmApiError::InvalidApiKey,
            LlmApiError::TokenRevoked,
            LlmApiError::OauthOrgNotAllowed,
            LlmApiError::AuthError,
            LlmApiError::CreditBalanceLow,
            LlmApiError::CapacityOffSwitch,
            LlmApiError::Aborted,
            LlmApiError::ServerError { status: 500 },
            LlmApiError::ClientError { status: 400 },
            LlmApiError::Unknown { message: "x".into() },
        ];
        for v in variants {
            let _kind = v.kind(); // exhaustive — compiler catches missing arms.
            let json = serde_json::to_string(&v).unwrap();
            let back: LlmApiError = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back, "variant did not roundtrip");
        }
    }

    #[test]
    fn store_error_display() {
        let err = StoreError::TenantMismatch { owner: TenantId::new(), requested: TenantId::new() };
        assert!(err.to_string().contains("tenant mismatch"));
    }

    #[test]
    fn config_error_display() {
        let err = ConfigError::Missing { key: "postgres.url".into() };
        assert_eq!(err.to_string(), "missing required config: postgres.url");
    }
}
