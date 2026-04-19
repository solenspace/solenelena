//! Elena Google Sheets connector — gRPC plugin sidecar.
//!
//! Reads and appends rows in a Google Sheet via the v4 REST API.
//! Application-agnostic: any Elena-hosted product that needs tabular
//! read/write can use it.
//!
//! Three actions:
//!
//! - **`get_values`** (read) — `GET /v4/spreadsheets/{id}/values/{range}`.
//!   Returns `{ range, values: [[string]] }`.
//! - **`append_values`** (write) — `POST .../values/{range}:append`.
//!   Appends rows in `USER_ENTERED` mode (Sheets parses `1.5`, `=SUM(...)`,
//!   etc. as if a user typed them). Returns `{ updated_range, updated_rows }`.
//! - **`get_metadata`** (read) — `GET /v4/spreadsheets/{id}`. Returns
//!   `{ title, sheets: [{title, sheetId}] }` so the LLM can pick a sheet
//!   by name before reading.
//!
//! Auth: a short-lived OAuth2 access token from `GOOGLE_ACCESS_TOKEN`.
//! Operators mint it via `gcloud auth application-default print-access-token`
//! or a token-broker service. v1.0 deliberately does **not** sign service
//! account JWTs in-process — that lands in v1.0.x with the
//! `jsonwebtoken` crate's RSA support so the hot path stays free of
//! private-key handling. Per-tenant credential injection lands in v1.0.x
//! via the worker's `x-elena-cred-*` gRPC metadata.
//!
//! API base URL is configurable via `GOOGLE_SHEETS_API_BASE_URL` so
//! wiremock integration tests can stand in for real Google.

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

const DEFAULT_API_BASE: &str = "https://sheets.googleapis.com";
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Per-tenant access-token metadata key.
const CRED_HEADER: &str = "x-elena-cred-token";

fn resolve_token(
    metadata: &tonic::metadata::MetadataMap,
    env_default: &SecretString,
) -> SecretString {
    metadata
        .get(CRED_HEADER)
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| env_default.clone(), |s| SecretString::from(s.to_owned()))
}

/// gRPC service implementing Sheets actions.
#[derive(Clone)]
pub struct SheetsConnector {
    api_base: String,
    access_token: SecretString,
    http: reqwest::Client,
}

impl std::fmt::Debug for SheetsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SheetsConnector")
            .field("api_base", &self.api_base)
            .field("access_token", &"<redacted>")
            .finish()
    }
}

impl SheetsConnector {
    /// Build a connector with an already-minted access token.
    #[must_use]
    pub fn new(api_base: impl Into<String>, access_token: SecretString) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_base: api_base.into(), access_token, http }
    }

    /// Build from environment.
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("GOOGLE_ACCESS_TOKEN").map_err(|_| {
            anyhow::anyhow!(
                "GOOGLE_ACCESS_TOKEN is required (mint via `gcloud auth \
                 application-default print-access-token` or a token-broker)"
            )
        })?;
        let base = std::env::var("GOOGLE_SHEETS_API_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_API_BASE.to_owned());
        Ok(Self::new(base, SecretString::from(token)))
    }
}

