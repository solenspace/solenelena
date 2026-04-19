//! Elena Slack connector — gRPC plugin sidecar.
//!
//! Exposes two actions against Slack's Web API:
//!
//! - **`post_message`** (write) — `chat.postMessage`. Posts a text message
//!   to a channel. Returns the posted message's `ts` so callers can
//!   correlate with later API calls.
//! - **`list_channels`** (read) — `conversations.list`. Returns up to
//!   200 channels the bot can see; useful for the LLM picking a target
//!   channel by name.
//!
//! Auth: bot token from `SLACK_BOT_TOKEN` at sidecar startup. Multi-tenant
//! credential injection via the worker's `x-elena-cred-*` gRPC metadata
//! is a v1.0.x feature (planned in
//! `crates/elena-plugins/src/action_tool.rs`).
//!
//! API base URL is configurable via `SLACK_API_BASE_URL` so wiremock
//! integration tests can stand in for the real Slack API.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
// The connector binary necessarily prints to stderr/stdout for its
// startup banner + transport-level errors that don't surface through
// gRPC. Pedantic-clean is not the goal here.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::missing_fields_in_debug
)]

use std::time::Duration;

use elena_plugins::proto::pb;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use tracing::warn;

const DEFAULT_API_BASE: &str = "https://slack.com/api";

/// Default per-call HTTP timeout. Slack's chat.postMessage typically
/// returns in <1s; allow 10s for tail latency + retry headroom.
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Per-tenant credential metadata key — set by the worker from
/// `tenant_credentials` for the requesting tenant when present.
const CRED_HEADER: &str = "x-elena-cred-token";

/// Pick the bot token to use for one call: per-tenant from gRPC metadata
/// when present, otherwise the env-default the sidecar booted with.
fn resolve_token(
    metadata: &tonic::metadata::MetadataMap,
    env_default: &SecretString,
) -> SecretString {
    metadata
        .get(CRED_HEADER)
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| env_default.clone(), |s| SecretString::from(s.to_owned()))
}

/// gRPC service implementing Slack actions.
#[derive(Clone)]
pub struct SlackConnector {
    api_base: String,
    bot_token: SecretString,
    http: reqwest::Client,
}

impl std::fmt::Debug for SlackConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConnector")
            .field("api_base", &self.api_base)
            .field("bot_token", &"<redacted>")
            .finish()
    }
}

impl SlackConnector {
    /// Build a connector pointing at the configured Slack API base.
    /// `bot_token` is the `xoxb-...` Slack bot token.
    #[must_use]
    pub fn new(api_base: impl Into<String>, bot_token: SecretString) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_base: api_base.into(), bot_token, http }
    }

    /// Build from environment. Reads `SLACK_BOT_TOKEN` (required) and
    /// `SLACK_API_BASE_URL` (defaults to https://slack.com/api).
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("SLACK_BOT_TOKEN")
            .map_err(|_| anyhow::anyhow!("SLACK_BOT_TOKEN is required"))?;
        let base =
            std::env::var("SLACK_API_BASE_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_owned());
        Ok(Self::new(base, SecretString::from(token)))
    }
}

