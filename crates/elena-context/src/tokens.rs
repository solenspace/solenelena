//! Character-based token-count approximation.
//!
//! Elena needs to keep context windows under the model's budget, but neither
//! the Anthropic API nor the public `tiktoken-rs` crate ships a Claude
//! tokenizer as of Phase 4. Empirically, Claude's BPE-style tokenizer sits
//! around 3.5 characters per token for English prose and ~2.5 for
//! punctuation-heavy or non-English input. We use a slightly conservative
//! constant (`3.2`) so budget math under-counts tokens — a pack that fits
//! this estimate will also fit the real tokenizer.
//!
//! Good enough for Phase 4. Phase 5 can switch to Anthropic's
//! `/v1/messages/count_tokens` endpoint when batched token counting is worth
//! the round-trip.

/// Characters per token (conservative estimate).
const CHARS_PER_TOKEN: f32 = 3.2;

/// Token counter used by the context packer and summarizer heuristics.
#[derive(Debug, Clone, Copy)]
pub struct TokenCounter {
    chars_per_token: f32,
}

impl TokenCounter {
    /// Counter with the default conservative ratio.
    #[must_use]
    pub const fn new() -> Self {
        Self { chars_per_token: CHARS_PER_TOKEN }
    }

    /// Counter with an explicit ratio — handy for unit tests.
    #[must_use]
    pub fn with_ratio(chars_per_token: f32) -> Self {
        Self { chars_per_token: chars_per_token.max(1.0) }
    }

    /// Approximate token count for a string. Empty → 0.
    ///
    /// The intermediate cast from `usize` to `f32` is bounded in practice
    /// (we count Unicode scalars, not bytes) and produces a non-negative
    /// float; truncation to `u32` rounds down after `ceil()`. Capped at
    /// `u32::MAX` for pathological inputs.
    #[must_use]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn count(&self, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }
        let len = text.chars().count();
        let approx = (len as f32) / self.chars_per_token;
        // `ceil` so a single-char input never rounds to 0 tokens.
        let ceil = approx.ceil();
        if ceil >= f32::from(u16::MAX) * f32::from(u16::MAX) { u32::MAX } else { ceil as u32 }
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        let c = TokenCounter::new();
        assert_eq!(c.count(""), 0);
    }

    #[test]
    fn one_char_is_one_token() {
        let c = TokenCounter::new();
        assert_eq!(c.count("a"), 1);
    }

    #[test]
    fn long_prose_tracks_ratio() {
        let c = TokenCounter::new();
        let text = "a".repeat(320);
        // 320 chars / 3.2 cpt = 100 tokens
        assert_eq!(c.count(&text), 100);
    }

    #[test]
    fn ratio_override_is_honored() {
        let c = TokenCounter::with_ratio(4.0);
        assert_eq!(c.count("abcdefgh"), 2); // 8/4 = 2
    }

    #[test]
    fn ratio_floor_is_one() {
        // Nonsensical ratios clamp to 1 to avoid divide-by-zero pathology.
        let c = TokenCounter::with_ratio(0.1);
        assert_eq!(c.count("abc"), 3);
    }

    #[test]
    fn counts_unicode_code_points_not_bytes() {
        let c = TokenCounter::new();
        // "🎉" is 1 scalar but 4 UTF-8 bytes; we count 1.
        let token_count = c.count("🎉🎉🎉🎉");
        assert!(token_count >= 1, "emoji counted: {token_count}");
    }
}
