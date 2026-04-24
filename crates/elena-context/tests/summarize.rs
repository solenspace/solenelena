//! Summarizer test — verifies the Anthropic call shape and text extraction
//! against a wiremock server. No Postgres, no real embedding model.

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use elena_config::AnthropicConfig;
use elena_context::Summarizer;
use elena_llm::{AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy};
use elena_types::{
    BudgetLimits, Message, ModelId, PermissionSet, SessionId, TenantContext, TenantId, TenantTier,
    ThreadId, UserId, WorkspaceId,
};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tenant() -> TenantContext {
    TenantContext {
        tenant_id: TenantId::new(),
        user_id: UserId::new(),
        workspace_id: WorkspaceId::new(),
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        permissions: PermissionSet::default(),
        budget: BudgetLimits::DEFAULT_PRO,
        tier: TenantTier::Pro,
        plan: None,
        metadata: std::collections::HashMap::new(),
    }
}

fn usage_json(input: u64, output: u64) -> String {
    format!(
        r#"{{"input_tokens":{input},"output_tokens":{output},"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0}},"server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}}"#
    )
}

fn mock_sse_body(text: &str) -> String {
    let u = usage_json(20, 0);
    let u2 = usage_json(20, 8);
    [
        format!(
            r#"event: message_start
data: {{"type":"message_start","message":{{"id":"msg_sum","role":"assistant","model":"claude-haiku-4-5-20251001","usage":{u}}}}}

"#
        ),
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#
        .to_owned(),
        format!(
            r#"event: content_block_delta
data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}

"#
        ),
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#
        .to_owned(),
        format!(
            r#"event: message_delta
data: {{"type":"message_delta","delta":{{"stop_reason":"end_turn"}},"usage":{u2}}}

"#
        ),
        r#"event: message_stop
data: {"type":"message_stop"}

"#
        .to_owned(),
    ]
    .join("")
}

#[tokio::test]
async fn summarize_returns_concatenated_text_deltas() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    mock_sse_body("Customer wanted to reverse a string.").into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&server)
        .await;

    let cfg = AnthropicConfig {
        api_key: SecretString::from("sk-test"),
        base_url: server.uri(),
        api_version: "2023-06-01".into(),
        request_timeout_ms: Some(5_000),
        connect_timeout_ms: 2_000,
        max_attempts: 2,
        pool_max_idle_per_host: 64,
    };
    let client = AnthropicClient::new(&cfg, AnthropicAuth::ApiKey(cfg.api_key.clone())).unwrap();
    let sum = Summarizer::new(
        Arc::new(client),
        CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        ModelId::new("claude-haiku-4-5-20251001"),
        256,
    );

    let history: Vec<Message> = (0..3)
        .map(|i| Message::user_text(TenantId::new(), ThreadId::new(), format!("msg {i}")))
        .collect();
    let out = sum.summarize(&tenant(), &history, CancellationToken::new()).await.unwrap();
    assert_eq!(out, "Customer wanted to reverse a string.");
}

#[tokio::test]
async fn summarize_empty_history_still_issues_one_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    mock_sse_body("Empty conversation; nothing happened.").into_bytes(),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let cfg = AnthropicConfig {
        api_key: SecretString::from("sk-test"),
        base_url: server.uri(),
        api_version: "2023-06-01".into(),
        request_timeout_ms: Some(5_000),
        connect_timeout_ms: 2_000,
        max_attempts: 2,
        pool_max_idle_per_host: 64,
    };
    let client = AnthropicClient::new(&cfg, AnthropicAuth::ApiKey(cfg.api_key.clone())).unwrap();
    let sum = Summarizer::new(
        Arc::new(client),
        CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        ModelId::new("claude-haiku-4-5-20251001"),
        128,
    );
    let out = sum.summarize(&tenant(), &[], CancellationToken::new()).await.unwrap();
    assert!(out.starts_with("Empty conversation"));
}
