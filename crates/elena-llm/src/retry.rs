//! Retry policy, HTTP-status classification, and exponential backoff with
//! jitter.
//!
//! Behavior mirrors `services/api/withRetry.ts` in the reference TS source,
//! simplified for Phase 2:
//!
//! - Base delay 500ms, max 32s, 10 attempts, 25% jitter.
//! - 408 / 409 / 5xx / 529 / connection errors retry.
//! - 529 retries are capped at 3 (then surfaced as
//!   [`LlmApiError::Repeated529`]) — Phase 2 has no fallback model.
//! - 429 always retries (subscriber-aware gating lands when Elena's billing
//!   state does).
//! - `Retry-After` header parsed as integer seconds; overrides the
//!   exponential delay for that attempt.
//! - 400 + `maximum_tokens` error message is NOT retried by the LLM layer;
//!   callers (the agentic loop) handle max-output-tokens escalation.
//! - Content errors (`prompt_too_long`, `image_*`, `pdf_*`, etc.) fail
//!   without retry — the agentic loop reshapes and re-sends.

use std::time::Duration;

use elena_types::{LlmApiError, LlmApiErrorKind};
use http::{HeaderMap, StatusCode, header};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Policy controlling exponential backoff + retry limits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Initial delay in milliseconds. Default 500.
    pub base_delay_ms: u64,
    /// Upper bound on the computed delay. Default `32_000` (32s).
    pub max_delay_ms: u64,
    /// Maximum number of attempts (including the first). Default 10.
    pub max_attempts: u32,
    /// Jitter ratio — fraction of the base delay added as uniform noise.
    /// Default 0.25 (= up to +25%).
    pub jitter_ratio: f32,
}

impl RetryPolicy {
    /// Default Elena retry policy.
    ///
    /// Matches Claude Code's defaults as closely as the reference warrants:
    /// 500ms base, 32s cap, 10 attempts, 25% jitter.
    pub const DEFAULT: Self =
        Self { base_delay_ms: 500, max_delay_ms: 32_000, max_attempts: 10, jitter_ratio: 0.25 };

    /// Compute the backoff delay for the given attempt number (1-indexed).
    ///
    /// Formula: `base * 2^(attempt-1) + uniform(0, jitter_ratio · base · 2^(attempt-1))`,
    /// clamped to `max_delay_ms`.
    ///
    /// `attempt == 0` returns zero (first try has no delay).
    #[must_use]
    // `compute_delay` does an intentional f64/u64 conversion for jitter
    // sizing. Precision loss on the scale of milliseconds at this magnitude
    // is irrelevant; `jitter_ratio` is a small, non-negative fraction.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    pub fn compute_delay(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::ZERO;
        }
        // Use saturating shifts so large attempts don't panic via overflow.
        let exp = u32::min(attempt.saturating_sub(1), 62);
        let base = self.base_delay_ms.saturating_mul(1u64 << exp);
        let base = base.min(self.max_delay_ms);
        let jitter_max = ((base as f64) * f64::from(self.jitter_ratio)).max(0.0) as u64;
        let jitter = if jitter_max == 0 { 0 } else { rand::thread_rng().gen_range(0..=jitter_max) };
        Duration::from_millis(base.saturating_add(jitter).min(self.max_delay_ms))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The outcome of a retry decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryDecision {
    /// Retry after the given delay.
    Retry {
        /// How long to wait before the next attempt.
        delay: Duration,
    },
    /// Give up; surface the error.
    Fail(LlmApiError),
}

