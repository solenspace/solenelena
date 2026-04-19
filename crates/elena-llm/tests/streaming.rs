//! End-to-end integration tests for `AnthropicClient` against wiremock.
//!
//! These tests never touch the real Anthropic API — they run a local HTTP
//! server and assert on (a) what the client sends and (b) how it handles
//! the served responses.

#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{collections::HashMap, time::Duration};

use chrono::Utc;
use elena_config::AnthropicConfig;
use elena_llm::{
    AnthropicAuth, AnthropicClient, CacheAllowlist, CachePolicy, LlmRequest, RequestOptions,
};
use elena_types::{
    BudgetLimits, ContentBlock, Message, MessageId, MessageKind, ModelId, PermissionSet, Role,
    SessionId, StreamEvent, TenantContext, TenantId, TenantTier, Terminal, ThreadId, UserId,
    WorkspaceId,
};
use futures::StreamExt;
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path},
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
        metadata: HashMap::new(),
    }
}

fn req_hello() -> LlmRequest {
    LlmRequest {
        provider: "anthropic".to_owned(),
        model: ModelId::new("claude-sonnet-4-6"),
        tenant: tenant(),
        messages: vec![Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::User,
            content: vec![ContentBlock::Text { text: "hi".into(), cache_control: None }],
            created_at: Utc::now(),
            token_count: None,
            parent_id: None,
        }],
        system: vec![ContentBlock::Text { text: "You are helpful.".into(), cache_control: None }],
        tools: vec![],
        tool_choice: None,
        max_tokens: 64,
        thinking: None,
        temperature: Some(0.5),
        top_p: None,
        stop_sequences: vec![],
        options: RequestOptions { enable_prompt_caching: true, ..RequestOptions::default() },
    }
}

fn policy() -> CachePolicy {
    CachePolicy::new(TenantTier::Pro, CacheAllowlist::default())
}

fn client_for(server: &MockServer, max_attempts: u32) -> AnthropicClient {
    let cfg = AnthropicConfig {
        api_key: SecretString::from("sk-test"),
        base_url: server.uri(),
        api_version: "2023-06-01".into(),
        request_timeout_ms: Some(5_000),
        connect_timeout_ms: 2_000,
        max_attempts,
    };
    AnthropicClient::new(&cfg, AnthropicAuth::ApiKey(cfg.api_key.clone())).unwrap()
}

fn sse_body(events: &[&str]) -> String {
    let mut out = String::new();
    for ev in events {
        out.push_str(ev);
        out.push_str("\n\n");
    }
    out
}

/// A complete happy-path SSE transcript producing "Hello".
fn happy_sse_transcript() -> String {
    sse_body(&[
        r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_01","role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":10,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}}}"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}"#,
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","inference_geo":"","iterations":[],"speed":"standard"}}"#,
        r#"event: message_stop
data: {"type":"message_stop"}"#,
    ])
}

#[tokio::test]
async fn happy_path_emits_text_delta_and_done() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(happy_sse_transcript().into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server, 3);
    let mut stream = client.stream(req_hello(), policy(), CancellationToken::new());

    let mut saw_text = false;
    let mut saw_done = false;
    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { delta } => {
                assert_eq!(delta, "Hello");
                saw_text = true;
            }
            StreamEvent::Done(Terminal::Completed) => {
                saw_done = true;
                break;
            }
            StreamEvent::Done(other) => panic!("unexpected terminal: {other:?}"),
            _ => {}
        }
    }
    assert!(saw_text, "expected TextDelta");
    assert!(saw_done, "expected Done(Completed)");
}

