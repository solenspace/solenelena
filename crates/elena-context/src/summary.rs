//! Extract a compact episode summary from a completed thread's message log.
//!
//! Uses a pragmatic non-LLM heuristic: the first user message's text,
//! truncated to 400 characters. That captures the user's stated task
//! 95% of the time and costs nothing to compute. A swap to
//! `Summarizer` (LLM-backed) is possible later if retrieval quality
//! proves insufficient.

use elena_types::{ContentBlock, Message, MessageKind, Role};

/// Hard cap for the stored `task_summary` field.
pub const MAX_SUMMARY_LEN: usize = 400;

/// Extract a compact summary + the list of tool names used, in order, from
/// a thread's full message log.
#[must_use]
pub fn extract_summary(messages: &[Message]) -> Summary {
    let task_summary = first_user_text(messages)
        .map_or_else(|| fallback_summary(messages), |t| truncate_to(t, MAX_SUMMARY_LEN));
    let actions = tool_names_in_order(messages);
    Summary { task_summary, actions }
}

/// Fields an [`crate::memory::EpisodicMemory::record_episode`] needs beyond
/// tenant / workspace / thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Task summary — what the thread was trying to do.
    pub task_summary: String,
    /// Tool names used, in the order they were invoked.
    pub actions: Vec<String>,
}

fn first_user_text(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|m| {
        if !matches!(m.role, Role::User) || !matches!(m.kind, MessageKind::User) {
            return None;
        }
        extract_text(m)
    })
}

fn fallback_summary(messages: &[Message]) -> String {
    // If no user message surfaced (e.g., template-initialized thread), fall
    // back to the first text chunk we can find — gives us at least a
    // breadcrumb.
    let raw =
        messages.iter().find_map(extract_text).unwrap_or_else(|| "(no text in thread)".to_owned());
    truncate_to(raw, MAX_SUMMARY_LEN)
}

fn extract_text(m: &Message) -> Option<String> {
    let mut out = String::new();
    for block in &m.content {
        if let ContentBlock::Text { text, .. } = block {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn tool_names_in_order(messages: &[Message]) -> Vec<String> {
    let mut out = Vec::new();
    for m in messages {
        for block in &m.content {
            if let ContentBlock::ToolUse { name, .. } = block {
                out.push(name.clone());
            }
        }
    }
    out
}

fn truncate_to(mut s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    // Truncate on a char boundary to avoid splitting multi-byte sequences.
    let mut boundary = 0;
    for (idx, _) in s.char_indices().take(max) {
        boundary = idx;
    }
    s.truncate(boundary);
    s.push('\u{2026}'); // ellipsis
    s
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use elena_types::{ContentBlock, Message, MessageId, Role, TenantId, ThreadId, ToolCallId};
    use serde_json::json;

    use super::*;

    fn user(text: &str) -> Message {
        Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::User,
            content: vec![ContentBlock::Text { text: text.into(), cache_control: None }],
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }
    }

    fn assistant_with_tool(name: &str) -> Message {
        Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::Assistant,
            kind: MessageKind::Assistant { stop_reason: None, is_api_error: false },
            content: vec![ContentBlock::ToolUse {
                id: ToolCallId::new(),
                name: name.into(),
                input: json!({}),
                cache_control: None,
            }],
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }
    }

    #[test]
    fn first_user_message_is_the_summary() {
        let s =
            extract_summary(&[user("Reverse hello elena"), assistant_with_tool("reverse_text")]);
        assert_eq!(s.task_summary, "Reverse hello elena");
        assert_eq!(s.actions, vec!["reverse_text"]);
    }

    #[test]
    fn long_summaries_are_truncated_with_ellipsis() {
        let long = "x".repeat(MAX_SUMMARY_LEN * 2);
        let s = extract_summary(&[user(&long)]);
        assert!(
            s.task_summary.ends_with('\u{2026}'),
            "expected ellipsis, got {:?}",
            s.task_summary
        );
        assert!(s.task_summary.chars().count() <= MAX_SUMMARY_LEN + 1);
    }

    #[test]
    fn actions_preserve_tool_order() {
        let s = extract_summary(&[
            user("do the thing"),
            assistant_with_tool("tool_a"),
            assistant_with_tool("tool_b"),
            assistant_with_tool("tool_a"),
        ]);
        assert_eq!(s.actions, vec!["tool_a", "tool_b", "tool_a"]);
    }

    #[test]
    fn empty_thread_uses_fallback() {
        let s = extract_summary(&[]);
        assert_eq!(s.task_summary, "(no text in thread)");
        assert!(s.actions.is_empty());
    }
}
