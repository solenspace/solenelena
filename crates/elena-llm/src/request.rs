//! Request types for the LLM client.
//!
//! [`LlmRequest`] is the high-level, Elena-friendly request shape. The
//! Anthropic wire body is assembled by the client — see
//! `anthropic::wire_body` (Step 7). The types here are deliberately
//! independent of any HTTP client so they can be constructed, tested, and
//! serialized without pulling in `reqwest`.

use elena_types::{ContentBlock, Message, ModelId, TenantContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::retry::RetryPolicy;

/// A single request to the LLM.
///
/// Build once per turn. Cloneable so retries can reuse the same value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    /// Which provider (by name) should serve this request.
    ///
    /// Matches one of the keys in
    /// [`LlmMultiplexer`](crate::LlmMultiplexer) — typically `"anthropic"`,
    /// `"openrouter"`, or `"groq"`. Defaults to `"anthropic"` for backward
    /// compatibility with existing call sites.
    #[serde(default = "default_provider")]
    pub provider: String,

    /// The model to invoke.
    pub model: ModelId,

    /// The requesting tenant's context — drives multi-tenant metadata and
    /// prompt-cache eligibility.
    pub tenant: TenantContext,

    /// Conversation history, user turn last.
    pub messages: Vec<Message>,

    /// System prompt as a list of content blocks (cache-ready).
    ///
    /// Empty vec means "no system prompt."
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentBlock>,

    /// Tool definitions visible to the model.
    ///
    /// Phase 2 treats tool schemas as opaque JSON — `elena-tools` (Phase 3)
    /// provides the structured builder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSchema>,

    /// Tool-choice policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    /// Maximum output tokens.
    pub max_tokens: u32,

    /// Reasoning budget configuration, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,

    /// Sampling temperature. Omitted from the wire when [`Self::thinking`]
    /// is set (Anthropic requires temperature=1 with thinking; omitting lets
    /// the provider default apply).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Sampling top-p. Mutually exclusive with temperature in practice;
    /// callers pick one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,

    /// Per-request options that aren't wire fields.
    #[serde(default)]
    pub options: RequestOptions,
}

/// Optional per-request knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestOptions {
    /// Query-source tag used for prompt-cache 1h TTL allowlist matching.
    ///
    /// `None` means "no allowlist match"; such requests still get 5m TTL
    /// caching by default if prompt caching is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_source: Option<String>,

    /// Whether to emit `cache_control` markers on this request.
    ///
    /// Defaults to `true`. Set `false` for fire-and-forget sub-requests that
    /// shouldn't perturb the main conversation's cache.
    #[serde(default = "default_true")]
    pub enable_prompt_caching: bool,

    /// Override the client's default retry policy for this request.
    ///
    /// `None` = use the policy the client was constructed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RetryPolicy>,

    /// Per-attempt HTTP timeout override, in milliseconds.
    ///
    /// `None` = use [`AnthropicConfig::request_timeout_ms`](elena_config::AnthropicConfig::request_timeout_ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
}

const fn default_true() -> bool {
    true
}

fn default_provider() -> String {
    "anthropic".to_owned()
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            query_source: None,
            enable_prompt_caching: true,
            retry_policy: None,
            request_timeout_ms: None,
        }
    }
}

/// Reasoning / thinking budget.
///
/// Maps 1:1 to Anthropic's `thinking` request field. When present,
/// `temperature` is omitted from the outgoing wire body.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Thinking {
    /// Model decides its own reasoning budget.
    Adaptive,
    /// Caller caps the reasoning token budget.
    Enabled {
        /// Max tokens spent on visible reasoning blocks.
        budget_tokens: u32,
    },
}

/// Tool-choice policy — mirrors the Anthropic wire enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model chooses freely (default when tools are provided).
    Auto {
        /// If true, the model cannot emit parallel `tool_use` blocks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        disable_parallel_tool_use: bool,
    },
    /// Model must use some tool (any).
    Any {
        /// If true, the model cannot emit parallel `tool_use` blocks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        disable_parallel_tool_use: bool,
    },
    /// Model must use a specific named tool.
    Tool {
        /// The tool name that must be called.
        name: String,
        /// If true, the model cannot emit parallel `tool_use` blocks.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        disable_parallel_tool_use: bool,
    },
    /// Force text output — no tool use permitted.
    None,
}