#[tonic::async_trait]
impl pb::elena_plugin_server::ElenaPlugin for SlackConnector {
    async fn get_manifest(&self, _: Request<()>) -> Result<Response<pb::PluginManifest>, Status> {
        Ok(Response::new(pb::PluginManifest {
            id: "slack".into(),
            name: "Slack".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            actions: vec![
                pb::ActionDefinition {
                    name: "post_message".into(),
                    description: "Post a message to a Slack channel via \
                                  chat.postMessage. Input: { channel, text }. \
                                  Output: { ok, ts, channel } where `ts` is \
                                  the posted message's timestamp."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "channel": {"type": "string", "description": "Channel id (Cxxx) or name (#general)"},
                            "text": {"type": "string", "minLength": 1}
                        },
                        "required": ["channel", "text"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "ok": {"type": "boolean"},
                            "ts": {"type": "string"},
                            "channel": {"type": "string"}
                        },
                        "required": ["ok"]
                    }"#
                    .into(),
                    is_read_only: false,
                },
                pb::ActionDefinition {
                    name: "list_channels".into(),
                    description: "List up to 200 channels the bot can see via \
                                  conversations.list. Input: optional { types, \
                                  exclude_archived }. Output: { channels: [{id, \
                                  name, is_private}] }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "types": {"type": "string", "description": "Comma-separated channel types: public_channel,private_channel,im,mpim"},
                            "exclude_archived": {"type": "boolean"}
                        }
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "channels": {"type": "array"}
                        },
                        "required": ["channels"]
                    }"#
                    .into(),
                    is_read_only: true,
                },
            ],
            // Required Slack scopes (informational; enforced by the
            // operator who provisions the bot token, not by this code).
            required_credentials: vec![
                "chat:write".into(),
                "channels:read".into(),
            ],
        }))
    }

    type ExecuteStream = tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

    async fn execute(
        &self,
        req: Request<pb::PluginRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        // Per-tenant credentials, if present, override the env-default
        // bot token. This is the multi-tenant injection seam — the worker
        // attaches `x-elena-cred-token` to each call from the
        // `tenant_credentials` table.
        let token = resolve_token(req.metadata(), &self.bot_token);
        let inner = req.into_inner();
        let body: Value = serde_json::from_slice(&inner.input_json)
            .map_err(|e| Status::invalid_argument(format!("input is not valid JSON: {e}")))?;

        let (tx, rx) = mpsc::channel::<Result<pb::PluginResponse, Status>>(8);
        let me = self.clone();
        tokio::spawn(async move {
            let result = match inner.action.as_str() {
                "post_message" => me.post_message(body, &token).await,
                "list_channels" => me.list_channels(body, &token).await,
                other => Err(SlackError::Unknown(other.to_owned())),
            };
            let response = match result {
                Ok(payload) => pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Result(pb::FinalResult {
                        output_json: payload.into_bytes(),
                        is_error: false,
                    })),
                },
                Err(err) => pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Result(pb::FinalResult {
                        output_json: serde_json::to_vec(
                            &serde_json::json!({"ok": false, "error": err.to_string()}),
                        )
                        .unwrap_or_default(),
                        is_error: true,
                    })),
                },
            };
            let _ = tx.send(Ok(response)).await;
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
        // Cheap probe: ping `auth.test`. Failure flips the registry's
        // health gauge so the LLM stops being offered Slack.
        let url = format!("{}/auth.test", self.api_base);
        let token = self.bot_token.expose_secret().to_string();
        match self.http.post(&url).bearer_auth(token).send().await {
            Ok(resp) if resp.status().is_success() => {
                Ok(Response::new(pb::HealthResponse { ok: true, detail: "auth.test ok".into() }))
            }
            Ok(resp) => {
                let status = resp.status();
                Ok(Response::new(pb::HealthResponse {
                    ok: false,
                    detail: format!("auth.test returned {status}"),
                }))
            }
            Err(e) => {
                warn!(?e, "slack health probe failed");
                Ok(Response::new(pb::HealthResponse {
                    ok: false,
                    detail: format!("auth.test transport error: {e}"),
                }))
            }
        }
    }
}

impl SlackConnector {
    async fn post_message(
        &self,
        input: Value,
        bot_token: &SecretString,
    ) -> Result<String, SlackError> {
        let channel = input
            .get("channel")
            .and_then(Value::as_str)
            .ok_or_else(|| SlackError::BadInput("missing required field: channel".into()))?;
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| SlackError::BadInput("missing required field: text".into()))?;
        if text.is_empty() {
            return Err(SlackError::BadInput("text must not be empty".into()));
        }

