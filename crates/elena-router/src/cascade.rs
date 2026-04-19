//! Reactive cascade check — runs after the LLM stream completes and decides
//! whether the loop should re-issue the turn at a higher tier.
//!
//! Signals (deterministic for Phase 4):
//! 1. The model refused (keyword match on common refusal phrases).
//! 2. The model emitted a `tool_use` for a tool that isn't registered
//!    (hallucination).
//! 3. The assistant emitted no text and no tool calls (empty turn).
//! 4. The assistant emitted a single very short phrase (<10 chars) and no
//!    tool calls when tools were available.
//!
//! Any signal + a non-`Premium` current tier + escalations-so-far below the
//! configured cap → `Escalate(next_tier)`.

use elena_tools::ToolInvocation;
use elena_types::ModelTier;
use serde::{Deserialize, Serialize};

/// Outcome of [`cascade_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CascadeDecision {
    /// Keep the turn's output.
    Accept,
    /// Re-issue the turn at `tier`.
    Escalate(ModelTier),
}

/// Inputs for a cascade check.
#[derive(Debug, Clone, Copy)]
pub struct CascadeInputs<'a> {
    /// Full assistant text emitted this turn (concatenated text deltas).
    pub assistant_text: &'a str,
    /// Tool calls the model requested.
    pub tool_calls: &'a [ToolInvocation],
    /// Tool names currently registered; used to flag hallucinations.
    pub registered_tool_names: &'a [&'a str],
    /// Whether any tools are registered at all.
    pub tools_present: bool,
    /// The current tier.
    pub current_tier: ModelTier,
    /// How many escalations already happened for this turn.
    pub escalations_so_far: u32,
    /// Maximum escalations allowed per turn.
    pub max_escalations: u32,
}

/// Decide whether to escalate.
#[must_use]
pub fn cascade_check(inp: CascadeInputs<'_>) -> CascadeDecision {
    if inp.escalations_so_far >= inp.max_escalations {
        return CascadeDecision::Accept;
    }
    let Some(next) = inp.current_tier.escalate() else {
        // Already at Premium.
        return CascadeDecision::Accept;
    };

    if is_refusal(inp.assistant_text)
        || has_hallucinated_tool(inp.tool_calls, inp.registered_tool_names)
        || is_empty_turn(inp.assistant_text, inp.tool_calls)
        || is_brief_text_when_tools_expected(inp.assistant_text, inp.tool_calls, inp.tools_present)
    {
        CascadeDecision::Escalate(next)
    } else {
        CascadeDecision::Accept
    }
}

fn is_refusal(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "i cannot",
        "i can't",
        "i am not able",
        "i'm unable",
        "i am unable",
        "i won't",
        "i will not",
        "i'm sorry, but",
        "as an ai",
        "i do not have the ability",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

fn has_hallucinated_tool(calls: &[ToolInvocation], registered: &[&str]) -> bool {
    calls.iter().any(|call| !registered.iter().any(|n| *n == call.name))
}

fn is_empty_turn(text: &str, calls: &[ToolInvocation]) -> bool {
    text.trim().is_empty() && calls.is_empty()
}

fn is_brief_text_when_tools_expected(
    text: &str,
    calls: &[ToolInvocation],
    tools_present: bool,
) -> bool {
    tools_present && calls.is_empty() && text.trim().chars().count() < 10
}

#[cfg(test)]
mod tests {
    use elena_types::ToolCallId;
    use serde_json::json;

    use super::*;

    fn call(name: &str) -> ToolInvocation {
        ToolInvocation { id: ToolCallId::new(), name: name.to_owned(), input: json!({}) }
    }

    fn base() -> CascadeInputs<'static> {
        CascadeInputs {
            assistant_text: "Here is a reasonable answer.",
            tool_calls: &[],
            registered_tool_names: &["echo"],
            tools_present: true,
            current_tier: ModelTier::Fast,
            escalations_so_far: 0,
            max_escalations: 2,
        }
    }

    #[test]
    fn accept_when_everything_looks_good() {
        let out = cascade_check(base());
        assert_eq!(out, CascadeDecision::Accept);
    }

    #[test]
    fn escalate_on_refusal() {
        let mut inp = base();
        inp.assistant_text = "I'm sorry, but I cannot help with that.";
        assert_eq!(cascade_check(inp), CascadeDecision::Escalate(ModelTier::Standard));
    }

    #[test]
    fn escalate_on_hallucinated_tool() {
        let calls = [call("not_registered")];
        let mut inp = base();
        inp.tool_calls = &calls;
        assert_eq!(cascade_check(inp), CascadeDecision::Escalate(ModelTier::Standard));
    }

    #[test]
    fn accept_when_all_tool_names_are_registered() {
        let calls = [call("echo")];
        let mut inp = base();
        inp.tool_calls = &calls;
        assert_eq!(cascade_check(inp), CascadeDecision::Accept);
    }

    #[test]
    fn escalate_on_empty_turn() {
        let mut inp = base();
        inp.assistant_text = "   ";
        assert_eq!(cascade_check(inp), CascadeDecision::Escalate(ModelTier::Standard));
    }

    #[test]
    fn escalate_on_too_brief_when_tools_present() {
        let mut inp = base();
        inp.assistant_text = "ok";
        assert_eq!(cascade_check(inp), CascadeDecision::Escalate(ModelTier::Standard));
    }

    #[test]
    fn brief_without_tools_available_does_not_escalate() {
        let mut inp = base();
        inp.assistant_text = "ok";
        inp.tools_present = false;
        inp.registered_tool_names = &[];
        assert_eq!(cascade_check(inp), CascadeDecision::Accept);
    }

    #[test]
    fn accept_when_already_premium() {
        let mut inp = base();
        inp.assistant_text = "I'm sorry, but I cannot help.";
        inp.current_tier = ModelTier::Premium;
        assert_eq!(cascade_check(inp), CascadeDecision::Accept);
    }

    #[test]
    fn accept_when_escalation_cap_hit() {
        let mut inp = base();
        inp.assistant_text = "I cannot.";
        inp.escalations_so_far = 2;
        assert_eq!(cascade_check(inp), CascadeDecision::Accept);
    }

    #[test]
    fn escalation_honors_tier_ordering() {
        let mut inp = base();
        inp.assistant_text = "I cannot.";
        inp.current_tier = ModelTier::Standard;
        assert_eq!(cascade_check(inp), CascadeDecision::Escalate(ModelTier::Premium));
    }
}
