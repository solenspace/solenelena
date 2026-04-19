//! Wire-body builder — turns an [`LlmRequest`] into the JSON Anthropic expects.
//!
//! Key calibrations from the reference TS source:
//! - `temperature` is *omitted* (not sent as 1.0) when `thinking` is set.
//! - `cache_control` markers go on exactly one message-level block (the last
//!   text block of the last message, via [`CachePolicy::decide_message_marker`])
//!   and optionally one system-level block (via
//!   [`CachePolicy::decide_system_marker`]).
//! - `metadata.user_id` is required and carries a JSON-encoded object with
//!   tenant/user/workspace/session IDs.

use elena_types::ContentBlock;
use serde_json::{Value, json};

use crate::{cache::CachePolicy, request::LlmRequest};

/// Assemble the Anthropic Messages API request body for the given
/// [`LlmRequest`] and [`CachePolicy`].
#[must_use]
pub fn build_wire_body(req: &LlmRequest, policy: &CachePolicy) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(req.model.as_str()));
    body.insert("max_tokens".into(), json!(req.max_tokens));
    body.insert("stream".into(), json!(true));

    // Apply cache_control markers to in-place copies of the request's
    // messages / system blocks; the original request stays immutable.
    let enable_cache = req.options.enable_prompt_caching && policy.is_enabled();
    let cache_marker = enable_cache.then(|| policy.marker_for(req.options.query_source.as_deref()));

    // Messages.
    let marker_target = enable_cache.then(|| CachePolicy::decide_message_marker(req)).flatten();
    let messages = render_messages(req, cache_marker, marker_target);
    body.insert("messages".into(), json!(messages));

    // System prompt.
    if !req.system.is_empty() {
        let sys_marker_idx = enable_cache.then(|| CachePolicy::decide_system_marker(req)).flatten();
        let system = render_system(&req.system, cache_marker, sys_marker_idx);
        body.insert("system".into(), json!(system));
    }

    // Tools.
    if !req.tools.is_empty() {
        let tools: Vec<&Value> =
            req.tools.iter().map(crate::request::ToolSchema::as_value).collect();
        body.insert("tools".into(), json!(tools));
    }
    if let Some(choice) = &req.tool_choice {
        body.insert("tool_choice".into(), serde_json::to_value(choice).unwrap_or(Value::Null));
    }

    // Thinking + temperature: mutually exclusive on the wire.
    if let Some(thinking) = req.thinking {
        body.insert("thinking".into(), serde_json::to_value(thinking).unwrap_or(Value::Null));
    } else if let Some(temp) = req.temperature {
        body.insert("temperature".into(), json!(temp));
    }

    if let Some(top_p) = req.top_p {
        body.insert("top_p".into(), json!(top_p));
    }

    if !req.stop_sequences.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop_sequences));
    }

    // Metadata: Anthropic wants `user_id` as an opaque string. We put a
    // JSON-encoded object of the relevant IDs in that string so operators
    // can correlate requests to sessions without extra headers.
    let metadata = json!({
        "user_id": build_user_id_json(req),
    });
    body.insert("metadata".into(), metadata);

    Value::Object(body)
}

/// Serialize the set of beta headers that apply to this request.
///
/// Phase 2 only: `prompt-caching-scope` is the one beta we conditionally
/// enable (when global scope is active).
#[must_use]
pub fn beta_headers(_req: &LlmRequest, policy: &CachePolicy) -> Vec<&'static str> {
    let mut betas = Vec::new();
    if policy.is_enabled() {
        // The marker's scope field is only honored with this beta header.
        betas.push("prompt-caching-scope-2026-01-05");
    }
    betas
}

fn build_user_id_json(req: &LlmRequest) -> String {
    let payload = json!({
        "tenant_id": req.tenant.tenant_id.to_string(),
        "user_id": req.tenant.user_id.to_string(),
        "workspace_id": req.tenant.workspace_id.to_string(),
        "session_id": req.tenant.session_id.to_string(),
    });
    payload.to_string()
}

fn render_messages(
    req: &LlmRequest,
    marker: Option<elena_types::CacheControl>,
    target: Option<(usize, usize)>,
) -> Vec<Value> {
    req.messages
        .iter()
        .enumerate()
        .map(|(msg_idx, msg)| {
            let mut content_json = Vec::with_capacity(msg.content.len());
            for (blk_idx, block) in msg.content.iter().enumerate() {
                let mut value = serde_json::to_value(block).unwrap_or(Value::Null);
                if let Some(m) = marker {
                    if Some((msg_idx, blk_idx)) == target {
                        attach_cache_control(&mut value, m);
                    }
                }
                content_json.push(value);
            }
            json!({
                "role": role_wire_value(msg.role),
                "content": content_json,
            })
        })
        .collect()
}

fn render_system(
    system: &[ContentBlock],
    marker: Option<elena_types::CacheControl>,
    target: Option<usize>,
) -> Vec<Value> {
    system
        .iter()
        .enumerate()
        .map(|(idx, block)| {
            let mut value = serde_json::to_value(block).unwrap_or(Value::Null);
            if let Some(m) = marker {
                if Some(idx) == target {
                    attach_cache_control(&mut value, m);
                }
            }
            value
        })
        .collect()
}

fn attach_cache_control(block: &mut Value, marker: elena_types::CacheControl) {
    if let Value::Object(map) = block {
        map.insert("cache_control".to_owned(), serde_json::to_value(marker).unwrap_or(Value::Null));
    }
}

