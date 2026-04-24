//! Anthropic Messages API client.
//!
//! The client is a thin shell around [`reqwest::Client`] that:
//!
//! 1. Builds the wire body via [`crate::wire::build_wire_body`].
//! 2. POSTs to `/v1/messages` with `stream: true`.
//! 3. Drains the response body via [`crate::sse::SseExtractor`], feeding
//!    parsed [`crate::events::AnthropicEvent`] into a
//!    [`crate::assembler::StreamAssembler`].
//! 4. Emits [`StreamEvent`]s to the caller over an `mpsc` channel.
//! 5. Classifies HTTP errors via [`crate::retry::classify_http`] and applies
//!    [`crate::retry::decide_retry`] — retries happen transparently.
//!
//! Cancellation flows end-to-end: every `await` point selects against a
//! caller-supplied [`CancellationToken`]. Dropping the returned stream
//! also aborts the in-flight HTTP request.

use std::sync::Arc;
use std::time::Duration;

use elena_config::AnthropicConfig;
use elena_types::{ElenaError, LlmApiError, StreamEvent, Terminal};
use futures::Stream;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::{
    assembler::StreamAssembler,
    cache::CachePolicy,
    events::AnthropicEvent,
    provider::LlmClient,
    request::LlmRequest,
    retry::{RetryDecision, RetryPolicy, classify_http, decide_retry},
    sse::SseExtractor,
    wire::{beta_headers, build_wire_body},
};

/// Authentication for Anthropic.
///
/// API-key auth only is supported here. OAuth, Bedrock, Foundry, Vertex
/// variants can be added as additional enum cases later without breaking
/// the `AnthropicClient` API.
#[derive(Debug, Clone)]
pub enum AnthropicAuth {
    /// `x-api-key` header authentication.
    ApiKey(SecretString),
}

impl AnthropicAuth {
    fn apply(&self, headers: &mut HeaderMap) -> Result<(), LlmApiError> {
        match self {
            Self::ApiKey(key) => {
                let value = HeaderValue::from_str(key.expose_secret())
                    .map_err(|_| LlmApiError::InvalidApiKey)?;
                headers.insert("x-api-key", value);
                Ok(())
            }
        }
    }
}

/// Cheap-to-clone handle to an Anthropic Messages API client.
#[derive(Clone)]
pub struct AnthropicClient {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("base_url", &self.inner.base_url)
            .field("api_version", &self.inner.api_version)
            .finish_non_exhaustive()
    }
}

struct Inner {
    http: Client,
    base_url: String,
    api_version: String,
    auth: AnthropicAuth,
    retry_policy: RetryPolicy,
}

