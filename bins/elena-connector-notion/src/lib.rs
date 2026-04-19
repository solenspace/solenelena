//! Elena Notion connector — gRPC plugin sidecar.
//!
//! Application-agnostic Notion integration. Useful anywhere the LLM
//! needs to read or write Notion pages — Solen workflows that file
//! reports, Hannlys Seeds whose creator chose Notion as one of several
//! possible output formats, third-party apps. Three actions:
//!
//! - **`create_page`** (write) — `POST /v1/pages`. Creates a child page
//!   under a parent page id, with a title and an array of paragraph
//!   blocks. Returns `{ id, url }`.
//! - **`append_blocks`** (write) — `PATCH /v1/blocks/{id}/children`.
//!   Appends paragraph blocks to an existing page (used for
//!   re-personalization without replacing the page). Returns
//!   `{ ok, appended_count }`.
//! - **`get_page`** (read) — `GET /v1/pages/{id}`. Fetches a page's
//!   metadata. Used by the LLM to confirm a page exists before linking.
//!
//! Auth: integration token from `NOTION_TOKEN` (a `secret_*` integration
//! secret, not a user OAuth token). Per-tenant credential injection
//! lands in v1.0.x.
//!
//! API base URL is configurable via `NOTION_API_BASE_URL` so wiremock
//! integration tests can stand in for real Notion.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
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

const DEFAULT_API_BASE: &str = "https://api.notion.com";
const NOTION_VERSION: &str = "2022-06-28";
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Per-tenant integration-token metadata key.
const CRED_HEADER: &str = "x-elena-cred-token";

fn resolve_token(
    metadata: &tonic::metadata::MetadataMap,
    env_default: &SecretString,
) -> SecretString {
    metadata.get(CRED_HEADER).and_then(|v| v.to_str().ok()).map_or_else(
        || env_default.clone(),
        |s| SecretString::from(s.to_owned()),
    )
}

/// gRPC service implementing Notion actions.
#[derive(Clone)]
pub struct NotionConnector {
    api_base: String,
    token: SecretString,
    http: reqwest::Client,
}

impl std::fmt::Debug for NotionConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotionConnector")
            .field("api_base", &self.api_base)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl NotionConnector {
    /// Build a connector. `token` is the Notion integration secret.
    #[must_use]
    pub fn new(api_base: impl Into<String>, token: SecretString) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_base: api_base.into(), token, http }
    }

    /// Build from environment.
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("NOTION_TOKEN")
            .map_err(|_| anyhow::anyhow!("NOTION_TOKEN is required"))?;
        let base =
            std::env::var("NOTION_API_BASE_URL").unwrap_or_else(|_| DEFAULT_API_BASE.to_owned());
        Ok(Self::new(base, SecretString::from(token)))
    }
}