/// Opaque tool schema.
///
/// Phase 2 treats tool definitions as pre-rendered JSON — what goes in the
/// `tools` array of the outgoing request body. `elena-tools` (Phase 3) will
/// provide the structured builder and `Tool` trait that produces these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolSchema(pub Value);

impl ToolSchema {
    /// Wrap a raw JSON value as a tool schema.
    #[must_use]
    pub const fn new(value: Value) -> Self {
        Self(value)
    }

    /// Borrow the underlying JSON value.
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use elena_types::{
        BudgetLimits, ContentBlock, PermissionSet, Role, SessionId, TenantContext, TenantId,
        TenantTier, ThreadId, UserId, WorkspaceId,
    };

    use super::*;

    fn example_tenant() -> TenantContext {
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

    fn example_request() -> LlmRequest {
        LlmRequest {
            provider: "anthropic".to_owned(),
            model: ModelId::new("claude-sonnet-4-6"),
            tenant: example_tenant(),
            messages: vec![elena_types::Message {
                id: elena_types::MessageId::new(),
                thread_id: ThreadId::new(),
                tenant_id: TenantId::new(),
                role: Role::User,
                kind: elena_types::MessageKind::User,
                content: vec![ContentBlock::Text { text: "hi".into(), cache_control: None }],
                created_at: chrono::Utc::now(),
                token_count: None,
                parent_id: None,
            }],
            system: vec![ContentBlock::Text {
                text: "You are helpful.".into(),
                cache_control: None,
            }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            thinking: Some(Thinking::Enabled { budget_tokens: 2_000 }),
            temperature: None,
            top_p: None,
            stop_sequences: vec![],
            options: RequestOptions::default(),
        }
    }

    #[test]
    fn request_roundtrip_preserves_every_field() {
        let req = example_request();
        let json = serde_json::to_string(&req).unwrap();
        let back: LlmRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model.as_str(), req.model.as_str());
        assert_eq!(back.max_tokens, req.max_tokens);
        assert_eq!(back.thinking, req.thinking);
        assert_eq!(back.system.len(), 1);
        assert_eq!(back.messages.len(), 1);
    }

    #[test]
    fn thinking_wire_shape() {
        let adaptive = Thinking::Adaptive;
        assert_eq!(
            serde_json::to_value(adaptive).unwrap(),
            serde_json::json!({"type": "adaptive"})
        );
        let enabled = Thinking::Enabled { budget_tokens: 5_000 };
        assert_eq!(
            serde_json::to_value(enabled).unwrap(),
            serde_json::json!({"type": "enabled", "budget_tokens": 5_000})
        );
    }

    #[test]
    fn tool_choice_auto_default_omits_parallel_flag() {
        let choice = ToolChoice::Auto { disable_parallel_tool_use: false };
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json, serde_json::json!({"type": "auto"}));
    }

    #[test]
    fn tool_choice_tool_with_name() {
        let choice = ToolChoice::Tool { name: "search".into(), disable_parallel_tool_use: true };
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "tool",
                "name": "search",
                "disable_parallel_tool_use": true
            })
        );
    }

    #[test]
    fn tool_choice_none_variant() {
        let choice = ToolChoice::None;
        let json = serde_json::to_value(&choice).unwrap();
        assert_eq!(json, serde_json::json!({"type": "none"}));
    }

    #[test]
    fn tool_schema_is_transparent() {
        let schema = ToolSchema::new(serde_json::json!({
            "name": "weather",
            "description": "Get weather",
            "input_schema": {"type": "object"}
        }));
        let json = serde_json::to_string(&schema).unwrap();
        // Transparent serde: should be the raw object, not wrapped.
        assert!(json.starts_with('{') && json.contains("\"weather\""));
        let back: ToolSchema = serde_json::from_str(&json).unwrap();
        assert_eq!(back, schema);
    }

    #[test]
    fn options_default_enables_caching() {
        let opts = RequestOptions::default();
        assert!(opts.enable_prompt_caching, "caching should default on");
        assert!(opts.query_source.is_none());
        assert!(opts.retry_policy.is_none());
    }

    #[test]
    fn empty_system_is_omitted() {
        let mut req = example_request();
        req.system.clear();
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none(), "empty system should be skipped");
    }
}
