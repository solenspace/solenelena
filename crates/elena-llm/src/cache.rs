//! Prompt-cache decision + marker placement.
//!
//! Elena cache policy, a simplified port of the reference TS source's
//! layered logic:
//!
//! - **TTL eligibility** is latched at construction. Tenant tier drives it
//!   (paid tiers get 1h TTL; Free stays at default 5m). The operator's
//!   allowlist filters further — only matching query sources receive the
//!   1h marker.
//! - **Scope** defaults to `Global` (Anthropic-direct, no MCP tools for
//!   now). Operators can opt out via [`CachePolicy::without_global_scope`].
//! - **Marker placement** is exactly one message-level marker (on the last
//!   text block of the last message) plus one system-level marker (on the
//!   final system block).
//!
//! The reference TS source's cross-tenant isolation logic (scope
//! downgrade when per-user MCP tools are present) lives here as a
//! documented hook but is not exercised until MCP-scoped caching is
//! wired in.

use std::sync::Arc;

use elena_config::CacheConfig;
use elena_types::{CacheControl, CacheScope, CacheTtl, ContentBlock, TenantTier};

use crate::request::LlmRequest;

/// Operator-supplied allowlist of query-source patterns eligible for the
/// 1-hour TTL variant of Anthropic's ephemeral cache.
///
/// Patterns support a trailing `*` for prefix matching; otherwise they're
/// compared exactly. An empty allowlist means nothing qualifies for 1h TTL
/// (default behavior).
///
/// Not `Serialize`/`Deserialize` — it's operator-local config, not wire
/// data. Load via [`CacheAllowlist::from`] on a deserialized [`CacheConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheAllowlist {
    patterns: Arc<[String]>,
}

impl CacheAllowlist {
    /// Build from an iterator of pattern strings.
    pub fn new<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let patterns: Vec<String> = patterns.into_iter().map(Into::into).collect();
        Self { patterns: Arc::from(patterns) }
    }

    /// True if the given query source matches at least one allowlist pattern.
    #[must_use]
    pub fn matches(&self, query_source: &str) -> bool {
        self.patterns.iter().any(|pat| match pat.strip_suffix('*') {
            Some(prefix) => query_source.starts_with(prefix),
            None => pat == query_source,
        })
    }
}

impl From<CacheConfig> for CacheAllowlist {
    fn from(cfg: CacheConfig) -> Self {
        Self::new(cfg.allowlist)
    }
}

/// Latched cache policy — computed once per session/tenant and reused.
///
/// Mid-session state changes that would affect eligibility (e.g., a tenant
/// tier upgrade) do **not** change the policy — Elena requires a new session
/// to pick up the change. This matches the reference TS source's latching
/// behavior and prevents a flicker that would otherwise bust every cached
/// prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePolicy {
    ttl_eligible: bool,
    allowlist: CacheAllowlist,
    scope: CacheScope,
}

impl CachePolicy {
    /// Build a policy from tenant tier + operator allowlist.
    ///
    /// Uses the global scope by default (Elena speaks directly to
    /// Anthropic with no MCP tools in play). MCP-aware scope downgrade
    /// is not yet wired in.
    #[must_use]
    pub fn new(tier: TenantTier, allowlist: CacheAllowlist) -> Self {
        let ttl_eligible =
            matches!(tier, TenantTier::Pro | TenantTier::Team | TenantTier::Enterprise);
        Self { ttl_eligible, allowlist, scope: CacheScope::Global }
    }

    /// Force `org`-level scope (used by operators who don't want global
    /// cache sharing, or tests that need deterministic markers).
    #[must_use]
    pub const fn without_global_scope(mut self) -> Self {
        self.scope = CacheScope::Org;
        self
    }