impl AnthropicClient {
    /// Build a client from operator config + auth.
    ///
    /// Fails only if the HTTP client itself can't be constructed (bad TLS
    /// config). Most validation of `cfg` happens at `elena_config::validate`.
    pub fn new(cfg: &AnthropicConfig, auth: AnthropicAuth) -> Result<Self, LlmApiError> {
        let mut builder = Client::builder()
            .user_agent(concat!("elena-llm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
            .http2_prior_knowledge()
            .pool_max_idle_per_host(cfg.pool_max_idle_per_host);
        if let Some(ms) = cfg.request_timeout_ms {
            builder = builder.timeout(Duration::from_millis(ms));
        }
        let http =
            builder.build().map_err(|e| LlmApiError::ConnectionError { message: e.to_string() })?;

        Ok(Self {
            inner: Arc::new(Inner {
                http,
                base_url: cfg.base_url.clone(),
                api_version: cfg.api_version.clone(),
                auth,
                retry_policy: RetryPolicy {
                    max_attempts: cfg.max_attempts,
                    ..RetryPolicy::DEFAULT
                },
            }),
        })
    }

    /// Stream a completion from the Anthropic Messages API.
    ///
    /// The returned stream yields [`StreamEvent`]s and terminates when the
    /// model finishes (or retry budget is exhausted, or the caller cancels).
    /// Retries happen transparently inside this call — consumers only see
    /// events from the successful attempt.
    ///
    /// Cancellation: dropping the stream cancels the in-flight HTTP request.
    /// Passing a cancelled [`CancellationToken`] causes the stream to emit
    /// `StreamEvent::Error(ElenaError::Aborted)` and terminate.
    #[must_use]
    pub fn stream(
        &self,
        req: LlmRequest,
        policy: CachePolicy,
        cancel: CancellationToken,
    ) -> BoxStream<'static, StreamEvent> {
        let (tx, rx) = mpsc::channel::<StreamEvent>(64);
        let this = self.clone();

        tokio::spawn(async move {
            this.run_stream(req, policy, cancel, tx).await;
        });

        ReceiverStream::new(rx).boxed()
    }

    async fn run_stream(
        &self,
        req: LlmRequest,
        policy: CachePolicy,
        cancel: CancellationToken,
        tx: mpsc::Sender<StreamEvent>,
    ) {
        let retry_policy = req.options.retry_policy.unwrap_or(self.inner.retry_policy);
        let mut attempt: u32 = 0;
        let mut consecutive_529s: u32 = 0;

        loop {
            attempt += 1;
            let attempt_span = tracing::info_span!(
                "llm.attempt",
                attempt = attempt,
                model = %req.model,
            );
            let _enter = attempt_span.enter();

            let outcome = self.execute_once(&req, &policy, cancel.clone(), tx.clone()).await;

            match outcome {
                StreamOutcome::Completed => {
                    tracing::info!(attempt = attempt, "llm stream completed");
                    return;
                }
                StreamOutcome::Cancelled => {
                    tracing::info!("llm stream cancelled by caller");
                    let _ = tx.send(StreamEvent::Error(ElenaError::Aborted)).await;
                    let _ = tx.send(StreamEvent::Done(Terminal::AbortedStreaming)).await;
                    return;
                }
                StreamOutcome::Error(err) => {
                    if matches!(err, LlmApiError::ServerOverload) {
                        consecutive_529s += 1;
                    } else {
                        consecutive_529s = 0;
                    }

                    match decide_retry(&err, attempt, consecutive_529s, &retry_policy) {
                        RetryDecision::Retry { delay } => {
                            tracing::warn!(
                                ?err,
                                ?delay,
                                attempt,
                                "retrying after classified LLM error",
                            );
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
                            tracing::error!(?final_err, attempt, "llm stream gave up");
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

    /// Run a single attempt. Emits events to `tx` on success; returns
    /// [`StreamOutcome`] for the retry layer to act on.
    async fn execute_once(
        &self,
        req: &LlmRequest,
        policy: &CachePolicy,
        cancel: CancellationToken,
        tx: mpsc::Sender<StreamEvent>,
    ) -> StreamOutcome {
        if cancel.is_cancelled() {
            return StreamOutcome::Cancelled;
        }

        let body = build_wire_body(req, policy);
        let betas = beta_headers(req, policy);

        let mut headers = HeaderMap::new();
        if let Err(err) = self.inner.auth.apply(&mut headers) {
            return StreamOutcome::Error(err);
        }
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_str(&self.inner.api_version)
                .unwrap_or(HeaderValue::from_static("2023-06-01")),
        );
        if !betas.is_empty() {
            let joined = betas.join(",");
            if let Ok(v) = HeaderValue::from_str(&joined) {
                headers.insert(HeaderName::from_static("anthropic-beta"), v);
            }
        }
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("text/event-stream"),
        );

        let url = format!("{}/v1/messages", self.inner.base_url);

        let send = self.inner.http.post(&url).headers(headers).json(&body).send();

        let response = tokio::select! {
            res = send => res,
            () = cancel.cancelled() => return StreamOutcome::Cancelled,
        };

        let response = match response {
            Ok(r) => r,
            Err(e) => {
                return StreamOutcome::Error(LlmApiError::ConnectionError {
                    message: e.to_string(),
                });
            }
        };

        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body_snip = response.text().await.unwrap_or_default();
            let truncated = body_snippet(&body_snip, 2048);
            return StreamOutcome::Error(classify_http(status, &headers, Some(&truncated)));
        }

        self.pump_stream(req, response, cancel, tx).await
    }

    /// Drain the SSE body, feed events to the assembler, and forward stream
    /// events to the caller.
    async fn pump_stream(
        &self,
        req: &LlmRequest,
        response: reqwest::Response,
        cancel: CancellationToken,
        tx: mpsc::Sender<StreamEvent>,
    ) -> StreamOutcome {
        let mut assembler = StreamAssembler::new(req.tenant.tenant_id, req.tenant.thread_id);
        let mut extractor = SseExtractor::new();
        let mut body = response.bytes_stream();

        while let Some(chunk) = tokio::select! {
            next = body.next() => next,
            () = cancel.cancelled() => return StreamOutcome::Cancelled,
        } {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    return StreamOutcome::Error(LlmApiError::ConnectionError {
                        message: e.to_string(),
                    });
                }
            };

            for frame in extractor.push(&bytes) {
                if frame.data == "[DONE]" {
                    continue;
                }
                let event: AnthropicEvent = match serde_json::from_str(&frame.data) {
                    Ok(ev) => ev,
                    Err(err) => {
                        tracing::warn!(?err, data = %frame.data, "skipping unparseable SSE frame");
                        continue;
                    }
                };

                for stream_event in assembler.handle(event) {
                    if tx.send(stream_event).await.is_err() {
                        // Consumer dropped the stream — treat as cancellation.
                        return StreamOutcome::Cancelled;
                    }
                }

                if assembler.is_finished() {
                    let finished = std::mem::replace(
                        &mut assembler,
                        StreamAssembler::new(req.tenant.tenant_id, req.tenant.thread_id),
                    )
                    .finish();
                    let _ = tx.send(StreamEvent::UsageUpdate(finished.usage)).await;
                    let _ = tx.send(StreamEvent::Done(Terminal::Completed)).await;
                    return StreamOutcome::Completed;
                }
            }
        }

        // Stream ended without `message_stop` — partial completion.
        StreamOutcome::Error(LlmApiError::ConnectionError {
            message: "stream ended before message_stop".into(),
        })
    }
}

impl LlmClient for AnthropicClient {
    #[allow(clippy::unnecessary_literal_bound)]
    fn provider(&self) -> &str {
        "anthropic"
    }

    fn stream(
        &self,
        req: LlmRequest,
        policy: CachePolicy,
        cancel: CancellationToken,
    ) -> BoxStream<'static, StreamEvent> {
        // Delegate to the inherent method. Keeping both the inherent +
        // trait methods lets concrete-type callers avoid the vtable hop
        // while dyn callers still work.
        Self::stream(self, req, policy, cancel)
    }
}

enum StreamOutcome {
    Completed,
    Cancelled,
    Error(LlmApiError),
}

fn body_snippet(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        body.to_owned()
    } else {
        let mut idx = max_bytes;
        while idx > 0 && !body.is_char_boundary(idx) {
            idx -= 1;
        }
        let mut out = body[..idx].to_owned();
        out.push('…');
        out
    }
}

