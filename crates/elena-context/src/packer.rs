//! Token-aware context window packer.
//!
//! Input: a chronologically-ordered `Vec<Message>` (after retrieval + merge),
//! a `TokenCounter`, and a budget in tokens.
//!
//! Output: a subset that fits the budget, preserving chronological order and
//! always keeping the tail (most-recent messages). When over budget, the
//! packer drops rows from the middle (oldest-first, skipping the tail).

use elena_types::Message;

use crate::tokens::TokenCounter;

/// How much headroom to reserve below the budget to absorb tokenization error.
const HEADROOM_RATIO: f32 = 0.10;

/// Tail length (most-recent messages) that is kept no matter what.
///
/// The last user message + any immediately preceding `tool_result` + the last
/// assistant message typically land in a 3-message tail. Keeping 4 gives one
/// row of slack for multi-part tool turns.
const TAIL_KEEP: usize = 4;

/// Return a chronologically-ordered subset of `messages` whose approximate
/// token total is at or below `budget_tokens`.
///
/// Strategy:
/// 1. Always include the last [`TAIL_KEEP`] messages.
/// 2. Fill from the front, newest-to-oldest, while under budget.
///
/// If the tail alone exceeds the budget, the function still returns the full
/// tail — callers must decide whether to bail or summarize.
#[must_use]
pub fn pack(messages: Vec<Message>, counter: &TokenCounter, budget_tokens: u32) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }
    let effective_budget = apply_headroom(budget_tokens);
    let costs: Vec<u32> = messages.iter().map(|m| message_tokens(m, *counter)).collect();
    let n = messages.len();

    // Always keep the tail (most-recent TAIL_KEEP rows).
    let tail_start = n.saturating_sub(TAIL_KEEP);
    let tail_cost: u64 = costs[tail_start..].iter().map(|&c| u64::from(c)).sum();
    let mut keep: Vec<bool> = (0..n).map(|i| i >= tail_start).collect();

    // Fill from newest-to-oldest (i.e. indexes just below tail_start),
    // skipping already-kept and budget-exceeding rows.
    let mut used: u64 = tail_cost;
    let budget_u64 = u64::from(effective_budget);
    for i in (0..tail_start).rev() {
        if used.saturating_add(u64::from(costs[i])) > budget_u64 {
            continue;
        }
        keep[i] = true;
        used = used.saturating_add(u64::from(costs[i]));
    }

    messages
        .into_iter()
        .enumerate()
        .filter_map(|(i, m)| if keep[i] { Some(m) } else { None })
        .collect()
}

fn message_tokens(msg: &Message, counter: TokenCounter) -> u32 {
    // Sum every text-like block's length. Non-text blocks (image, tool_use
    // JSON, tool_result structured) contribute a flat approximation.
    use elena_types::{ContentBlock, ToolResultContent};
    let mut total: u32 = 0;
    for block in &msg.content {
        let delta = match block {
            ContentBlock::Text { text, .. } => counter.count(text),
            ContentBlock::Thinking { thinking, .. } => counter.count(thinking),
            ContentBlock::RedactedThinking { .. } => 8,
            ContentBlock::ToolUse { input, .. } => {
                counter.count(&input.to_string()).saturating_add(16)
            }
            ContentBlock::ToolResult { content, .. } => match content {
                ToolResultContent::Text(t) => counter.count(t),
                ToolResultContent::Blocks(_) => 32,
            },
            ContentBlock::Image { .. } => 64,
        };
        total = total.saturating_add(delta);
    }
    // Add a 4-token overhead per message for role + structural framing.
    total.saturating_add(4)
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn apply_headroom(budget: u32) -> u32 {
    let trimmed = (budget as f32) * (1.0 - HEADROOM_RATIO);
    trimmed.floor() as u32
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use elena_types::{ContentBlock, Message, MessageId, MessageKind, Role, TenantId, ThreadId};

    use super::*;

    fn msg(text: &str) -> Message {
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

    #[test]
    fn empty_input_returns_empty() {
        let out = pack(Vec::new(), &TokenCounter::new(), 1_000);
        assert!(out.is_empty());
    }

    #[test]
    fn under_budget_keeps_everything() {
        let inputs = vec![msg("a"), msg("b"), msg("c"), msg("d")];
        let out = pack(inputs.clone(), &TokenCounter::new(), 10_000);
        assert_eq!(out.len(), inputs.len());
    }

    #[test]
    fn over_budget_drops_oldest_keeps_tail() {
        // 10 messages, each ~100 chars (~32 tokens + 4 frame = 36). Budget
        // ~90 tokens → after 10% headroom = 81. TAIL_KEEP=4 consumes ~4*36
        // = 144 tokens — exceeds budget, so only tail stays.
        let inputs: Vec<Message> = (0..10).map(|i| msg(&"x".repeat(100 * (i + 1)))).collect();
        let out = pack(inputs.clone(), &TokenCounter::new(), 90);
        assert_eq!(out.len(), TAIL_KEEP);
        assert_eq!(out[0].id, inputs[6].id);
        assert_eq!(out[3].id, inputs[9].id);
    }

    #[test]
    fn tail_always_kept_even_if_over_budget() {
        // 4 messages, budget of 1 token. Tail = all 4. Packer returns all 4.
        let inputs = vec![
            msg(&"a".repeat(200)),
            msg(&"b".repeat(200)),
            msg(&"c".repeat(200)),
            msg(&"d".repeat(200)),
        ];
        let out = pack(inputs.clone(), &TokenCounter::new(), 1);
        assert_eq!(out.len(), inputs.len(), "tail must survive under-budget");
    }

    #[test]
    fn preserves_chronological_order() {
        let inputs: Vec<Message> = (0..8).map(|_| msg("hello")).collect();
        let ids: Vec<_> = inputs.iter().map(|m| m.id).collect();
        let out = pack(inputs, &TokenCounter::new(), 10_000);
        let out_ids: Vec<_> = out.iter().map(|m| m.id).collect();
        assert_eq!(out_ids, ids);
    }
}