/// Classify an HTTP response from the Anthropic API into an [`LlmApiError`].
///
/// `body_snippet` is an optional excerpt of the response body (up to the
/// first few KB) used to disambiguate error categories — the same status
/// code can carry different semantic errors depending on the body.
#[must_use]
pub fn classify_http(
    status: StatusCode,
    headers: &HeaderMap,
    body_snippet: Option<&str>,
) -> LlmApiError {
    let retry_after_ms = parse_retry_after_ms(headers);

    match status.as_u16() {
        429 => LlmApiError::RateLimit { retry_after_ms },
        529 => LlmApiError::ServerOverload,
        401 => LlmApiError::InvalidApiKey,
        403 => body_snippet
            .filter(|b| b.contains("OAuth"))
            .map_or(LlmApiError::AuthError, |_| LlmApiError::TokenRevoked),
        400 => classify_400(body_snippet),
        408 => LlmApiError::ApiTimeout { elapsed_ms: 0 },
        413 => LlmApiError::RequestTooLarge,
        s if (500u16..600u16).contains(&s) => LlmApiError::ServerError { status: s },
        s if (400u16..500u16).contains(&s) => LlmApiError::ClientError { status: s },
        s => LlmApiError::Unknown { message: format!("unexpected status: {s}") },
    }
}

fn classify_400(body: Option<&str>) -> LlmApiError {
    let Some(body) = body else {
        return LlmApiError::InvalidRequest { message: "400 without body".into() };
    };
    // Walk likely markers in priority order. Reference TS source uses string
    // substring checks, which is the only stable contract the API offers.
    if body.contains("prompt is too long")
        || body.contains("prompt exceeds")
        || body.contains("context_length_exceeded")
    {
        LlmApiError::PromptTooLong { actual_tokens: None, limit_tokens: None }
    } else if body.contains("tool_use") && body.contains("mismatch") {
        LlmApiError::ToolUseMismatch
    } else if body.contains("image too large") {
        LlmApiError::ImageTooLarge
    } else if body.contains("image dimensions") {
        LlmApiError::ImageDimension
    } else if body.contains("PDF") && body.contains("password") {
        LlmApiError::PdfPasswordProtected
    } else if body.contains("PDF") && body.contains("too large") {
        LlmApiError::PdfTooLarge
    } else if body.contains("credit balance") {
        LlmApiError::CreditBalanceLow
    } else {
        LlmApiError::InvalidRequest { message: truncate(body, 512) }
    }
}

fn parse_retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    // Integer seconds form. HTTP-date form (`"Mon, 29 Apr..."`) is technically
    // valid but Anthropic never sends it — defer parsing that until we see
    // it in the wild.
    raw.trim().parse::<u64>().ok().map(|s| s.saturating_mul(1000))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        // Respect UTF-8 boundaries.
        let mut idx = max;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        let mut out = s[..idx].to_owned();
        out.push('…');
        out
    }
}

/// Decide whether to retry after a classified error.
///
/// - `err`: the classified error from [`classify_http`] or a connection-level
///   failure the caller surfaced.
/// - `attempt`: 1-indexed attempt number that JUST failed (so the *next*
///   attempt number is `attempt + 1`).
/// - `consecutive_529s`: running count of 529 responses on this request,
///   to implement the Repeated529 cap.
/// - `policy`: retry settings.
///
/// Returns [`RetryDecision::Retry`] with an appropriate delay (honoring
/// `retry-after` when present), or [`RetryDecision::Fail`] if the error
/// is terminal or the retry budget is exhausted.
#[must_use]
pub fn decide_retry(
    err: &LlmApiError,
    attempt: u32,
    consecutive_529s: u32,
    policy: &RetryPolicy,
) -> RetryDecision {
    // Budget check first — cheap and short-circuits.
    if attempt >= policy.max_attempts {
        return RetryDecision::Fail(err.clone());
    }

    match err {
        LlmApiError::RateLimit { retry_after_ms } => RetryDecision::Retry {
            delay: retry_after_ms
                .map_or_else(|| policy.compute_delay(attempt + 1), Duration::from_millis),
        },
        LlmApiError::ServerOverload => {
            // Reference uses MAX_529_RETRIES = 3 — three retries after the
            // initial 529, i.e. up to four total attempts. On the 4th
            // consecutive 529 we give up.
            if consecutive_529s >= 4 {
                RetryDecision::Fail(LlmApiError::Repeated529)
            } else {
                RetryDecision::Retry { delay: policy.compute_delay(attempt + 1) }
            }
        }
        LlmApiError::ApiTimeout { .. }
        | LlmApiError::ConnectionError { .. }
        | LlmApiError::ServerError { .. } => RetryDecision::Retry {
            delay: policy.compute_delay(attempt + 1),
        },
        // Content-size / tool-mismatch errors don't retry — the caller
        // (agentic loop) will reshape the prompt and try again at its level.
        LlmApiError::PromptTooLong { .. }
        | LlmApiError::RequestTooLarge
        | LlmApiError::ImageTooLarge
        | LlmApiError::ImageDimension
        | LlmApiError::PdfTooLarge
        | LlmApiError::PdfPasswordProtected
        | LlmApiError::ToolUseMismatch
        | LlmApiError::InvalidRequest { .. }
        // Auth / billing / explicit aborts never retry silently.
        | LlmApiError::InvalidApiKey
        | LlmApiError::TokenRevoked
        | LlmApiError::OauthOrgNotAllowed
        | LlmApiError::AuthError
        | LlmApiError::CreditBalanceLow
        | LlmApiError::CapacityOffSwitch
        | LlmApiError::Aborted
        | LlmApiError::Repeated529
        | LlmApiError::SslCertError { .. }
        | LlmApiError::ClientError { .. }
        | LlmApiError::Unknown { .. } => RetryDecision::Fail(err.clone()),
    }
}

