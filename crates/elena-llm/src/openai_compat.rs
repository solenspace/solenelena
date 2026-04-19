// This module's docs mention a lot of proper nouns (OpenAI, OpenRouter,
// Groq, DeepSeek). Backtick-wrapping each one adds no value — allow the
// lint module-wide.
#![allow(clippy::doc_markdown)]

//! [`OpenAiCompatClient`] — streaming client for OpenAI-compatible
//! chat-completions APIs (OpenRouter, Groq, Together, DeepSeek, etc.).
//!
//! This client lets Elena talk to any provider whose API mirrors OpenAI's
//! `/chat/completions` SSE surface. It translates:
//!
//! - Elena's [`LlmRequest`] → OpenAI chat-completions body
//! - OpenAI tool-schema `{type:"function", function:{...}}` ↔ Elena's
//!   Anthropic-shaped `{name, description, input_schema}`
//! - OpenAI streaming deltas → Elena's [`StreamEvent`]s (including
//!   `tool_calls` → `ToolUseStart`/`ToolUseInputDelta`/`ToolUseComplete`)
//! - OpenAI `usage` → Elena's [`elena_types::Usage`]
//!
//! Prompt caching is a no-op on this client (OpenAI-compat providers
//! don't expose Anthropic's `cache_control`). [`CachePolicy`] is accepted
//! for API symmetry but ignored.
//!
//! The client reuses [`SseExtractor`] and [`decide_retry`] from the
//! Anthropic path so retry/back-off behaviour is uniform across providers.

use std::sync::Arc;
use std::time::Duration;

use elena_types::{
    ContentBlock, ElenaError, LlmApiError, Message, MessageId, MessageKind, StreamEvent, Terminal,
    ToolCallId, ToolResultContent, Usage,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{
    cache::CachePolicy,
    provider::LlmClient,
    request::{LlmRequest, ToolChoice},
    retry::{RetryDecision, RetryPolicy, classify_http, decide_retry},
    sse::SseExtractor,
};

/// Operator-facing config for one OpenAI-compatible provider.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiCompatConfig {
    /// API key sent as `Authorization: Bearer ...`.
    pub api_key: SecretString,
    /// Base URL. Must point at the provider's chat-completions root
    /// (e.g. `https://openrouter.ai/api/v1`). Client appends
    /// `/chat/completions`.
    pub base_url: String,
    /// Per-attempt HTTP timeout (ms). `None` means no timeout.
    #[serde(default)]
    pub request_timeout_ms: Option<u64>,
    /// Connection-establishment timeout (ms).
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// Maximum retry attempts for transient errors.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

const fn default_connect_timeout_ms() -> u64 {
    10_000
}
const fn default_max_attempts() -> u32 {
    3
}

/// Cheap-to-clone handle to an OpenAI-compatible provider.
#[derive(Clone)]
pub struct OpenAiCompatClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for OpenAiCompatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatClient")
            .field("name", &self.inner.name)
            .field("base_url", &self.inner.base_url)
            .finish_non_exhaustive()
    }
}

struct Inner {
    name: String,
    http: Client,
    base_url: String,
    api_key: SecretString,
    retry_policy: RetryPolicy,
}