#[tonic::async_trait]
impl pb::elena_plugin_server::ElenaPlugin for NotionConnector {
    async fn get_manifest(&self, _: Request<()>) -> Result<Response<pb::PluginManifest>, Status> {
        Ok(Response::new(pb::PluginManifest {
            id: "notion".into(),
            name: "Notion".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            actions: vec![
                pb::ActionDefinition {
                    name: "create_page".into(),
                    description: "Create a child page under a parent page. \
                                  Input: { parent_page_id, title, paragraphs: \
                                  [string] }. Output: { id, url }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "parent_page_id": {"type": "string", "minLength": 32},
                            "title":          {"type": "string", "minLength": 1, "maxLength": 200},
                            "paragraphs":     {
                                "type": "array",
                                "items": {"type": "string", "maxLength": 2000}
                            }
                        },
                        "required": ["parent_page_id", "title"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "id":  {"type": "string"},
                            "url": {"type": "string"}
                        },
                        "required": ["id"]
                    }"#
                    .into(),
                    is_read_only: false,
                },
                pb::ActionDefinition {
                    name: "append_blocks".into(),
                    description: "Append paragraph blocks to a Notion page. \
                                  Input: { page_id, paragraphs: [string] }. \
                                  Output: { ok, appended_count }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "page_id":    {"type": "string", "minLength": 32},
                            "paragraphs": {
                                "type": "array",
                                "items": {"type": "string", "maxLength": 2000},
                                "minItems": 1
                            }
                        },
                        "required": ["page_id", "paragraphs"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "ok":              {"type": "boolean"},
                            "appended_count":  {"type": "integer"}
                        },
                        "required": ["ok"]
                    }"#
                    .into(),
                    is_read_only: false,
                },
                pb::ActionDefinition {
                    name: "get_page".into(),
                    description: "Fetch a page's metadata by id. \
                                  Output: { id, url, archived, last_edited_time }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "page_id": {"type": "string", "minLength": 32}
                        },
                        "required": ["page_id"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "id":               {"type": "string"},
                            "url":              {"type": "string"},
                            "archived":         {"type": "boolean"},
                            "last_edited_time": {"type": "string"}
                        }
                    }"#
                    .into(),
                    is_read_only: true,
                },
            ],
            required_credentials: vec!["NOTION_TOKEN".into()],
        }))
    }

    type ExecuteStream = tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

    async fn execute(
        &self,
        req: Request<pb::PluginRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let token = resolve_token(req.metadata(), &self.token);
        let inner = req.into_inner();
        let body: Value = serde_json::from_slice(&inner.input_json)
            .map_err(|e| Status::invalid_argument(format!("input is not valid JSON: {e}")))?;

        let (tx, rx) = mpsc::channel::<Result<pb::PluginResponse, Status>>(8);
        let me = self.clone();
        tokio::spawn(async move {
            let result = match inner.action.as_str() {
                "create_page" => me.create_page(body, &token).await,
                "append_blocks" => me.append_blocks(body, &token).await,
                "get_page" => me.get_page(body, &token).await,
                other => Err(NotionError::Unknown(other.to_owned())),
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
        // Notion has no auth.test equivalent; the cheapest probe is
        // GET /v1/users/me which returns the bot's user object.
        // Health uses the env-default token (per-tenant tokens never
        // route through the health probe).
        let url = format!("{}/v1/users/me", self.api_base);
        let token = self.env_token();
        match self
            .http
            .get(&url)
            .bearer_auth(token)
            .header("Notion-Version", NOTION_VERSION)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                Ok(Response::new(pb::HealthResponse { ok: true, detail: "users.me ok".into() }))
            }
            Ok(resp) => Ok(Response::new(pb::HealthResponse {
                ok: false,
                detail: format!("users.me returned {}", resp.status()),
            })),
            Err(e) => {
                warn!(?e, "notion health probe failed");
                Ok(Response::new(pb::HealthResponse {
                    ok: false,
                    detail: format!("users.me transport error: {e}"),
                }))
            }
        }
    }
}

impl NotionConnector {
    fn env_token(&self) -> String {
        self.token.expose_secret().to_string()
    }

    async fn create_page(&self, input: Value, token: &SecretString) -> Result<String, NotionError> {
        let parent_id = input
            .get("parent_page_id")
            .and_then(Value::as_str)
            .ok_or_else(|| NotionError::BadInput("parent_page_id is required".into()))?;
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .ok_or_else(|| NotionError::BadInput("title is required".into()))?;
        let paragraphs = input
            .get("paragraphs")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect::<Vec<_>>())
            .unwrap_or_default();

        let body = serde_json::json!({
            "parent": {"type": "page_id", "page_id": parent_id},
            "properties": {
                "title": {
                    "title": [{"type": "text", "text": {"content": title}}]
                }
            },
            "children": paragraphs.iter().map(|p| paragraph_block(p)).collect::<Vec<_>>()
        });