/// Stable tag for the error, useful for `tracing` fields and metrics.
#[must_use]
pub fn error_kind_tag(err: &LlmApiError) -> LlmApiErrorKind {
    err.kind()
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue, StatusCode};

    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy::DEFAULT
    }

    #[test]
    fn compute_delay_is_monotonic_until_cap() {
        let p = policy();
        let d1 = p.compute_delay(1).as_millis();
        let d2 = p.compute_delay(2).as_millis();
        let d3 = p.compute_delay(3).as_millis();
        assert!((500..=625).contains(&d1), "attempt 1 in [500, 625]: {d1}");
        assert!((1000..=1250).contains(&d2), "attempt 2 in [1000, 1250]: {d2}");
        assert!((2000..=2500).contains(&d3), "attempt 3 in [2000, 2500]: {d3}");
        assert!(d1 < d2 && d2 < d3);
    }

    #[test]
    fn compute_delay_clamps_at_max() {
        let p = policy();
        // At attempt 20, base·2^19 massively exceeds 32s cap.
        let d = u64::try_from(p.compute_delay(20).as_millis()).unwrap();
        assert!(d >= p.max_delay_ms && d <= p.max_delay_ms + 8_000);
    }

    #[test]
    fn compute_delay_zero_attempt_is_zero() {
        assert_eq!(policy().compute_delay(0), Duration::ZERO);
    }

    #[test]
    fn classify_429_captures_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
        let err = classify_http(StatusCode::TOO_MANY_REQUESTS, &headers, None);
        assert_eq!(err, LlmApiError::RateLimit { retry_after_ms: Some(7_000) });
    }

    #[test]
    fn classify_529_has_no_retry_after() {
        let err = classify_http(StatusCode::from_u16(529).unwrap(), &HeaderMap::new(), None);
        assert_eq!(err, LlmApiError::ServerOverload);
    }

    #[test]
    fn classify_500_is_server_error() {
        let err = classify_http(StatusCode::INTERNAL_SERVER_ERROR, &HeaderMap::new(), None);
        assert_eq!(err, LlmApiError::ServerError { status: 500 });
    }

    #[test]
    fn classify_401_is_invalid_api_key() {
        let err = classify_http(StatusCode::UNAUTHORIZED, &HeaderMap::new(), None);
        assert_eq!(err, LlmApiError::InvalidApiKey);
    }

    #[test]
    fn classify_403_oauth_revoked() {
        let err = classify_http(
            StatusCode::FORBIDDEN,
            &HeaderMap::new(),
            Some("OAuth token has been revoked"),
        );
        assert_eq!(err, LlmApiError::TokenRevoked);
    }

    #[test]
    fn classify_403_generic_auth() {
        let err = classify_http(StatusCode::FORBIDDEN, &HeaderMap::new(), Some("Not allowed"));
        assert_eq!(err, LlmApiError::AuthError);
    }

    #[test]
    fn classify_400_prompt_too_long() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 120000 tokens > 100000"}}"#;
        let err = classify_http(StatusCode::BAD_REQUEST, &HeaderMap::new(), Some(body));
        assert_eq!(err, LlmApiError::PromptTooLong { actual_tokens: None, limit_tokens: None });
    }

    #[test]
    fn classify_400_credit_balance() {
        let body =
            r#"{"error":{"message":"Your credit balance is too low to complete this request"}}"#;
        let err = classify_http(StatusCode::BAD_REQUEST, &HeaderMap::new(), Some(body));
        assert_eq!(err, LlmApiError::CreditBalanceLow);
    }

    #[test]
    fn classify_400_invalid_request_fallback() {
        let body = r#"{"error":{"message":"temperature must be in [0, 1]"}}"#;
        let err = classify_http(StatusCode::BAD_REQUEST, &HeaderMap::new(), Some(body));
        assert!(matches!(err, LlmApiError::InvalidRequest { .. }));
    }

    #[test]
    fn classify_413_request_too_large() {
        let err = classify_http(StatusCode::PAYLOAD_TOO_LARGE, &HeaderMap::new(), None);
        assert_eq!(err, LlmApiError::RequestTooLarge);
    }

    #[test]
    fn retry_after_parses_integer_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
        assert_eq!(parse_retry_after_ms(&headers), Some(2_000));
    }

    #[test]
    fn retry_after_malformed_is_none() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("Mon, 29 Apr..."));
        // HTTP-date form — not supported in Phase 2.
        assert_eq!(parse_retry_after_ms(&headers), None);
    }

    #[test]
    fn decide_rate_limit_uses_retry_after() {
        let p = policy();
        let err = LlmApiError::RateLimit { retry_after_ms: Some(3_500) };
        match decide_retry(&err, 1, 0, &p) {
            RetryDecision::Retry { delay } => assert_eq!(delay, Duration::from_millis(3_500)),
            RetryDecision::Fail(_) => panic!("should retry"),
        }
    }

    #[test]
    fn decide_overload_retries_up_to_three_more() {
        let p = policy();
        let err = LlmApiError::ServerOverload;
        // 3 retries after initial = counts 1, 2, 3 all retry.
        for n in 1..=3 {
            assert!(
                matches!(decide_retry(&err, 1, n, &p), RetryDecision::Retry { .. }),
                "count {n} should retry",
            );
        }
        // 4 consecutive 529s → give up.
        assert_eq!(decide_retry(&err, 1, 4, &p), RetryDecision::Fail(LlmApiError::Repeated529),);
    }

    #[test]
    fn decide_content_errors_never_retry() {
        let p = policy();
        let errs = [
            LlmApiError::PromptTooLong { actual_tokens: None, limit_tokens: None },
            LlmApiError::RequestTooLarge,
            LlmApiError::ImageTooLarge,
            LlmApiError::ToolUseMismatch,
            LlmApiError::InvalidApiKey,
            LlmApiError::CreditBalanceLow,
        ];
        for err in errs {
            assert!(
                matches!(decide_retry(&err, 1, 0, &p), RetryDecision::Fail(_)),
                "{err:?} should not retry"
            );
        }
    }

    #[test]
    fn decide_budget_exhaustion_fails() {
        let p = RetryPolicy { max_attempts: 3, ..RetryPolicy::DEFAULT };
        let err = LlmApiError::ServerError { status: 502 };
        // attempt 3 (1-indexed) equals max_attempts → no further retry.
        assert!(matches!(decide_retry(&err, 3, 0, &p), RetryDecision::Fail(_)));
    }

    #[test]
    fn decide_connection_error_retries() {
        let p = policy();
        let err = LlmApiError::ConnectionError { message: "TCP reset".into() };
        assert!(matches!(decide_retry(&err, 1, 0, &p), RetryDecision::Retry { .. }));
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // 🎉 is a 4-byte UTF-8 sequence. Truncating at byte 3 mid-emoji should
        // back off to byte 0.
        let s = "🎉hello";
        let out = truncate(s, 3);
        // Must parse cleanly as a valid String; the emoji is either fully
        // included or fully excluded.
        assert!(out.starts_with("…") || out.is_empty() || out.starts_with('🎉'));
    }
}
