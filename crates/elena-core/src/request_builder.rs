//! Turn a [`LoopState`] + context messages + registry schemas into an
//! [`LlmRequest`].

use elena_llm::{LlmRequest, RequestOptions, ToolSchema};
use elena_types::Message;

use crate::state::LoopState;

/// Build the per-turn LLM request.
///
/// `messages` is the chronologically-ordered context window from
/// [`context_builder::fetch_recent_messages`](crate::context_builder::fetch_recent_messages).
/// `tools` is the registry projection — it's already in Anthropic wire
/// shape. Phase 3 emits no `tool_choice` (the model decides whether to call
/// a tool), no thinking budget, and leaves temperature unset.
#[must_use]
pub fn build_llm_request(
    state: &LoopState,
    messages: Vec<Message>,
    tools: Vec<ToolSchema>,
) -> LlmRequest {
    LlmRequest {
        provider: state.provider.clone(),
        model: state.model.clone(),
        tenant: state.tenant.clone(),
        messages,
        system: state.system_prompt.clone(),
        tools,
        tool_choice: None,
        max_tokens: state.max_tokens_per_turn,
        thinking: None,
        temperature: None,
        top_p: None,
        stop_sequences: Vec::new(),
        options: RequestOptions::default(),
    }
}

#[cfg(test)]
mod tests {
    use elena_types::{
        BudgetLimits, ContentBlock, ModelId, PermissionSet, SessionId, TenantContext, TenantId,
        TenantTier, ThreadId, UserId, WorkspaceId,
    };
    use serde_json::json;

    use super::*;

    fn tenant() -> TenantContext {
        TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::default(),
            tier: TenantTier::Pro,
            metadata: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn builds_request_with_expected_wiring() {
        let mut state = LoopState::new(
            ThreadId::new(),
            tenant(),
            ModelId::new("claude-haiku-4-5-20251001"),
            20,
            1_024,
        );
        state.system_prompt =
            vec![ContentBlock::Text { text: "Be brief.".into(), cache_control: None }];

        let msgs = vec![Message::user_text(TenantId::new(), state.thread_id, "hello")];
        let tools = vec![ToolSchema::new(json!({"name": "echo"}))];

        let req = build_llm_request(&state, msgs.clone(), tools.clone());

        assert_eq!(req.model.as_str(), "claude-haiku-4-5-20251001");
        assert_eq!(req.max_tokens, 1_024);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.system.len(), 1);
        assert_eq!(req.tools.len(), 1);
        assert!(req.tool_choice.is_none());
        assert!(req.thinking.is_none());
        assert!(req.temperature.is_none());
        assert!(req.options.enable_prompt_caching);
    }

    #[test]
    fn empty_system_prompt_stays_empty() {
        let state = LoopState::new(
            ThreadId::new(),
            tenant(),
            ModelId::new("claude-haiku-4-5-20251001"),
            10,
            512,
        );
        let req = build_llm_request(&state, Vec::new(), Vec::new());
        assert!(req.system.is_empty());
        assert!(req.tools.is_empty());
        assert!(req.messages.is_empty());
    }
}
