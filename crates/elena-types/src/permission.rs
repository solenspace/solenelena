//! Permission system.
//!
//! Three-outcome decisions — [`allow`](Permission::Allow),
//! [`ask`](Permission::Ask), [`deny`](Permission::Deny) — with rich bodies
//! for explanation, user-modified input, and content-block attachments (e.g.,
//! a user pasting an image with a rejection reason).
//!
//! Rules are grouped by [`PermissionRuleSource`] (user settings, policy,
//! session, etc.) and matched to tool names via [`PermissionRuleValue`]. The
//! [`PermissionSet`] is the materialized bag of rules a tool consults.
//!
//! Deterministic serialization: [`PermissionSet`] uses [`BTreeMap`] so cache
//! hashes over system prompts (which include permission markers) are stable
//! across processes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::message::ContentBlock;

/// The three permission outcomes. Matches the Anthropic / Claude Code
/// taxonomy exactly so hook authors can target the same wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    /// Tool call is permitted.
    Allow,
    /// Tool call requires user confirmation.
    Ask,
    /// Tool call is rejected.
    Deny,
}

/// Top-level permission mode for a request. Controls the default behavior
/// when no rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Ask for every side-effectful tool. The safe default.
    #[default]
    Default,
    /// Auto-accept edits to files under the workspace root.
    AcceptEdits,
    /// Bypass permission prompts entirely. Requires explicit opt-in per
    /// tenant; Elena operators should gate this with policy.
    BypassPermissions,
    /// Never ask — deny anything that would normally prompt.
    DontAsk,
    /// Plan-only mode. Side-effectful tools are blocked; advisory responses
    /// only.
    Plan,
}

/// Where a permission rule came from. Used for provenance, display, and
/// determining persistence destination on update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionRuleSource {
    /// User's global settings file.
    UserSettings,
    /// Project-level settings (per-workspace).
    ProjectSettings,
    /// Machine-local settings file.
    LocalSettings,
    /// Flags supplied via feature-flag service.
    FlagSettings,
    /// Policy file pushed by the operator.
    PolicySettings,
    /// A command-line argument.
    CliArg,
    /// Set by a slash command during this session.
    Command,
    /// Session-scoped override (does not persist beyond this session).
    Session,
}

/// The content of a permission rule — tool it applies to and optional
/// fine-grained matcher content (e.g., a bash pattern, a file glob).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    /// Tool name this rule targets.
    pub tool_name: String,
    /// Rule-specific content (tool-dependent meaning).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rule_content: Option<String>,
}

/// A fully-formed permission rule.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Origin of the rule.
    pub source: PermissionRuleSource,
    /// Behavior to apply when the rule matches.
    pub behavior: PermissionBehavior,
    /// The matcher.
    pub value: PermissionRuleValue,
}

/// Why a permission decision was reached.
///
/// Simplified from the reference TS source — Elena carries the reason
/// verbatim for audit and UI display; tenant-specific classifier stages are
/// app-level concerns and not modeled here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecisionReason {
    /// Decision came from an explicit rule match.
    Rule {
        /// The matched rule.
        rule: PermissionRule,
    },
    /// Decision came from the permission mode default.
    Mode {
        /// The active mode.
        mode: PermissionMode,
    },
    /// A pre-tool hook produced the decision.
    Hook {
        /// Hook name.
        hook_name: String,
        /// Optional human-readable reason.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        reason: Option<String>,
    },
    /// Catchall for reasons produced by app-level policy.
    Other {
        /// Reason text.
        reason: String,
    },
}

/// A persistence operation requested by a permission decision (e.g., user
/// chose "always allow" — the matching destination is recorded alongside).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PermissionUpdate {
    /// Add rules to a destination.
    AddRules {
        /// Where to persist.
        destination: PermissionUpdateDestination,
        /// Behavior these rules apply.
        behavior: PermissionBehavior,
        /// The rules.
        rules: Vec<PermissionRuleValue>,
    },
    /// Replace the rule set at a destination.
    ReplaceRules {
        /// Where to persist.
        destination: PermissionUpdateDestination,
        /// Behavior the rules apply.
        behavior: PermissionBehavior,
        /// Replacement rules.
        rules: Vec<PermissionRuleValue>,
    },
    /// Remove rules from a destination.
    RemoveRules {
        /// Where to persist.
        destination: PermissionUpdateDestination,
        /// Which behavior's rules to remove.
        behavior: PermissionBehavior,
        /// Rules to remove.
        rules: Vec<PermissionRuleValue>,
    },
    /// Change the active mode.
    SetMode {
        /// Where to persist the mode change.
        destination: PermissionUpdateDestination,
        /// New mode.
        mode: PermissionMode,
    },
}

/// Where to persist a permission update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// User's global settings.
    UserSettings,
    /// Per-workspace project settings.
    ProjectSettings,
    /// Machine-local settings.
    LocalSettings,
    /// Session-only (in-memory).
    Session,
    /// CLI arg (runtime override, does not persist).
    CliArg,
}