#[tonic::async_trait]
impl pb::elena_plugin_server::ElenaPlugin for SheetsConnector {
    async fn get_manifest(&self, _: Request<()>) -> Result<Response<pb::PluginManifest>, Status> {
        Ok(Response::new(pb::PluginManifest {
            id: "sheets".into(),
            name: "Google Sheets".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            actions: vec![
                pb::ActionDefinition {
                    name: "get_values".into(),
                    description: "Fetch a range from a sheet. Input: { \
                                  spreadsheet_id, range }. Range is A1 \
                                  notation, e.g. `Sales!A1:D10`. Output: \
                                  { range, values: [[string]] }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "spreadsheet_id": {"type": "string", "minLength": 8},
                            "range":          {"type": "string", "minLength": 1}
                        },
                        "required": ["spreadsheet_id", "range"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "range":  {"type": "string"},
                            "values": {"type": "array"}
                        },
                        "required": ["values"]
                    }"#
                    .into(),
                    is_read_only: true,
                },
                pb::ActionDefinition {
                    name: "append_values".into(),
                    description: "Append rows to a sheet. Input: { \
                                  spreadsheet_id, range, values: [[string]] }. \
                                  Uses USER_ENTERED so formulas + numeric \
                                  parsing work. Output: { updated_range, \
                                  updated_rows }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "spreadsheet_id": {"type": "string", "minLength": 8},
                            "range":          {"type": "string", "minLength": 1},
                            "values":         {
                                "type": "array",
                                "items": {"type": "array"},
                                "minItems": 1
                            }
                        },
                        "required": ["spreadsheet_id", "range", "values"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "updated_range": {"type": "string"},
                            "updated_rows":  {"type": "integer"}
                        }
                    }"#
                    .into(),
                    is_read_only: false,
                },
                pb::ActionDefinition {
                    name: "get_metadata".into(),
                    description: "Fetch a spreadsheet's metadata: title + \
                                  the list of sheets within. Useful for \
                                  picking a sheet by name before reading. \
                                  Output: { title, sheets: [{title, sheetId}] }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "spreadsheet_id": {"type": "string", "minLength": 8}
                        },
                        "required": ["spreadsheet_id"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "title":  {"type": "string"},
                            "sheets": {"type": "array"}
                        }
                    }"#
                    .into(),
                    is_read_only: true,
                },
            ],
            required_credentials: vec!["GOOGLE_ACCESS_TOKEN".into()],
        }))
    }

    type ExecuteStream = tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

    async fn execute(
        &self,
        req: Request<pb::PluginRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let token = resolve_token(req.metadata(), &self.access_token);
        let inner = req.into_inner();
        let body: Value = serde_json::from_slice(&inner.input_json)
            .map_err(|e| Status::invalid_argument(format!("input is not valid JSON: {e}")))?;

        let (tx, rx) = mpsc::channel::<Result<pb::PluginResponse, Status>>(8);
        let me = self.clone();
        tokio::spawn(async move {
            let result = match inner.action.as_str() {
                "get_values" => me.get_values(body, &token).await,
                "append_values" => me.append_values(body, &token).await,
                "get_metadata" => me.get_metadata(body, &token).await,
                other => Err(SheetsError::Unknown(other.to_owned())),
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
        // Cheapest auth probe: GET tokeninfo. We deliberately don't probe
        // sheets.googleapis.com without a known sheet ID since that would
        // 404 and look like a failure. Health uses the env-default token
        // (per-tenant tokens never route through the health probe).
        let url =
            format!("https://oauth2.googleapis.com/tokeninfo?access_token={}", self.env_token());
        match self.http.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                Ok(Response::new(pb::HealthResponse { ok: true, detail: "tokeninfo ok".into() }))
            }
            Ok(resp) => Ok(Response::new(pb::HealthResponse {
                ok: false,
                detail: format!("tokeninfo returned {}", resp.status()),
            })),
            Err(e) => {
                warn!(?e, "sheets health probe failed");
                Ok(Response::new(pb::HealthResponse {
                    ok: false,
                    detail: format!("tokeninfo transport error: {e}"),
                }))
            }
        }
    }
}

impl SheetsConnector {
    fn env_token(&self) -> String {
        self.access_token.expose_secret().to_string()
    }

    async fn get_values(&self, input: Value, token: &SecretString) -> Result<String, SheetsError> {
        let id = input
            .get("spreadsheet_id")
            .and_then(Value::as_str)
            .ok_or_else(|| SheetsError::BadInput("spreadsheet_id is required".into()))?;
        let range = input
            .get("range")
            .and_then(Value::as_str)
            .ok_or_else(|| SheetsError::BadInput("range is required".into()))?;
        let url = format!("{}/v4/spreadsheets/{id}/values/{}", self.api_base, urlencode(range));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(|e| SheetsError::Transport(e.to_string()))?;
        let body = parse_sheets_response(resp).await?;
        Ok(serde_json::json!({
            "range":  body.get("range").cloned().unwrap_or(Value::Null),
            "values": body.get("values").cloned().unwrap_or(Value::Array(vec![])),
        })
        .to_string())
    }

