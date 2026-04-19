//! Types shared between `elena-memory` and the rest of the workspace.
//!
//! Kept in `elena-types` so crates that refer to memory (the store, the loop,
//! config) don't need a dependency on `elena-memory` itself.

use serde::{Deserialize, Serialize};

use crate::terminal::Terminal;

/// How a thread's agentic loop ended, from a memory-record perspective.
///
/// `Outcome::Completed` is the happy path. `Outcome::Failed(Terminal)`
/// preserves the exact reason so future `recall()` calls can filter out
/// unhelpful past failures. Tombstones, cancellations, and retries share the
/// `Failed` variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// The thread completed successfully.
    Completed,
    /// The thread ended with a failure; the inner `Terminal` explains why.
    Failed {
        /// Original terminal condition.
        terminal: Terminal,
    },
}

impl Outcome {
    /// True for the clean-completion case.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LlmApiErrorKind;

    #[test]
    fn completed_roundtrip() {
        let o = Outcome::Completed;
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "completed"}));
        let back: Outcome = serde_json::from_value(json).unwrap();
        assert_eq!(o, back);
    }

    #[test]
    fn failed_roundtrip_preserves_terminal() {
        let o = Outcome::Failed {
            terminal: Terminal::ModelError { classified: LlmApiErrorKind::RateLimit },
        };
        let json = serde_json::to_string(&o).unwrap();
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back);
    }

    #[test]
    fn is_success_matches_variant() {
        assert!(Outcome::Completed.is_success());
        assert!(!Outcome::Failed { terminal: Terminal::PromptTooLong }.is_success());
    }
}
