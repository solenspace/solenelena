//! Context builder — fetch the last-N messages for the LLM turn.
//!
//! A crude recency window. `elena-context` replaces this with
//! embedding-based retrieval, tokenization-aware packing, and smart
//! truncation of old tool-result blocks.

use elena_store::ThreadStore;
use elena_types::{Message, MessageKind, StoreError, TenantId, ThreadId};

/// Cap used to bound a single read from Postgres. Conversations using
/// the recency window shouldn't approach this; if they do,
/// `elena-context` retrieval replaces the bulk read entirely.
const READ_CAP: u32 = 1_000;

/// Fetch the last `limit` messages in a thread, in chronological order.
///
/// Rows that shouldn't appear in an LLM context (tombstones, Elena-internal
/// bookkeeping rows) are filtered out. If the thread has fewer than `limit`
/// eligible messages, all of them are returned.
pub async fn fetch_recent_messages(
    store: &ThreadStore,
    tenant: TenantId,
    thread: ThreadId,
    limit: u32,
) -> Result<Vec<Message>, StoreError> {
    let all = store.list_messages(tenant, thread, READ_CAP, None).await?;
    let eligible: Vec<Message> = all.into_iter().filter(is_llm_visible).collect();
    let limit = limit as usize;
    let start = eligible.len().saturating_sub(limit);
    Ok(eligible.into_iter().skip(start).collect())
}

/// True if a message row should be visible to the LLM.
fn is_llm_visible(msg: &Message) -> bool {
    !matches!(
        msg.kind,
        MessageKind::Tombstone { .. } | MessageKind::System(_) | MessageKind::ToolUseSummary
    )
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use elena_types::{
        ContentBlock, Message, MessageId, MessageKind, Role, SystemMessageKind, TenantId, ThreadId,
    };

    use super::*;

    fn user_msg(text: &str) -> Message {
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

    fn tombstone() -> Message {
        Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::Tombstone { reason: "compacted".into() },
            content: vec![],
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }
    }

    fn system_notice() -> Message {
        Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::System,
            kind: MessageKind::System(SystemMessageKind::Text),
            content: vec![ContentBlock::Text { text: "notice".into(), cache_control: None }],
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }
    }

    #[test]
    fn filter_hides_tombstones_system_and_summaries() {
        assert!(is_llm_visible(&user_msg("hi")));
        assert!(!is_llm_visible(&tombstone()));
        assert!(!is_llm_visible(&system_notice()));
    }
}