    /// Whether this policy can emit any cache markers at all.
    ///
    /// Always `true` today — the 5m ephemeral TTL is the fallback for
    /// tiers below Pro. Kept as a method so callers can short-circuit
    /// marker placement when feature flags disable caching entirely.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        true
    }

    /// Compute the TTL to apply to markers on this request.
    ///
    /// Returns:
    /// - `Some(CacheTtl::OneHour)` if the tenant tier is eligible AND the
    ///   query source matches an allowlist pattern.
    /// - `None` otherwise — callers omit the `ttl` field from the wire
    ///   marker, letting Anthropic apply the default 5-minute ephemeral TTL.
    #[must_use]
    pub fn ttl_for(&self, query_source: Option<&str>) -> Option<CacheTtl> {
        if !self.ttl_eligible {
            return None;
        }
        let qs = query_source?;
        if self.allowlist.matches(qs) { Some(CacheTtl::OneHour) } else { None }
    }

    /// Build the [`CacheControl`] value for a marker on a request.
    #[must_use]
    pub fn marker_for(&self, query_source: Option<&str>) -> CacheControl {
        let mut cc = CacheControl::ephemeral();
        if let Some(ttl) = self.ttl_for(query_source) {
            cc = cc.with_ttl(ttl);
        }
        cc.with_scope(self.scope)
    }

    /// Decide where to place the single message-level cache marker.
    ///
    /// Returns `Some((message_index, block_index))` pointing to the last
    /// *text* block of the last message, or `None` if no suitable block
    /// exists (e.g., the last message is entirely `tool_result` or image).
    ///
    /// The reference TS source's rule is: exactly one message-level marker
    /// per request. Markers on non-text blocks are tolerated by Anthropic
    /// but produce worse cache locality, so we target text.
    #[must_use]
    pub fn decide_message_marker(req: &LlmRequest) -> Option<(usize, usize)> {
        if req.messages.is_empty() {
            return None;
        }
        let last_idx = req.messages.len() - 1;
        let last = &req.messages[last_idx];
        // Walk content blocks in reverse to find the last text block.
        last.content
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, b)| matches!(b, ContentBlock::Text { .. }).then_some(i))
            .map(|i| (last_idx, i))
    }

    /// Decide the index of the system block that should carry the system-
    /// level cache marker, if any.
    ///
    /// The last system block gets the marker. Splitting static vs.
    /// dynamic system-prompt blocks (and marking only the static
    /// prefix) is not yet supported.
    #[must_use]
    pub fn decide_system_marker(req: &LlmRequest) -> Option<usize> {
        if req.system.is_empty() { None } else { Some(req.system.len() - 1) }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use elena_types::{
        BudgetLimits, ContentBlock, Message, MessageId, MessageKind, ModelId, PermissionSet, Role,
        SessionId, TenantContext, TenantId, TenantTier, ThreadId, UserId, WorkspaceId,
    };

    use super::*;
    use crate::request::{LlmRequest, RequestOptions};

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
            plan: None,
            metadata: std::collections::HashMap::new(),
        }
    }

    fn req_with_messages(messages: Vec<Message>) -> LlmRequest {
        LlmRequest {
            provider: "anthropic".to_owned(),
            model: ModelId::new("claude-sonnet-4-6"),
            tenant: tenant(),
            messages,
            system: vec![],
            tools: vec![],
            tool_choice: None,
            max_tokens: 1024,
            thinking: None,
            temperature: Some(1.0),
            top_p: None,
            stop_sequences: vec![],
            options: RequestOptions::default(),
        }
    }

    fn message_with_content(content: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::User,
            content,
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }
    }

    #[test]
    fn allowlist_prefix_match_with_star() {
        let al = CacheAllowlist::new(["repl_main_thread*", "sdk"]);
        assert!(al.matches("repl_main_thread_42"));
        assert!(al.matches("repl_main_thread"));
        assert!(al.matches("sdk"));
        assert!(!al.matches("sdk_extra"));
        assert!(!al.matches("other"));
    }

    #[test]
    fn allowlist_empty_matches_nothing() {
        let al = CacheAllowlist::default();
        assert!(!al.matches("anything"));
    }

    #[test]
    fn ttl_is_none_for_free_tier() {
        let p = CachePolicy::new(TenantTier::Free, CacheAllowlist::new(["sdk"]));
        assert_eq!(p.ttl_for(Some("sdk")), None);
    }

    #[test]
    fn ttl_needs_both_tier_and_allowlist() {
        let p = CachePolicy::new(TenantTier::Pro, CacheAllowlist::new(["sdk"]));
        assert_eq!(p.ttl_for(None), None, "no query source = no 1h");
        assert_eq!(p.ttl_for(Some("sdk")), Some(CacheTtl::OneHour));
        assert_eq!(p.ttl_for(Some("other")), None);
    }

    #[test]
    fn ttl_enterprise_also_eligible() {
        let p = CachePolicy::new(TenantTier::Enterprise, CacheAllowlist::new(["*"]));
        assert_eq!(p.ttl_for(Some("anything")), Some(CacheTtl::OneHour));
    }

    #[test]
    fn marker_always_ephemeral_with_scope() {
        let p = CachePolicy::new(TenantTier::Free, CacheAllowlist::default());
        let cc = p.marker_for(None);
        assert_eq!(cc.kind, elena_types::CacheControlKind::Ephemeral);
        assert_eq!(cc.scope, Some(CacheScope::Global));
        assert_eq!(cc.ttl, None);
    }

    #[test]
    fn without_global_scope_falls_back_to_org() {
        let p = CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()).without_global_scope();
        let cc = p.marker_for(None);
        assert_eq!(cc.scope, Some(CacheScope::Org));
    }

    #[test]
    fn message_marker_points_to_last_text_block_of_last_message() {
        let last_msg = message_with_content(vec![
            ContentBlock::ToolResult {
                tool_use_id: elena_types::ToolCallId::new(),
                content: elena_types::ToolResultContent::text("ok"),
                is_error: false,
                cache_control: None,
            },
            ContentBlock::Text { text: "marker here".into(), cache_control: None },
            ContentBlock::ToolResult {
                tool_use_id: elena_types::ToolCallId::new(),
                content: elena_types::ToolResultContent::text("more"),
                is_error: false,
                cache_control: None,
            },
        ]);
        let req = req_with_messages(vec![
            message_with_content(vec![ContentBlock::Text {
                text: "ignored".into(),
                cache_control: None,
            }]),
            last_msg,
        ]);
        // The last text block is at position 1 of the last message (index 1).
        assert_eq!(CachePolicy::decide_message_marker(&req), Some((1, 1)));
    }

    #[test]
    fn message_marker_none_when_no_text_block_in_last_message() {
        let last_msg = message_with_content(vec![ContentBlock::ToolResult {
            tool_use_id: elena_types::ToolCallId::new(),
            content: elena_types::ToolResultContent::text("ok"),
            is_error: false,
            cache_control: None,
        }]);
        let req = req_with_messages(vec![last_msg]);
        assert_eq!(CachePolicy::decide_message_marker(&req), None);
    }

    #[test]
    fn message_marker_none_on_empty_messages() {
        let req = req_with_messages(vec![]);
        assert_eq!(CachePolicy::decide_message_marker(&req), None);
    }

    #[test]
    fn system_marker_on_last_system_block() {
        let mut req = req_with_messages(vec![]);
        req.system = vec![
            ContentBlock::Text { text: "a".into(), cache_control: None },
            ContentBlock::Text { text: "b".into(), cache_control: None },
        ];
        assert_eq!(CachePolicy::decide_system_marker(&req), Some(1));
    }

    #[test]
    fn system_marker_none_on_empty_system() {
        let req = req_with_messages(vec![]);
        assert_eq!(CachePolicy::decide_system_marker(&req), None);
    }

    #[test]
    fn cache_config_converts_to_allowlist() {
        let cfg = CacheConfig { allowlist: vec!["repl_main_thread*".into()] };
        let al: CacheAllowlist = cfg.into();
        assert!(al.matches("repl_main_thread_1"));
    }
}