/// A permission decision — the result of consulting rules, mode, and hooks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum Permission {
    /// The tool call is permitted.
    Allow {
        /// Modified tool input (e.g., a classifier trimmed a dangerous flag).
        /// `None` means the original input is used.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        updated_input: Option<serde_json::Value>,
        /// True if the user manually edited the input.
        #[serde(default)]
        user_modified: bool,
        /// Why the decision was reached.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        decision_reason: Option<PermissionDecisionReason>,
        /// Content blocks to include with the tool result (e.g., an accept
        /// feedback note).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content_blocks: Vec<ContentBlock>,
    },
    /// The user must confirm before proceeding.
    Ask {
        /// Human-readable prompt to display.
        message: String,
        /// Candidate input if the user accepts (for preview/editing).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        updated_input: Option<serde_json::Value>,
        /// Why the decision was reached.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        decision_reason: Option<PermissionDecisionReason>,
        /// Suggested rule updates (e.g., "always allow this pattern").
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        suggestions: Vec<PermissionUpdate>,
    },
    /// The tool call is refused.
    Deny {
        /// Human-readable message explaining the denial.
        message: String,
        /// Reason. Required on deny (audit trail).
        decision_reason: PermissionDecisionReason,
    },
}

impl Permission {
    /// Construct a simple `Allow` with no overrides.
    #[must_use]
    pub fn allow() -> Self {
        Self::Allow {
            updated_input: None,
            user_modified: false,
            decision_reason: None,
            content_blocks: Vec::new(),
        }
    }

    /// Construct a simple `Deny` from a message and reason.
    #[must_use]
    pub fn deny(message: impl Into<String>, reason: PermissionDecisionReason) -> Self {
        Self::Deny { message: message.into(), decision_reason: reason }
    }

    /// The matching [`PermissionBehavior`] for this decision.
    #[must_use]
    pub const fn behavior(&self) -> PermissionBehavior {
        match self {
            Self::Allow { .. } => PermissionBehavior::Allow,
            Self::Ask { .. } => PermissionBehavior::Ask,
            Self::Deny { .. } => PermissionBehavior::Deny,
        }
    }
}

/// Materialized rule set a tool consults during `check_permissions`.
///
/// Keyed by [`PermissionRuleSource`] so the tool can tell which tier the
/// rule came from (important for showing the user why a call was blocked
/// and where the blocking rule lives).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionSet {
    /// Active mode.
    pub mode: PermissionMode,
    /// Rules that unconditionally allow (per source).
    pub always_allow: BTreeMap<PermissionRuleSource, Vec<String>>,
    /// Rules that unconditionally deny (per source).
    pub always_deny: BTreeMap<PermissionRuleSource, Vec<String>>,
    /// Rules that force an ask (per source).
    pub always_ask: BTreeMap<PermissionRuleSource, Vec<String>>,
    /// Additional directories in scope for file tools.
    #[serde(default)]
    pub additional_directories: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behavior_wire_values() {
        assert_eq!(
            serde_json::to_value(PermissionBehavior::Allow).unwrap(),
            serde_json::json!("allow")
        );
        assert_eq!(
            serde_json::to_value(PermissionBehavior::Ask).unwrap(),
            serde_json::json!("ask")
        );
        assert_eq!(
            serde_json::to_value(PermissionBehavior::Deny).unwrap(),
            serde_json::json!("deny")
        );
    }

    #[test]
    fn mode_wire_values_are_camel_case() {
        assert_eq!(
            serde_json::to_value(PermissionMode::Default).unwrap(),
            serde_json::json!("default")
        );
        assert_eq!(
            serde_json::to_value(PermissionMode::AcceptEdits).unwrap(),
            serde_json::json!("acceptEdits")
        );
        assert_eq!(
            serde_json::to_value(PermissionMode::BypassPermissions).unwrap(),
            serde_json::json!("bypassPermissions")
        );
    }

    #[test]
    fn source_wire_values_are_camel_case() {
        assert_eq!(
            serde_json::to_value(PermissionRuleSource::UserSettings).unwrap(),
            serde_json::json!("userSettings")
        );
        assert_eq!(
            serde_json::to_value(PermissionRuleSource::PolicySettings).unwrap(),
            serde_json::json!("policySettings")
        );
    }

    #[test]
    fn allow_omits_defaults() {
        let p = Permission::allow();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json, serde_json::json!({"behavior": "allow", "user_modified": false}));
    }

    #[test]
    fn deny_requires_reason() {
        let p = Permission::deny(
            "not allowed",
            PermissionDecisionReason::Other { reason: "policy".into() },
        );
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "behavior": "deny",
                "message": "not allowed",
                "decision_reason": {"type": "other", "reason": "policy"}
            })
        );
    }

    #[test]
    fn ask_with_suggestions_roundtrip() {
        let p = Permission::Ask {
            message: "Allow write to /etc?".into(),
            updated_input: Some(serde_json::json!({"path": "/etc/hosts"})),
            decision_reason: None,
            suggestions: vec![PermissionUpdate::AddRules {
                destination: PermissionUpdateDestination::Session,
                behavior: PermissionBehavior::Allow,
                rules: vec![PermissionRuleValue {
                    tool_name: "Edit".into(),
                    rule_content: Some("/etc/*".into()),
                }],
            }],
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: Permission = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn decision_behavior_matches() {
        assert_eq!(Permission::allow().behavior(), PermissionBehavior::Allow);
        assert_eq!(
            Permission::deny("no", PermissionDecisionReason::Other { reason: String::new() })
                .behavior(),
            PermissionBehavior::Deny
        );
    }

    #[test]
    fn permission_set_uses_deterministic_ordering() {
        // BTreeMap iteration order is deterministic — important for cache hashing.
        let mut set = PermissionSet::default();
        set.always_allow.insert(PermissionRuleSource::UserSettings, vec!["Read".into()]);
        set.always_allow.insert(PermissionRuleSource::ProjectSettings, vec!["Edit".into()]);
        let json_a = serde_json::to_string(&set).unwrap();
        let json_b = serde_json::to_string(&set).unwrap();
        assert_eq!(json_a, json_b);
    }
}