const fn role_wire_value(role: elena_types::Role) -> &'static str {
    match role {
        elena_types::Role::Assistant => "assistant",
        // Anthropic accepts `"user"` for tool-result and system rows — there
        // are no separate wire roles for those.
        elena_types::Role::User | elena_types::Role::Tool | elena_types::Role::System => "user",
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use elena_types::{
        BudgetLimits, CacheScope, ContentBlock, Message, MessageId, MessageKind, ModelId,
        PermissionSet, Role, SessionId, TenantContext, TenantId, TenantTier, ThreadId, UserId,
        WorkspaceId,
    };

    use super::*;
    use crate::{
        cache::CacheAllowlist,
        request::{LlmRequest, RequestOptions, Thinking},
    };

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

    fn example_req() -> LlmRequest {
        LlmRequest {
            provider: "anthropic".to_owned(),
            model: ModelId::new("claude-sonnet-4-6"),
            tenant: tenant(),
            messages: vec![user_msg("What's the weather?")],
            system: vec![ContentBlock::Text { text: "Be concise.".into(), cache_control: None }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 512,
            thinking: None,
            temperature: Some(0.7),
            top_p: None,
            stop_sequences: vec![],
            options: RequestOptions::default(),
        }
    }

    fn pro_policy() -> CachePolicy {
        CachePolicy::new(TenantTier::Pro, CacheAllowlist::default())
    }

    #[test]
    fn body_contains_required_fields() {
        let body = build_wire_body(&example_req(), &pro_policy());
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["max_tokens"], 512);
        assert_eq!(body["stream"], true);
        assert!(body["messages"].is_array());
        assert!(body["system"].is_array());
        assert!(body["metadata"]["user_id"].is_string());
    }

    #[test]
    fn temperature_omitted_when_thinking_set() {
        let mut req = example_req();
        req.thinking = Some(Thinking::Enabled { budget_tokens: 1_000 });
        req.temperature = Some(0.7); // should be dropped
        let body = build_wire_body(&req, &pro_policy());
        assert!(body.get("temperature").is_none());
        assert!(body["thinking"].is_object());
    }

    #[test]
    fn temperature_sent_when_no_thinking() {
        let body = build_wire_body(&example_req(), &pro_policy());
        // f32 → JSON precision: 0.7f32 renders as 0.6999... so compare with
        // a tolerance rather than exact equality.
        let t = body["temperature"].as_f64().expect("temperature is a number");
        assert!((t - 0.7).abs() < 1e-3, "got temperature {t}");
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn cache_control_placed_on_last_text_block() {
        let body = build_wire_body(&example_req(), &pro_policy());
        let last_block = &body["messages"][0]["content"][0];
        assert!(
            last_block["cache_control"].is_object(),
            "cache_control missing on last text block: {last_block}"
        );
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        assert_eq!(last_block["cache_control"]["scope"], "global");
    }

    #[test]
    fn cache_control_placed_on_last_system_block() {
        let body = build_wire_body(&example_req(), &pro_policy());
        let system = &body["system"][0];
        assert!(system["cache_control"].is_object());
    }

    #[test]
    fn cache_disabled_when_options_say_so() {
        let mut req = example_req();
        req.options.enable_prompt_caching = false;
        let body = build_wire_body(&req, &pro_policy());
        let block = &body["messages"][0]["content"][0];
        assert!(block.get("cache_control").is_none(), "no marker expected: {block}");
        let system = &body["system"][0];
        assert!(system.get("cache_control").is_none());
    }

    #[test]
    fn ttl_applied_when_query_source_matches() {
        let mut req = example_req();
        req.options.query_source = Some("sdk".into());
        let policy = CachePolicy::new(TenantTier::Pro, CacheAllowlist::new(["sdk"]));
        let body = build_wire_body(&req, &policy);
        let cc = &body["messages"][0]["content"][0]["cache_control"];
        assert_eq!(cc["ttl"], "1h");
    }

    #[test]
    fn org_scope_when_policy_downgraded() {
        let mut req = example_req();
        req.options.enable_prompt_caching = true;
        let policy =
            CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()).without_global_scope();
        let body = build_wire_body(&req, &policy);
        let cc = &body["messages"][0]["content"][0]["cache_control"];
        assert_eq!(cc["scope"], "org");
        // Beta header only fires for global scope.
        let _ = CacheScope::Org;
    }

    #[test]
    fn beta_headers_include_cache_scope() {
        let betas = beta_headers(&example_req(), &pro_policy());
        assert!(betas.contains(&"prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn metadata_user_id_encodes_tenant_ids() {
        let body = build_wire_body(&example_req(), &pro_policy());
        let raw = body["metadata"]["user_id"].as_str().expect("user_id string");
        let parsed: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert!(parsed["tenant_id"].is_string());
        assert!(parsed["user_id"].is_string());
        assert!(parsed["session_id"].is_string());
    }

    #[test]
    fn no_system_field_when_none_supplied() {
        let mut req = example_req();
        req.system.clear();
        let body = build_wire_body(&req, &pro_policy());
        assert!(body.get("system").is_none());
    }

    #[test]
    fn stop_sequences_roundtrip_when_present() {
        let mut req = example_req();
        req.stop_sequences = vec!["<END>".into()];
        let body = build_wire_body(&req, &pro_policy());
        assert_eq!(body["stop_sequences"][0], "<END>");
    }
}