    async fn append_values(
        &self,
        input: Value,
        token: &SecretString,
    ) -> Result<String, SheetsError> {
        let id = input
            .get("spreadsheet_id")
            .and_then(Value::as_str)
            .ok_or_else(|| SheetsError::BadInput("spreadsheet_id is required".into()))?;
        let range = input
            .get("range")
            .and_then(Value::as_str)
            .ok_or_else(|| SheetsError::BadInput("range is required".into()))?;
        let values = input
            .get("values")
            .and_then(Value::as_array)
            .ok_or_else(|| SheetsError::BadInput("values is required (array of arrays)".into()))?;
        if values.is_empty() {
            return Err(SheetsError::BadInput("values must contain at least one row".into()));
        }
        let url = format!(
            "{}/v4/spreadsheets/{id}/values/{}:append?valueInputOption=USER_ENTERED",
            self.api_base,
            urlencode(range)
        );
        let body = serde_json::json!({"values": values});
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token.expose_secret())
            .json(&body)
            .send()
            .await
            .map_err(|e| SheetsError::Transport(e.to_string()))?;
        let body = parse_sheets_response(resp).await?;
        let updates = body.get("updates").cloned().unwrap_or(Value::Null);
        Ok(serde_json::json!({
            "updated_range": updates.get("updatedRange").cloned().unwrap_or(Value::Null),
            "updated_rows":  updates.get("updatedRows").cloned().unwrap_or(Value::from(0)),
        })
        .to_string())
    }

    async fn get_metadata(
        &self,
        input: Value,
        token: &SecretString,
    ) -> Result<String, SheetsError> {
        let id = input
            .get("spreadsheet_id")
            .and_then(Value::as_str)
            .ok_or_else(|| SheetsError::BadInput("spreadsheet_id is required".into()))?;
        let url = format!(
            "{}/v4/spreadsheets/{id}?fields=properties.title,sheets.properties",
            self.api_base
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(|e| SheetsError::Transport(e.to_string()))?;
        let body = parse_sheets_response(resp).await?;
        let title =
            body.get("properties").and_then(|p| p.get("title")).cloned().unwrap_or(Value::Null);
        let sheets = body
            .get("sheets")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("properties"))
                    .map(|p| {
                        serde_json::json!({
                            "title":   p.get("title").cloned().unwrap_or(Value::Null),
                            "sheetId": p.get("sheetId").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(serde_json::json!({"title": title, "sheets": sheets}).to_string())
    }
}

async fn parse_sheets_response(resp: reqwest::Response) -> Result<Value, SheetsError> {
    let status = resp.status();
    let body: Value =
        resp.json().await.map_err(|e| SheetsError::Transport(format!("decode: {e}")))?;
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(SheetsError::Api(format!("HTTP {status}: {msg}")));
    }
    Ok(body)
}

/// Conservative URL-encoding for sheet ranges. Sheet names may contain
/// spaces (`Sheet1!A1:B2`) and exclamation marks; reqwest's `query()`
/// helper encodes the value but we're building the path inline.
fn urlencode(input: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[derive(Debug, thiserror::Error)]
enum SheetsError {
    #[error("invalid input: {0}")]
    BadInput(String),
    #[error("sheets api error: {0}")]
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

    fn connector(base: &str) -> SheetsConnector {
        SheetsConnector::new(base, SecretString::from("ya29.test_token"))
    }

    #[tokio::test]
    async fn manifest_advertises_three_actions_with_correct_read_only_bits() {
        let c = connector("http://localhost");
        let m = c.get_manifest(tonic::Request::new(())).await.unwrap().into_inner();
        assert_eq!(m.id, "sheets");
        assert_eq!(m.actions.len(), 3);
        let writes: Vec<&str> =
            m.actions.iter().filter(|a| !a.is_read_only).map(|a| a.name.as_str()).collect();
        assert_eq!(writes, vec!["append_values"]);
        let reads: Vec<&str> =
            m.actions.iter().filter(|a| a.is_read_only).map(|a| a.name.as_str()).collect();
        assert!(reads.contains(&"get_values"));
        assert!(reads.contains(&"get_metadata"));
    }

    #[test]
    fn debug_redacts_token() {
        let c = connector("http://localhost");
        let s = format!("{c:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("ya29.test_token"));
    }

    #[test]
    fn urlencode_handles_sheet_names_with_spaces_and_bangs() {
        // `Sales Q4!A1:D10` is a perfectly legal Sheets range. Real sheet
        // names contain spaces routinely; the connector must not break on them.
        let raw = "Sales Q4!A1:D10";
        let encoded = urlencode(raw);
        assert!(!encoded.contains(' '));
        assert!(!encoded.contains('!'));
        assert!(encoded.contains("%20"));
        assert!(encoded.contains("%21"));
    }

    #[test]
    fn urlencode_passes_through_safe_chars() {
        let raw = "abcXYZ123-_.~";
        assert_eq!(urlencode(raw), raw);
    }

    #[test]
    fn resolve_token_uses_metadata_first() {
        let env_default = SecretString::from("ya29.env");
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert(CRED_HEADER, "ya29.tenant".parse().unwrap());
        assert_eq!(resolve_token(&metadata, &env_default).expose_secret(), "ya29.tenant");
    }

    #[test]
    fn resolve_token_falls_back_to_env() {
        let env_default = SecretString::from("ya29.env");
        assert_eq!(
            resolve_token(&tonic::metadata::MetadataMap::new(), &env_default).expose_secret(),
            "ya29.env"
        );
    }
}