        let url = format!("{}/chat.postMessage", self.api_base);
        let body = serde_json::json!({"channel": channel, "text": text});
        let token = bot_token.expose_secret().to_string();
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| SlackError::Transport(e.to_string()))?;
        let status = resp.status();
        let body: Value =
            resp.json().await.map_err(|e| SlackError::Transport(format!("decode: {e}")))?;
        if !status.is_success() {
            return Err(SlackError::Api(format!("HTTP {status}: {body}")));
        }
        if !body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Err(SlackError::Api(format!(
                "slack rejected: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("unknown")
            )));
        }
        Ok(body.to_string())
    }

    async fn list_channels(
        &self,
        input: Value,
        bot_token: &SecretString,
    ) -> Result<String, SlackError> {
        let types = input.get("types").and_then(Value::as_str).unwrap_or("public_channel");
        let exclude_archived =
            input.get("exclude_archived").and_then(Value::as_bool).unwrap_or(true);

        let url = format!("{}/conversations.list", self.api_base);
        let token = bot_token.expose_secret().to_string();
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .query(&[
                ("types", types),
                ("exclude_archived", if exclude_archived { "true" } else { "false" }),
                ("limit", "200"),
            ])
            .send()
            .await
            .map_err(|e| SlackError::Transport(e.to_string()))?;
        let status = resp.status();
        let body: Value =
            resp.json().await.map_err(|e| SlackError::Transport(format!("decode: {e}")))?;
        if !status.is_success() {
            return Err(SlackError::Api(format!("HTTP {status}: {body}")));
        }
        if !body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return Err(SlackError::Api(format!(
                "slack rejected: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("unknown")
            )));
        }
        // Slim down the response to the LLM-friendly shape declared in
        // the manifest's output_schema.
        let channels = body
            .get("channels")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.get("id").and_then(Value::as_str).unwrap_or(""),
                            "name": c.get("name").and_then(Value::as_str).unwrap_or(""),
                            "is_private": c.get("is_private").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(serde_json::json!({"channels": channels}).to_string())
    }
}

/// Categorised error from a Slack action call. Surfaces back to the LLM
/// as a structured `is_error: true` tool result.
#[derive(Debug, thiserror::Error)]
enum SlackError {
    #[error("invalid input: {0}")]
    BadInput(String),
    #[error("slack api error: {0}")]
    Api(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("unknown action: {0:?}")]
    Unknown(String),
}

#[cfg(test)]
mod tests {
    use elena_plugins::proto::pb::elena_plugin_server::ElenaPlugin;
    use secrecy::SecretString;

    use super::*;

    fn connector(base: &str) -> SlackConnector {
        SlackConnector::new(base, SecretString::from("xoxb-test-token"))
    }

    #[tokio::test]
    async fn manifest_advertises_two_actions_with_correct_read_only_bits() {
        let c = connector("http://localhost");
        let resp = c.get_manifest(tonic::Request::new(())).await.unwrap();
        let manifest = resp.into_inner();
        assert_eq!(manifest.id, "slack");
        assert_eq!(manifest.actions.len(), 2);
        let post = manifest.actions.iter().find(|a| a.name == "post_message").unwrap();
        let list = manifest.actions.iter().find(|a| a.name == "list_channels").unwrap();
        assert!(!post.is_read_only, "post_message must be a write action");
        assert!(list.is_read_only, "list_channels must be read-only");
    }

    #[tokio::test]
    async fn unknown_action_returns_error_payload_not_grpc_failure() {
        // Plugin protocol convention: bad-action / bad-input flow back
        // as `is_error: true` payloads, not transport-level Status::Error.
        // The orchestrator surfaces these as tool_result.is_error=true.
        let c = connector("http://localhost");
        let req = tonic::Request::new(pb::PluginRequest {
            action: "non_existent_action".into(),
            input_json: br"{}".to_vec(),
            tool_call_id: String::new(),
            context: None,
        });
        let resp = c.execute(req).await.unwrap();
        let mut stream = resp.into_inner();
        let first = futures::StreamExt::next(&mut stream).await.unwrap().unwrap();
        if let Some(pb::plugin_response::Payload::Result(r)) = first.payload {
            assert!(r.is_error, "expected is_error=true for unknown action");
            let body: Value = serde_json::from_slice(&r.output_json).unwrap();
            assert_eq!(body.get("ok").and_then(Value::as_bool), Some(false));
        } else {
            panic!("expected Result payload");
        }
    }

    #[test]
    fn resolve_token_uses_metadata_when_present() {
        let env_default = SecretString::from("env-token");
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert(CRED_HEADER, "tenant-token".parse().unwrap());
        let resolved = resolve_token(&metadata, &env_default);
        assert_eq!(resolved.expose_secret(), "tenant-token");
    }

    #[test]
    fn resolve_token_falls_back_to_env_default_when_metadata_absent() {
        let env_default = SecretString::from("env-token");
        let metadata = tonic::metadata::MetadataMap::new();
        let resolved = resolve_token(&metadata, &env_default);
        assert_eq!(resolved.expose_secret(), "env-token");
    }

    #[test]
    fn debug_redacts_token() {
        let c = connector("http://localhost");
        let s = format!("{c:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("xoxb-test-token"));
    }
}
