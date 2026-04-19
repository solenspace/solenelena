//! Autonomy-mode decision: "should these proposed tool calls pause for
//! user approval, or run immediately?"
//!
//! Rules, in order:
//!
//! 1. [`AutonomyMode::Yolo`] — never pause.
//! 2. [`AutonomyMode::Cautious`] — always pause if there is at least one
//!    tool call.
//! 3. [`AutonomyMode::Moderate`] — pause only when at least one call is a
//!    "decision fork". A decision fork is any tool that mutates state or
//!    is visible outside Elena (sends a message, writes a row, moves
//!    money). Read-only calls do not fork the conversation and run
//!    without prompting.
//!
//! The "read-only vs side-effecting" classification is a **name
//! heuristic** scoped to Phase 7. The plugin manifest in Phase 8+ will
//! carry an explicit `side_effect: bool` field and this module will
//! switch to reading it directly.

use elena_tools::ToolInvocation;
use elena_types::AutonomyMode;

/// Known read-only verbs. A tool whose last name-segment (after the final
/// dot) exactly matches one of these — or begins with `<verb>_` in
/// snake-case — is treated as side-effect-free under
/// [`AutonomyMode::Moderate`].
const READ_ONLY_VERBS: &[&str] = &[
    "list",
    "get",
    "read",
    "search",
    "query",
    "find",
    "fetch",
    "describe",
    "lookup",
    "show",
    "view",
    "summarize",
    "count",
];

/// Decide whether the current batch of proposed tool calls should pause
/// for client approval before running.
#[must_use]
pub fn requires_approval(autonomy: AutonomyMode, invocations: &[ToolInvocation]) -> bool {
    if invocations.is_empty() {
        return false;
    }
    match autonomy {
        AutonomyMode::Yolo => false,
        AutonomyMode::Cautious => true,
        AutonomyMode::Moderate => invocations.iter().any(|inv| is_decision_fork(&inv.name)),
    }
}

/// Heuristic: is this tool a decision fork (side-effecting / externally
/// visible) or a read-only query?
///
/// We look at the **last segment** of the dot-separated tool name since
/// that is what most plugin manifests use for the action verb
/// (`slack.post_message`, `shopify.orders.list`, `stripe.payouts.create`).
/// Exact matches win; otherwise the segment's prefix is checked against a
/// small list of known side-effect verbs.
///
/// Conservative default: unknown tools are treated as forks. A new tool
/// that forgets to follow the naming convention should pause the loop
/// rather than fire silently.
#[must_use]
pub fn is_decision_fork(tool_name: &str) -> bool {
    let last = tool_name.rsplit('.').next().unwrap_or(tool_name);

    if READ_ONLY_VERBS.contains(&last) {
        return false;
    }

    // `list_*`, `get_*`, etc. Snake-case variants on actions.
    for verb in READ_ONLY_VERBS {
        if last.starts_with(verb) && last.as_bytes().get(verb.len()) == Some(&b'_') {
            return false;
        }
    }

    // Unknown verb → treat as a fork. Better to pause than to fire a
    // wrong-labelled write-tool silently.
    true
}

#[cfg(test)]
mod tests {
    use elena_tools::ToolInvocation;
    use elena_types::{AutonomyMode, ToolCallId};

    use super::*;

    fn inv(name: &str) -> ToolInvocation {
        ToolInvocation {
            id: ToolCallId::new(),
            name: name.to_owned(),
            input: serde_json::Value::Null,
        }
    }

    #[test]
    fn yolo_never_pauses() {
        assert!(!requires_approval(AutonomyMode::Yolo, &[inv("stripe.payouts.create")]));
    }

    #[test]
    fn cautious_always_pauses_for_any_tool() {
        assert!(requires_approval(AutonomyMode::Cautious, &[inv("shopify.orders.list")]));
        assert!(requires_approval(AutonomyMode::Cautious, &[inv("stripe.payouts.create")]));
    }

    #[test]
    fn cautious_no_pause_on_empty_batch() {
        assert!(!requires_approval(AutonomyMode::Cautious, &[]));
    }

    #[test]
    fn moderate_pauses_on_writes_only() {
        assert!(!requires_approval(AutonomyMode::Moderate, &[inv("shopify.orders.list")]));
        assert!(!requires_approval(AutonomyMode::Moderate, &[inv("gmail.search")]));
        assert!(requires_approval(AutonomyMode::Moderate, &[inv("slack.post_message")]));
        assert!(requires_approval(AutonomyMode::Moderate, &[inv("stripe.payouts.create")]));
    }

    #[test]
    fn moderate_pauses_if_any_invocation_is_a_fork() {
        let mixed = vec![inv("shopify.orders.list"), inv("stripe.payouts.create")];
        assert!(requires_approval(AutonomyMode::Moderate, &mixed));
    }

    #[test]
    fn snake_case_read_verbs_do_not_fork() {
        assert!(!is_decision_fork("notion.list_pages"));
        assert!(!is_decision_fork("sheets.get_rows"));
        assert!(!is_decision_fork("gmail.search_threads"));
    }

    #[test]
    fn unknown_verb_treated_as_fork() {
        assert!(is_decision_fork("mystery.zap"));
        assert!(is_decision_fork("unknown"));
    }
}