#[tokio::test]
async fn overload_529_retries_up_to_three_then_succeeds() {
    let server = MockServer::start().await;

    // First 3 attempts return 529; 4th returns a good stream.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529))
        .up_to_n_times(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(happy_sse_transcript().into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    // Use a short retry policy so the test is fast.
    let mut req = req_hello();
    req.options.retry_policy = Some(elena_llm::RetryPolicy {
        base_delay_ms: 10,
        max_delay_ms: 50,
        max_attempts: 8,
        jitter_ratio: 0.0,
    });

    let client = client_for(&server, 8);
    let mut stream = client.stream(req, policy(), CancellationToken::new());
    let mut terminal: Option<Terminal> = None;
    while let Some(ev) = stream.next().await {
        if let StreamEvent::Done(t) = ev {
            terminal = Some(t);
            break;
        }
    }
    assert_eq!(terminal, Some(Terminal::Completed));
}

#[tokio::test]
async fn fourth_consecutive_overload_gives_up_as_repeated_529() {
    let server = MockServer::start().await;
    // Unlimited 529s.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529))
        .mount(&server)
        .await;

    let mut req = req_hello();
    req.options.retry_policy = Some(elena_llm::RetryPolicy {
        base_delay_ms: 5,
        max_delay_ms: 20,
        max_attempts: 10,
        jitter_ratio: 0.0,
    });
    let client = client_for(&server, 10);
    let mut stream = client.stream(req, policy(), CancellationToken::new());

    let mut err_kind: Option<elena_types::LlmApiErrorKind> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::Error(elena_types::ElenaError::LlmApi(e)) => err_kind = Some(e.kind()),
            StreamEvent::Done(Terminal::ModelError { classified }) => {
                // On give-up we emit an error event followed by a Done(ModelError).
                assert_eq!(
                    err_kind,
                    Some(classified),
                    "Error event classification should match terminal",
                );
                assert_eq!(classified, elena_types::LlmApiErrorKind::Repeated529);
                return;
            }
            StreamEvent::Done(other) => panic!("unexpected terminal: {other:?}"),
            _ => {}
        }
    }
    panic!("stream ended without Done event");
}

#[tokio::test]
async fn retry_after_header_is_honored() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(happy_sse_transcript().into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut req = req_hello();
    req.options.retry_policy = Some(elena_llm::RetryPolicy {
        base_delay_ms: 5,
        max_delay_ms: 20,
        max_attempts: 3,
        jitter_ratio: 0.0,
    });

    let client = client_for(&server, 3);
    let start = std::time::Instant::now();
    let mut stream = client.stream(req, policy(), CancellationToken::new());
    while let Some(ev) = stream.next().await {
        if let StreamEvent::Done(Terminal::Completed) = ev {
            break;
        }
    }
    // Retry-after of 1s should delay at least that long.
    assert!(start.elapsed() >= Duration::from_millis(900), "elapsed {:?}", start.elapsed());
}

#[tokio::test]
async fn cancellation_mid_flight_surfaces_aborted() {
    let server = MockServer::start().await;
    // A slow response — we'll cancel before it arrives.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(10))
                .set_body_raw(b"x".to_vec(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server, 3);
    let cancel = CancellationToken::new();
    let mut stream = client.stream(req_hello(), policy(), cancel.clone());

    // Cancel after a brief delay so the request has a chance to start.
    let t = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.cancel();
    });

    let mut got_aborted = false;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::Error(elena_types::ElenaError::Aborted) => got_aborted = true,
            StreamEvent::Done(Terminal::AbortedStreaming) => {
                assert!(got_aborted, "expected Aborted error before terminal");
                return;
            }
            StreamEvent::Done(other) => panic!("unexpected terminal: {other:?}"),
            _ => {}
        }
    }
    panic!("stream ended without terminal event");
}

#[tokio::test]
async fn cache_control_marker_included_in_outgoing_body() {
    let server = MockServer::start().await;
    // Capture the request body via a wiremock matcher.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(happy_sse_transcript().into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = client_for(&server, 3);
    let mut stream = client.stream(req_hello(), policy(), CancellationToken::new());
    while let Some(ev) = stream.next().await {
        if matches!(ev, StreamEvent::Done(_)) {
            break;
        }
    }

    let received: Vec<Request> = server.received_requests().await.unwrap();
    let req = received.first().expect("got a request");
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let content = messages[0]["content"].as_array().unwrap();
    let block = &content[0];
    assert!(
        block["cache_control"].is_object(),
        "cache_control missing on last text block: {block}",
    );
    assert_eq!(block["cache_control"]["type"], "ephemeral");
    assert_eq!(block["cache_control"]["scope"], "global");

    // Beta header for caching scope.
    let beta = req.headers.get("anthropic-beta").expect("beta header");
    assert!(
        beta.to_str().unwrap().contains("prompt-caching-scope-2026-01-05"),
        "beta header content: {beta:?}",
    );
}
