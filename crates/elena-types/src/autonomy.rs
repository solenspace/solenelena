//! Autonomy modes and approval envelopes.
//!
//! Solen's core UX — Cautious, Moderate, YOLO — demands a first-class way
//! for the loop to pause between the model proposing a tool call and the
//! worker actually executing it. This module defines the vocabulary:
//!
//! - [`AutonomyMode`] is the per-thread policy knob (set on thread creation,
//!   persisted on `threads.autonomy_mode`).
//! - [`PendingApproval`] is one pending tool invocation the client must
//!   approve or deny before the loop resumes.
//! - [`ApprovalDecision`] is what the client sends back.
//!
//! The loop enforces these in [`elena-core`]: after streaming, if the
//! resolved `AutonomyMode` says "pause", the phase flips to
//! `AwaitingApproval` and a `StreamEvent::AwaitingApproval` goes out. The
//! worker releases the claim so another worker can resume once the client
//! has responded.

use serde::{Deserialize, Serialize};

use crate::id::ToolCallId;

/// Per-thread autonomy policy. Controls whether and where the loop pauses
/// before executing tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyMode {
    /// Pause before every tool call. Every step surfaces for explicit
    /// approval. Used for high-stakes enterprise workflows.
    Cautious,
    /// Pause only at "decision forks" — side-effecting tools (writes,
    /// payments, outbound messages). Read-only tools run without prompting.
    #[default]
    Moderate,
    /// No pause. The loop runs to completion; every action is logged but
    /// not gated. Used for trusted automation.
    Yolo,
}

impl AutonomyMode {
    /// Short stable tag for metrics/logs/DB serialization.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cautious => "cautious",
            Self::Moderate => "moderate",
            Self::Yolo => "yolo",
        }
    }

    /// Parse from the DB column value. Unknown strings fall back to
    /// [`AutonomyMode::Moderate`] — failing a whole thread load over a
    /// schema drift is worse than degrading to the safe default.
    #[must_use]
    pub fn from_str_or_default(s: &str) -> Self {
        match s {
            "cautious" => Self::Cautious,
            "yolo" => Self::Yolo,
            _ => Self::Moderate,
        }
    }
}

/// One tool invocation awaiting client approval.
///
/// Carries enough information for a client UI to render "Elena wants to
/// call `<tool>` with `<input summary>` — allow or deny?" without the
/// client needing to re-fetch anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// The tool-call ID the LLM emitted. Echoed back by the client in
    /// [`ApprovalDecision::tool_use_id`] so the worker can correlate.
    pub tool_use_id: ToolCallId,
    /// Tool name as registered — e.g., `"slack.post_message"`.
    pub tool_name: String,
    /// A short human-readable description of what the tool will do. The
    /// worker derives this from the plugin manifest + input payload; it's
    /// purely for UI.
    pub summary: String,
    /// The raw input the model proposed. Clients may display this, let
    /// the user edit it before approval (via `ApprovalDecision::edits`),
    /// or hide it behind a "show raw payload" toggle.
    pub input: serde_json::Value,
}

/// Client response to one pending approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalDecision {
    /// The tool-call ID this decision applies to.
    pub tool_use_id: ToolCallId,
    /// What to do.
    pub decision: ApprovalVerdict,
    /// Optional override for the tool's input. When `Some`, the worker
    /// replaces the model's proposed input before executing. Keeps the
    /// decision audit-trail honest: the client saw what the model wanted,
    /// then chose to adjust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<serde_json::Value>,
}

/// Allow or deny a single pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalVerdict {
    /// Run the tool. If `edits` is set on the enclosing decision, use
    /// the edited input.
    Allow,
    /// Do not run the tool. The worker will synthesise a `tool_result`
    /// of `"denied by user"` so the model sees a response and can
    /// continue coherently.
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_mode_roundtrips() {
        for mode in [AutonomyMode::Cautious, AutonomyMode::Moderate, AutonomyMode::Yolo] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: AutonomyMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }

    #[test]
    fn autonomy_mode_str_roundtrips() {
        for mode in [AutonomyMode::Cautious, AutonomyMode::Moderate, AutonomyMode::Yolo] {
            assert_eq!(AutonomyMode::from_str_or_default(mode.as_str()), mode);
        }
    }

    #[test]
    fn unknown_autonomy_mode_defaults_to_moderate() {
        assert_eq!(AutonomyMode::from_str_or_default("nonsense"), AutonomyMode::Moderate);
        assert_eq!(AutonomyMode::from_str_or_default(""), AutonomyMode::Moderate);
    }

    #[test]
    fn default_is_moderate() {
        assert_eq!(AutonomyMode::default(), AutonomyMode::Moderate);
    }

    #[test]
    fn pending_approval_roundtrips() {
        let pa = PendingApproval {
            tool_use_id: ToolCallId::new(),
            tool_name: "slack.post_message".into(),
            summary: "Post to #sales".into(),
            input: serde_json::json!({"channel": "#sales", "text": "hi"}),
        };
        let json = serde_json::to_string(&pa).unwrap();
        let back: PendingApproval = serde_json::from_str(&json).unwrap();
        assert_eq!(pa, back);
    }

    #[test]
    fn approval_decision_allow_with_edits() {
        let decision = ApprovalDecision {
            tool_use_id: ToolCallId::new(),
            decision: ApprovalVerdict::Allow,
            edits: Some(serde_json::json!({"text": "overridden"})),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: ApprovalDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, back);
    }

    #[test]
    fn approval_decision_deny_omits_edits() {
        let decision = ApprovalDecision {
            tool_use_id: ToolCallId::new(),
            decision: ApprovalVerdict::Deny,
            edits: None,
        };
        let json = serde_json::to_value(&decision).unwrap();
        assert!(json.get("edits").is_none(), "edits should be omitted when None: {json}");
    }
}