/// Manual re-export for the `Stream` trait bound on `BoxStream`.
#[allow(dead_code)]
fn _assert_stream<S: Stream<Item = StreamEvent> + Send + 'static>(_: S) {}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    #[test]
    fn api_key_auth_sets_header() {
        let mut headers = HeaderMap::new();
        let auth = AnthropicAuth::ApiKey(SecretString::from("sk-test-123"));
        auth.apply(&mut headers).unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-test-123");
    }

    #[test]
    fn api_key_rejects_non_ascii() {
        // `HeaderValue::from_str` rejects CR/LF; our API returns
        // InvalidApiKey on anything malformed.
        let mut headers = HeaderMap::new();
        let auth = AnthropicAuth::ApiKey(SecretString::from("bad\nkey"));
        let err = auth.apply(&mut headers).unwrap_err();
        assert_eq!(err, LlmApiError::InvalidApiKey);
    }

    #[test]
    fn body_snippet_truncates_on_char_boundary() {
        let s = "héllo 🎉".repeat(5);
        let snip = body_snippet(&s, 10);
        assert!(snip.len() <= s.len());
        assert!(snip.chars().last().is_some());
    }

    #[test]
    fn client_builds_from_minimal_config() {
        let cfg = AnthropicConfig {
            api_key: SecretString::from("sk-x"),
            base_url: "https://api.anthropic.com".into(),
            api_version: "2023-06-01".into(),
            request_timeout_ms: None,
            connect_timeout_ms: 5_000,
            max_attempts: 3,
            pool_max_idle_per_host: 64,
        };
        let client =
            AnthropicClient::new(&cfg, AnthropicAuth::ApiKey(cfg.api_key.clone())).unwrap();
        assert!(format!("{client:?}").contains("AnthropicClient"));
    }
}