impl OpenAiCompatClient {
    /// Build a client.
    ///
    /// `name` is the provider key used in
    /// [`LlmRequest::provider`](crate::LlmRequest::provider) — typically
    /// `"openrouter"` or `"groq"`. Two instances of this client can coexist
    /// under different names inside a [`LlmMultiplexer`](crate::LlmMultiplexer).
    pub fn new(name: impl Into<String>, cfg: &OpenAiCompatConfig) -> Result<Self, LlmApiError> {
        let mut builder = Client::builder()
            .user_agent(concat!("elena-llm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
            .pool_max_idle_per_host(8);
        if let Some(ms) = cfg.request_timeout_ms {
            builder = builder.timeout(Duration::from_millis(ms));
        }
        let http =
            builder.build().map_err(|e| LlmApiError::ConnectionError { message: e.to_string() })?;

        Ok(Self {
            inner: Arc::new(Inner {
                name: name.into(),
                http,
                base_url: cfg.base_url.clone(),
                api_key: cfg.api_key.clone(),
                retry_policy: RetryPolicy {
                    max_attempts: cfg.max_attempts,
                    ..RetryPolicy::DEFAULT
                },
            }),
        })
    }

    /// Stream a completion.
    #[must_use]
    pub fn stream(
        &self,
        req: LlmRequest,
        _policy: CachePolicy,
        cancel: CancellationToken,
    ) -> BoxStream<'static, StreamEvent> {
        let (tx, rx) = mpsc::channel::<StreamEvent>(64);
        let this = self.clone();
        tokio::spawn(async move {
            this.run_stream(req, cancel, tx).await;
        });
        ReceiverStream::new(rx).boxed()
    }

    async fn run_stream(
        &self,
        req: LlmRequest,
        cancel: CancellationToken,
        tx: mpsc::Sender<StreamEvent>,
    ) {
        let retry_policy = req.options.retry_policy.unwrap_or(self.inner.retry_policy);
        let mut attempt: u32 = 0;
        let mut consecutive_529s: u32 = 0;

        loop {
            attempt += 1;
            let outcome = self.execute_once(&req, cancel.clone(), tx.clone()).await;
            match outcome {
                Outcome::Completed => return,
                Outcome::Cancelled => {
                    let _ = tx.send(StreamEvent::Error(ElenaError::Aborted)).await;
                    let _ = tx.send(StreamEvent::Done(Terminal::AbortedStreaming)).await;
                    return;
                }
                Outcome::Error(err) => {
                    if matches!(err, LlmApiError::ServerOverload) {
                        consecutive_529s += 1;
                    } else {
                        consecutive_529s = 0;
                    }
                    match decide_retry(&err, attempt, consecutive_529s, &retry_policy) {
                        RetryDecision::Retry { delay } => {
                            tokio::select! {
                                () = tokio::time::sleep(delay) => {}
                                () = cancel.cancelled() => {
                                    let _ = tx.send(StreamEvent::Error(ElenaError::Aborted)).await;
                                    let _ = tx.send(StreamEvent::Done(Terminal::AbortedStreaming)).await;
                                    return;
                                }
                            }
                        }
                        RetryDecision::Fail(final_err) => {
                            let kind = final_err.kind();
                            let _ =
                                tx.send(StreamEvent::Error(ElenaError::LlmApi(final_err))).await;
                            let _ = tx
                                .send(StreamEvent::Done(Terminal::ModelError { classified: kind }))
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn execute_once(
        &self,
        req: &LlmRequest,
        cancel: CancellationToken,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Outcome {
        if cancel.is_cancelled() {
            return Outcome::Cancelled;
        }

        let body = build_body(req);

        let mut headers = HeaderMap::new();
        let auth_value = format!("Bearer {}", self.inner.api_key.expose_secret());
        match HeaderValue::from_str(&auth_value) {
            Ok(v) => {
                headers.insert(HeaderName::from_static("authorization"), v);
            }
            Err(_) => return Outcome::Error(LlmApiError::InvalidApiKey),
        }
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );

        let url = format!("{}/chat/completions", self.inner.base_url.trim_end_matches('/'));
        let send = self.inner.http.post(&url).headers(headers).json(&body).send();

        let response = tokio::select! {
            res = send => res,
            () = cancel.cancelled() => return Outcome::Cancelled,
        };
        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return Outcome::Error(LlmApiError::ConnectionError { message: e.to_string() });
            }
        };

        let status = response.status();
        if !status.is_success() {
            let hdrs = response.headers().clone();
            let body_snip = response.text().await.unwrap_or_default();
            return Outcome::Error(classify_http(status, &hdrs, Some(&body_snip)));
        }

        self.pump_stream(response, cancel, tx).await
    }

    async fn pump_stream(
        &self,
        response: reqwest::Response,
        cancel: CancellationToken,
        tx: mpsc::Sender<StreamEvent>,
    ) -> Outcome {
        let mut extractor = SseExtractor::new();
        let mut assembler = OpenAiAssembler::new();
        let mut body = response.bytes_stream();

        while let Some(chunk) = tokio::select! {
            next = body.next() => next,
            () = cancel.cancelled() => return Outcome::Cancelled,
        } {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    return Outcome::Error(LlmApiError::ConnectionError { message: e.to_string() });
                }
            };

            for frame in extractor.push(&bytes) {
                if frame.data.is_empty() || frame.data == "[DONE]" {
                    continue;
                }
                let chunk: StreamChunk = match serde_json::from_str(&frame.data) {
                    Ok(c) => c,
                    Err(err) => {
                        warn!(?err, data = %frame.data, "skipping unparseable OpenAI SSE frame");
                        continue;
                    }
                };
                for ev in assembler.handle(chunk) {
                    if tx.send(ev).await.is_err() {
                        return Outcome::Cancelled;
                    }
                }
                if assembler.finished {
                    let usage = assembler.usage.clone();
                    let _ = tx.send(StreamEvent::UsageUpdate(usage)).await;
                    let _ = tx.send(StreamEvent::Done(Terminal::Completed)).await;
                    return Outcome::Completed;
                }
            }
        }

        // Stream ended without a [DONE] — treat as partial.
        let usage = assembler.usage.clone();
        let _ = tx.send(StreamEvent::UsageUpdate(usage)).await;
        let _ = tx.send(StreamEvent::Done(Terminal::Completed)).await;
        Outcome::Completed
    }
}

impl LlmClient for OpenAiCompatClient {
    fn provider(&self) -> &str {
        &self.inner.name
    }

    fn stream(
        &self,
        req: LlmRequest,
        policy: CachePolicy,
        cancel: CancellationToken,
    ) -> BoxStream<'static, StreamEvent> {
        Self::stream(self, req, policy, cancel)
    }
}

enum Outcome {
    Completed,
    Cancelled,
    Error(LlmApiError),
}

// ============================================================================
// Streaming chunk shape (incoming wire)
// ============================================================================

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<ChoiceDelta>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChoiceDelta {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    index: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    /// Some providers (DeepSeek, OpenRouter pass-through) expose visible
    /// reasoning here.
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    /// 0-indexed slot — the same slot is referenced across partial chunks.
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    ty: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    #[allow(dead_code)]
    total_tokens: u64,
}

// ============================================================================
// Assembler — one per request. Produces Elena's StreamEvent stream.
// ============================================================================

struct OpenAiAssembler {
    emitted_start: bool,
    message_id: MessageId,
    text_buf: String,
    /// Per-slot tool-call accumulator (slot matches
    /// `choices[0].delta.tool_calls[].index`).
    tool_slots: Vec<ToolSlot>,
    usage: Usage,
    finished: bool,
}

struct ToolSlot {
    /// Our ULID — independent of OpenAI's returned id, so the rest of
    /// Elena sees consistent [`ToolCallId`]s.
    id: ToolCallId,
    name: String,
    args_buf: String,
    started: bool,
    completed: bool,
}

impl OpenAiAssembler {
    fn new() -> Self {
        Self {
            emitted_start: false,
            message_id: MessageId::new(),
            text_buf: String::new(),
            tool_slots: Vec::new(),
            usage: Usage::default(),
            finished: false,
        }
    }

    fn handle(&mut self, chunk: StreamChunk) -> Vec<StreamEvent> {
        let mut out: Vec<StreamEvent> = Vec::new();

        if let Some(usage) = chunk.usage {
            self.usage.input_tokens = usage.prompt_tokens;
            self.usage.output_tokens = usage.completion_tokens;
        }

        if !self.emitted_start && !chunk.choices.is_empty() {
            self.emitted_start = true;
            out.push(StreamEvent::MessageStart { message_id: self.message_id });
        }

        for choice in chunk.choices {
            // Text content.
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                self.text_buf.push_str(&text);
                out.push(StreamEvent::TextDelta { delta: text });
            }

            // Reasoning content (OpenRouter / DeepSeek).
            if let Some(thinking) = choice.delta.reasoning
                && !thinking.is_empty()
            {
                out.push(StreamEvent::ThinkingDelta { delta: thinking });
            }

            // Tool-call deltas — per slot.
            for tc in choice.delta.tool_calls {
                while self.tool_slots.len() <= tc.index as usize {
                    self.tool_slots.push(ToolSlot {
                        id: ToolCallId::new(),
                        name: String::new(),
                        args_buf: String::new(),
                        started: false,
                        completed: false,
                    });
                }
                let slot = &mut self.tool_slots[tc.index as usize];

                // OpenAI's id and the tool name arrive in the first chunk
                // for the slot. We ignore OpenAI's id and use our own ULID
                // throughout Elena.
                let _ = tc.id;

                if let Some(func) = tc.function {
                    if let Some(name) = func.name
                        && !name.is_empty()
                    {
                        slot.name = name;
                    }
                    if let Some(args) = func.arguments
                        && !args.is_empty()
                    {
                        slot.args_buf.push_str(&args);
                        if !slot.started && !slot.name.is_empty() {
                            slot.started = true;
                            out.push(StreamEvent::ToolUseStart {
                                id: slot.id,
                                name: slot.name.clone(),
                            });
                        }
                        if slot.started {
                            out.push(StreamEvent::ToolUseInputDelta {
                                id: slot.id,
                                partial_json: args,
                            });
                        }
                    } else if !slot.started && !slot.name.is_empty() {
                        // Emit Start even before arguments arrive — the
                        // name came in an earlier chunk than the args.
                        slot.started = true;
                        out.push(StreamEvent::ToolUseStart {
                            id: slot.id,
                            name: slot.name.clone(),
                        });
                    }
                }
            }

            // Finish reason — terminates the response.
            if let Some(reason) = choice.finish_reason {
                // Close out any still-open tool calls.
                for slot in &mut self.tool_slots {
                    if slot.started && !slot.completed {
                        slot.completed = true;
                        let parsed: Value = serde_json::from_str(&slot.args_buf)
                            .unwrap_or_else(|_| Value::Object(serde_json::Map::new()));
                        out.push(StreamEvent::ToolUseComplete {
                            id: slot.id,
                            name: slot.name.clone(),
                            input: parsed,
                        });
                    }
                }

                // Any finish reason terminates the stream. We don't
                // distinguish `stop` / `length` / `tool_calls` downstream.
                self.finished = true;
                let _ = reason;
            }
        }

        out
    }
}

// ============================================================================
// Request body builder — LlmRequest → OpenAI chat-completions body
// ============================================================================

fn build_body(req: &LlmRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    // System: join all system content blocks into one `system` message.
    if !req.system.is_empty() {
        let text = blocks_to_plain(&req.system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    for m in &req.messages {
        messages.extend(message_to_openai(m));
    }

    let mut body = json!({
        "model": req.model.as_str(),
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": req.max_tokens,
        "messages": messages,
    });

    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = req.top_p {
        body["top_p"] = json!(p);
    }
    if !req.stop_sequences.is_empty() {
        body["stop"] = json!(req.stop_sequences);
    }

    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools.iter().map(|t| anthropic_tool_to_openai(t.as_value())).collect(),
        );
        if let Some(choice) = &req.tool_choice {
            body["tool_choice"] = tool_choice_to_openai(choice);
        }
    }

    body
}

/// Translate an Anthropic-shaped tool schema `{name, description, input_schema}`
/// into OpenAI's `{type:"function", function:{name, description, parameters}}`.
fn anthropic_tool_to_openai(schema: &Value) -> Value {
    let name = schema.get("name").cloned().unwrap_or(Value::String(String::new()));
    let description = schema.get("description").cloned().unwrap_or(Value::String(String::new()));
    let parameters =
        schema.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object"}));
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

fn tool_choice_to_openai(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto { .. } => Value::String("auto".into()),
        ToolChoice::Any { .. } => Value::String("required".into()),
        ToolChoice::Tool { name, .. } => json!({
            "type": "function",
            "function": {"name": name}
        }),
        ToolChoice::None => Value::String("none".into()),
    }
}

/// Translate one Elena [`Message`] into zero or more OpenAI message objects.
///
/// Assistant turns with `tool_use` blocks become one `role:"assistant"` with
/// `tool_calls`. User turns that carry `ContentBlock::ToolResult` blocks
/// expand into one `role:"tool"` message per tool-result block (plus an
/// optional trailing `role:"user"` for any plain text), matching OpenAI's
/// convention that each tool result is its own message row.
fn message_to_openai(m: &Message) -> Vec<Value> {
    match &m.kind {
        MessageKind::User => {
            let mut out: Vec<Value> = Vec::new();
            let mut text_parts: Vec<String> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::ToolResult { tool_use_id, content, is_error, .. } => {
                        let text = match content {
                            ToolResultContent::Text(t) => t.clone(),
                            ToolResultContent::Blocks(blocks) => blocks_to_plain(blocks),
                        };
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id.to_string(),
                            "content": if *is_error { format!("ERROR: {text}") } else { text },
                        }));
                    }
                    ContentBlock::Text { text, .. } => text_parts.push(text.clone()),
                    _ => {}
                }
            }
            if !text_parts.is_empty() {
                out.push(json!({"role": "user", "content": text_parts.join("\n")}));
            } else if out.is_empty() {
                // Fully empty user message — still emit an empty row so
                // conversation alignment survives.
                out.push(json!({"role": "user", "content": ""}));
            }
            out
        }
        MessageKind::Assistant { .. } => assistant_to_openai(&m.content),
        MessageKind::Attachment | MessageKind::ToolUseSummary | MessageKind::Tombstone { .. } => {
            // These don't participate in the conversation sent to the model.
            Vec::new()
        }
        MessageKind::System(_) => {
            let text = blocks_to_plain(&m.content);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![json!({"role": "system", "content": text})]
            }
        }
    }
}

