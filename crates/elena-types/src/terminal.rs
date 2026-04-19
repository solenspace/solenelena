//! Terminal conditions for the agentic loop.
//!
//! A [`Terminal`] is the reason a loop run stopped, either successfully
//! ([`Terminal::Completed`]) or with a classified failure. Every variant
//! seen in the reference TypeScript source (`query.ts`'s `return { reason: ... }`
//! exit points) is represented here.

use serde::{Deserialize, Serialize};

use crate::error::LlmApiErrorKind;

/// Why an agentic-loop turn ended.
///
/// Variants are grouped semantically in the source; consumers can pattern
/// match individually or use the helpers below.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Terminal {
    // ---- Success ----
    /// The turn completed normally — model emitted a final reply.
    Completed,

    // ---- Resource limits ----
    /// The loop hit the configured max-turn cap.
    MaxTurns {
        /// How many turns ran before the cap tripped.
        turn_count: u32,
    },
    /// A tenant-level blocking limit (concurrent threads, tool calls) was hit.
    BlockingLimit,
    /// The token budget signaled a continuation boundary.
    TokenBudgetContinuation,

    // ---- Content failures ----
    /// The prompt exceeded the model context window and no recovery succeeded.
    PromptTooLong,
    /// An image block failed validation and could not be recovered.
    ImageError,

    // ---- Abort signals ----
    /// The streaming phase was aborted mid-flight.
    AbortedStreaming,
    /// Tool execution was aborted mid-flight.
    AbortedTools,

    // ---- Hook gating ----
    /// A stop hook prevented the loop from continuing.
    StopHookPrevented,
    /// A stop hook matched and ended the loop.
    HookStopped,

    // ---- Transient / recoverable — surfaced when the loop gave up retrying ----
    /// A classified LLM API error ended the loop after retry budget was spent.
    ModelError {
        /// The error classification.
        classified: LlmApiErrorKind,
    },
    /// A reactive-compact retry terminated the turn (expected to resume next turn).
    ReactiveCompactRetry,
    /// A context-collapse drain retry terminated the turn.
    CollapseDrainRetry,
    /// The loop escalated to a higher `max_output_tokens` cap.
    MaxOutputTokensEscalate,
    /// The loop ran max-output-tokens recovery to completion.
    MaxOutputTokensRecovery,
}

impl Terminal {
    /// True if this terminal condition represents a clean completion of the turn.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// True if this terminal condition is a retryable state for the next turn.
    ///
    /// Useful for deciding whether to re-enqueue the thread or surface the
    /// failure to the client.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::ReactiveCompactRetry
                | Self::CollapseDrainRetry
                | Self::MaxOutputTokensEscalate
                | Self::MaxOutputTokensRecovery
                | Self::TokenBudgetContinuation
        )
    }

    /// A short stable tag suitable for metrics/labels.
    #[must_use]
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::MaxTurns { .. } => "max_turns",
            Self::BlockingLimit => "blocking_limit",
            Self::TokenBudgetContinuation => "token_budget_continuation",
            Self::PromptTooLong => "prompt_too_long",
            Self::ImageError => "image_error",
            Self::AbortedStreaming => "aborted_streaming",
            Self::AbortedTools => "aborted_tools",
            Self::StopHookPrevented => "stop_hook_prevented",
            Self::HookStopped => "hook_stopped",
            Self::ModelError { .. } => "model_error",
            Self::ReactiveCompactRetry => "reactive_compact_retry",
            Self::CollapseDrainRetry => "collapse_drain_retry",
            Self::MaxOutputTokensEscalate => "max_output_tokens_escalate",
            Self::MaxOutputTokensRecovery => "max_output_tokens_recovery",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_is_success() {
        assert!(Terminal::Completed.is_success());
        assert!(!Terminal::PromptTooLong.is_success());
    }

    #[test]
    fn retryable_variants() {
        assert!(Terminal::ReactiveCompactRetry.is_retryable());
        assert!(Terminal::CollapseDrainRetry.is_retryable());
        assert!(!Terminal::Completed.is_retryable());
        assert!(!Terminal::PromptTooLong.is_retryable());
    }

    #[test]
    fn tag_is_snake_case() {
        assert_eq!(Terminal::Completed.tag(), "completed");
        assert_eq!(Terminal::MaxTurns { turn_count: 5 }.tag(), "max_turns");
    }

    #[test]
    fn wire_shape_is_tagged() {
        let t = Terminal::MaxTurns { turn_count: 10 };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json, serde_json::json!({"reason": "max_turns", "turn_count": 10}));
        let back: Terminal = serde_json::from_value(json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn every_variant_roundtrips() {
        let variants = [
            Terminal::Completed,
            Terminal::MaxTurns { turn_count: 3 },
            Terminal::BlockingLimit,
            Terminal::TokenBudgetContinuation,
            Terminal::PromptTooLong,
            Terminal::ImageError,
            Terminal::AbortedStreaming,
            Terminal::AbortedTools,
            Terminal::StopHookPrevented,
            Terminal::HookStopped,
            Terminal::ModelError { classified: LlmApiErrorKind::RateLimit },
            Terminal::ReactiveCompactRetry,
            Terminal::CollapseDrainRetry,
            Terminal::MaxOutputTokensEscalate,
            Terminal::MaxOutputTokensRecovery,
        ];
        for v in variants {
            let json = serde_json::to_string(&v).unwrap();
            let back: Terminal = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back, "variant did not roundtrip");
        }
    }
}