        let resp = self.notion_post("/v1/pages", &body, token).await?;
        Ok(serde_json::json!({
            "id":  resp.get("id").cloned().unwrap_or(Value::Null),
            "url": resp.get("url").cloned().unwrap_or(Value::Null),
        })
        .to_string())
    }

    async fn append_blocks(
        &self,
        input: Value,
        token: &SecretString,
    ) -> Result<String, NotionError> {
        let page_id = input
            .get("page_id")
            .and_then(Value::as_str)
            .ok_or_else(|| NotionError::BadInput("page_id is required".into()))?;
        let paragraphs = input
            .get("paragraphs")
            .and_then(Value::as_array)
            .ok_or_else(|| NotionError::BadInput("paragraphs is required".into()))?;
        if paragraphs.is_empty() {
            return Err(NotionError::BadInput("paragraphs must contain at least one item".into()));
        }
        let count = paragraphs.len();
        let blocks: Vec<Value> =
            paragraphs.iter().filter_map(Value::as_str).map(paragraph_block).collect();

        let body = serde_json::json!({"children": blocks});
        let path = format!("/v1/blocks/{page_id}/children");
        self.notion_patch(&path, &body, token).await?;
        Ok(serde_json::json!({"ok": true, "appended_count": count}).to_string())
    }

    async fn get_page(&self, input: Value, token: &SecretString) -> Result<String, NotionError> {
        let page_id = input
            .get("page_id")
            .and_then(Value::as_str)
            .ok_or_else(|| NotionError::BadInput("page_id is required".into()))?;
        let path = format!("/v1/pages/{page_id}");
        let resp = self.notion_get(&path, token).await?;
        Ok(serde_json::json!({
            "id":               resp.get("id").cloned().unwrap_or(Value::Null),
            "url":              resp.get("url").cloned().unwrap_or(Value::Null),
            "archived":         resp.get("archived").cloned().unwrap_or(Value::Null),
            "last_edited_time": resp.get("last_edited_time").cloned().unwrap_or(Value::Null),
        })
        .to_string())
    }

    async fn notion_post(
        &self,
        path: &str,
        body: &Value,
        token: &SecretString,
    ) -> Result<Value, NotionError> {
        let url = format!("{}{path}", self.api_base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token.expose_secret())
            .header("Notion-Version", NOTION_VERSION)
            .json(body)
            .send()
            .await
            .map_err(|e| NotionError::Transport(e.to_string()))?;
        parse_notion_response(resp).await
    }

    async fn notion_patch(
        &self,
        path: &str,
        body: &Value,
        token: &SecretString,
    ) -> Result<Value, NotionError> {
        let url = format!("{}{path}", self.api_base);
        let resp = self
            .http
            .patch(&url)
            .bearer_auth(token.expose_secret())
            .header("Notion-Version", NOTION_VERSION)
            .json(body)
            .send()
            .await
            .map_err(|e| NotionError::Transport(e.to_string()))?;
        parse_notion_response(resp).await
    }

    async fn notion_get(&self, path: &str, token: &SecretString) -> Result<Value, NotionError> {
        let url = format!("{}{path}", self.api_base);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token.expose_secret())
            .header("Notion-Version", NOTION_VERSION)
            .send()
            .await
            .map_err(|e| NotionError::Transport(e.to_string()))?;
        parse_notion_response(resp).await
    }
}

async fn parse_notion_response(resp: reqwest::Response) -> Result<Value, NotionError> {
    let status = resp.status();
    let body: Value =
        resp.json().await.map_err(|e| NotionError::Transport(format!("decode: {e}")))?;
    if !status.is_success() {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("unknown");
        return Err(NotionError::Api(format!("HTTP {status}: {msg}")));
    }
    Ok(body)
}

fn paragraph_block(text: &str) -> Value {
    serde_json::json!({
        "object": "block",
        "type":   "paragraph",
        "paragraph": {
            "rich_text": [{"type": "text", "text": {"content": text}}]
        }
    })
}

#[derive(Debug, thiserror::Error)]
enum NotionError {
    #[error("invalid input: {0}")]
    BadInput(String),
    #[error("notion api error: {0}")]
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

    fn connector(base: &str) -> NotionConnector {
        NotionConnector::new(base, SecretString::from("secret_test_token"))
    }

    #[tokio::test]
    async fn manifest_advertises_three_actions_with_correct_read_only_bits() {
        let c = connector("http://localhost");
        let m = c.get_manifest(tonic::Request::new(())).await.unwrap().into_inner();
        assert_eq!(m.id, "notion");
        assert_eq!(m.actions.len(), 3);
        let writes: Vec<&str> =
            m.actions.iter().filter(|a| !a.is_read_only).map(|a| a.name.as_str()).collect();
        let reads: Vec<&str> =
            m.actions.iter().filter(|a| a.is_read_only).map(|a| a.name.as_str()).collect();
        assert_eq!(writes.len(), 2, "create_page + append_blocks must be writes");
        assert!(writes.contains(&"create_page"));
        assert!(writes.contains(&"append_blocks"));
        assert_eq!(reads, vec!["get_page"]);
    }

    #[test]
    fn paragraph_block_shape_matches_notion_schema() {
        let b = paragraph_block("hello world");
        assert_eq!(b["object"], "block");
        assert_eq!(b["type"], "paragraph");
        assert_eq!(b["paragraph"]["rich_text"][0]["text"]["content"], "hello world");
    }

    #[test]
    fn debug_redacts_token() {
        let c = connector("http://localhost");
        let s = format!("{c:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("secret_test_token"));
    }

    #[test]
    fn resolve_token_prefers_metadata_over_env_default() {
        let env_default = SecretString::from("env-token");
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert(CRED_HEADER, "tenant-token".parse().unwrap());
        assert_eq!(resolve_token(&metadata, &env_default).expose_secret(), "tenant-token");
    }

    #[test]
    fn resolve_token_falls_back_when_metadata_missing() {
        let env_default = SecretString::from("env-token");
        let metadata = tonic::metadata::MetadataMap::new();
        assert_eq!(resolve_token(&metadata, &env_default).expose_secret(), "env-token");
    }
}