fn assistant_to_openai(blocks: &[ContentBlock]) -> Vec<Value> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text, .. } => text_parts.push(text.clone()),
            ContentBlock::ToolUse { id, name, input, .. } => {
                tool_calls.push(json!({
                    "id": id.to_string(),
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_default(),
                    }
                }));
            }
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::ToolResult { .. } => {
                // Unsupported in OpenAI assistant messages — skip.
            }
        }
    }

    let mut msg = serde_json::Map::new();
    msg.insert("role".into(), Value::String("assistant".into()));
    let text = text_parts.join("\n");
    if !text.is_empty() {
        msg.insert("content".into(), Value::String(text));
    } else if tool_calls.is_empty() {
        msg.insert("content".into(), Value::String(String::new()));
    } else {
        msg.insert("content".into(), Value::Null);
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    vec![Value::Object(msg)]
}

fn blocks_to_plain(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let ContentBlock::Text { text, .. } = b {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use elena_types::{
        BudgetLimits, ContentBlock, MessageId, PermissionSet, Role, SessionId, TenantContext,
        TenantId, TenantTier, ThreadId, UserId, WorkspaceId,
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

    #[test]
    fn anthropic_tool_schema_translates_to_openai_function() {
        let input = json!({
            "name": "echo_reverse",
            "description": "Reverses a string.",
            "input_schema": {
                "type": "object",
                "properties": {"word": {"type":"string"}},
                "required": ["word"]
            }
        });
        let out = anthropic_tool_to_openai(&input);
        assert_eq!(out["type"], "function");
        assert_eq!(out["function"]["name"], "echo_reverse");
        assert_eq!(out["function"]["description"], "Reverses a string.");
        assert_eq!(
            out["function"]["parameters"],
            json!({
                "type": "object",
                "properties": {"word": {"type":"string"}},
                "required": ["word"]
            })
        );
    }

    #[test]
    fn tool_choice_mapping_covers_every_variant() {
        assert_eq!(
            tool_choice_to_openai(&ToolChoice::Auto { disable_parallel_tool_use: false }),
            json!("auto")
        );
        assert_eq!(
            tool_choice_to_openai(&ToolChoice::Any { disable_parallel_tool_use: false }),
            json!("required")
        );
        assert_eq!(
            tool_choice_to_openai(&ToolChoice::Tool {
                name: "foo".into(),
                disable_parallel_tool_use: false
            }),
            json!({"type":"function","function":{"name":"foo"}})
        );
        assert_eq!(tool_choice_to_openai(&ToolChoice::None), json!("none"));
    }

    #[test]
    fn user_message_becomes_role_user() {
        let tid = ThreadId::new();
        let ten = TenantId::new();
        let msg = Message::user_text(ten, tid, "hello");
        let v = message_to_openai(&msg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"], "hello");
    }

    #[test]
    fn assistant_with_tool_use_emits_tool_calls() {
        let call_id = ToolCallId::new();
        let msg = Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::Assistant,
            kind: MessageKind::Assistant { stop_reason: None, is_api_error: false },
            content: vec![
                ContentBlock::Text { text: "Let me check.".into(), cache_control: None },
                ContentBlock::ToolUse {
                    id: call_id,
                    name: "echo_reverse".into(),
                    input: json!({"word": "hello"}),
                    cache_control: None,
                },
            ],
            created_at: chrono::Utc::now(),
            token_count: None,
            parent_id: None,
        };
        let v = message_to_openai(&msg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "assistant");
        assert_eq!(v[0]["content"], "Let me check.");
        let calls = v[0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], call_id.to_string());
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "echo_reverse");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"word":"hello"}"#);
    }

    #[test]
    fn tool_result_block_becomes_role_tool() {
        let call_id = ToolCallId::new();
        let msg = Message {
            id: MessageId::new(),
            thread_id: ThreadId::new(),
            tenant_id: TenantId::new(),
            role: Role::User,
            kind: MessageKind::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call_id,
                content: ToolResultContent::text("olleh"),
                is_error: false,
                cache_control: None,
            }],
            created_at: chrono::Utc::now(),
            token_count: None,
            parent_id: None,
        };
        let v = message_to_openai(&msg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "tool");
        assert_eq!(v[0]["tool_call_id"], call_id.to_string());
        assert_eq!(v[0]["content"], "olleh");
    }

    #[test]
    fn build_body_includes_stream_and_model() {
        let req = LlmRequest {
            provider: "groq".to_owned(),
            model: elena_types::ModelId::new("llama-3.3-70b-versatile"),
            tenant: tenant(),
            messages: vec![Message::user_text(TenantId::new(), ThreadId::new(), "hi")],
            system: vec![ContentBlock::Text { text: "Be brief.".into(), cache_control: None }],
            tools: vec![],
            tool_choice: None,
            max_tokens: 64,
            thinking: None,
            temperature: Some(0.5),
            top_p: None,
            stop_sequences: vec!["###".into()],
            options: crate::RequestOptions::default(),
        };
        let body = build_body(&req);
        assert_eq!(body["model"], "llama-3.3-70b-versatile");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["stop"][0], "###");
        // First message is the system.
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "Be brief.");
        // Second is the user.
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[test]
    fn assembler_emits_text_delta_and_done_on_simple_stream() {
        let mut asm = OpenAiAssembler::new();
        let chunk: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{"role":"assistant","content":"Hel"},"finish_reason":null}]
        })).unwrap();
        let events = asm.handle(chunk);
        assert!(matches!(events[0], StreamEvent::MessageStart { .. }));
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::TextDelta { delta } if delta == "Hel"))
        );
        assert!(!asm.finished);

        let chunk2: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}
        }))
        .unwrap();
        let events2 = asm.handle(chunk2);
        assert!(
            events2.iter().any(|e| matches!(e, StreamEvent::TextDelta { delta } if delta == "lo"))
        );
        assert!(asm.finished);
        assert_eq!(asm.usage.input_tokens, 5);
        assert_eq!(asm.usage.output_tokens, 2);
    }

    #[test]
    fn assembler_emits_tool_use_events_with_streaming_args() {
        let mut asm = OpenAiAssembler::new();

        // Chunk 1: role + empty delta.
        let c1: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{"role":"assistant"}}]
        }))
        .unwrap();
        let _ = asm.handle(c1);

        // Chunk 2: tool_call start with name + partial args.
        let c2: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{
                "tool_calls":[{
                    "index":0,
                    "id":"call_xyz",
                    "type":"function",
                    "function":{"name":"echo_reverse","arguments":"{\"wo"}
                }]
            }}]
        }))
        .unwrap();
        let events = asm.handle(c2);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::ToolUseStart { name, .. } if name == "echo_reverse")
        ));
        assert!(events.iter().any(|e| matches!(e, StreamEvent::ToolUseInputDelta { partial_json, .. } if partial_json == "{\"wo")));

        // Chunk 3: more args.
        let c3: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{
                "tool_calls":[{
                    "index":0,
                    "function":{"arguments":"rd\":\"hi\"}"}
                }]
            }}]
        }))
        .unwrap();
        let events3 = asm.handle(c3);
        assert!(events3.iter().any(|e| matches!(e, StreamEvent::ToolUseInputDelta { partial_json, .. } if partial_json == "rd\":\"hi\"}")));

        // Chunk 4: finish.
        let c4: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":10,"completion_tokens":6,"total_tokens":16}
        }))
        .unwrap();
        let events4 = asm.handle(c4);
        let complete = events4
            .iter()
            .find(|e| matches!(e, StreamEvent::ToolUseComplete { .. }))
            .expect("expected ToolUseComplete");
        if let StreamEvent::ToolUseComplete { name, input, .. } = complete {
            assert_eq!(name, "echo_reverse");
            assert_eq!(input, &json!({"word": "hi"}));
        }
        assert!(asm.finished);
    }

    #[test]
    fn assembler_passes_reasoning_content_as_thinking_delta() {
        let mut asm = OpenAiAssembler::new();
        let c: StreamChunk = serde_json::from_value(json!({
            "choices":[{"index":0,"delta":{"role":"assistant","reasoning":"Let me think...","content":""}}]
        })).unwrap();
        let events = asm.handle(c);
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::ThinkingDelta { delta } if delta == "Let me think...")
        ));
    }
}
