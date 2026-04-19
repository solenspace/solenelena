//! Elena Shopify connector — gRPC plugin sidecar.
//!
//! Read + write actions against a Shopify store's Admin REST API.
//! Application-agnostic: any Elena workflow that needs to query orders
//! or update products.
//!
//! Three actions:
//!
//! - **`list_orders`** (read) — `GET /admin/api/2024-04/orders.json`.
//!   Lists up to 50 orders, optionally filtered by `status` and a
//!   `created_at_min` ISO-8601 timestamp ("today's orders" pattern).
//!   Returns `{ orders: [{id, name, total_price, currency, created_at,
//!   financial_status}] }`.
//! - **`get_order`** (read) — `GET .../orders/{id}.json`. Returns the
//!   slim order shape above for one id.
//! - **`update_order_note`** (write) — `PUT .../orders/{id}.json` with
//!   `{order: {note}}`. Used by Solen workflows that flag orders for
//!   follow-up. Returns `{ ok, id }`.
//!
//! Auth: a custom-app admin access token from `SHOPIFY_ADMIN_TOKEN`,
//! sent as the `X-Shopify-Access-Token` header. Per-tenant credential
//! injection lands in v1.0.x.
//!
//! The store domain comes from `SHOPIFY_SHOP_DOMAIN` (e.g.
//! `mystore.myshopify.com`). For wiremock integration tests, set
//! `SHOPIFY_API_BASE_URL` to override the constructed `https://{shop}`
//! base entirely.

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

const API_VERSION: &str = "2024-04";
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Per-tenant Shopify admin token metadata key.
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

/// gRPC service implementing Shopify actions.
#[derive(Clone)]
pub struct ShopifyConnector {
    api_base: String,
    admin_token: SecretString,
    http: reqwest::Client,
}

impl std::fmt::Debug for ShopifyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShopifyConnector")
            .field("api_base", &self.api_base)
            .field("admin_token", &"<redacted>")
            .finish()
    }
}

impl ShopifyConnector {
    /// Build a connector pointing at a Shopify store.
    /// `api_base` is `https://{shop}.myshopify.com` — the connector
    /// appends the `/admin/api/{version}/...` path itself.
    #[must_use]
    pub fn new(api_base: impl Into<String>, admin_token: SecretString) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_base: api_base.into(), admin_token, http }
    }

    /// Build from environment.
    pub fn from_env() -> anyhow::Result<Self> {
        let token = std::env::var("SHOPIFY_ADMIN_TOKEN")
            .map_err(|_| anyhow::anyhow!("SHOPIFY_ADMIN_TOKEN is required"))?;
        let base = if let Ok(direct) = std::env::var("SHOPIFY_API_BASE_URL") {
            direct
        } else {
            let shop = std::env::var("SHOPIFY_SHOP_DOMAIN").map_err(|_| {
                anyhow::anyhow!("either SHOPIFY_API_BASE_URL or SHOPIFY_SHOP_DOMAIN must be set")
            })?;
            format!("https://{shop}")
        };
        Ok(Self::new(base, SecretString::from(token)))
    }
}

#[tonic::async_trait]
impl pb::elena_plugin_server::ElenaPlugin for ShopifyConnector {
    async fn get_manifest(&self, _: Request<()>) -> Result<Response<pb::PluginManifest>, Status> {
        Ok(Response::new(pb::PluginManifest {
            id: "shopify".into(),
            name: "Shopify".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            actions: vec![
                pb::ActionDefinition {
                    name: "list_orders".into(),
                    description: "List up to 50 orders. Input: optional { \
                                  status: any|open|closed|cancelled, \
                                  created_at_min: ISO-8601 timestamp }. \
                                  Output: { orders: [...] }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "status":         {"type": "string", "enum": ["any","open","closed","cancelled"]},
                            "created_at_min": {"type": "string"}
                        }
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {"orders": {"type": "array"}},
                        "required": ["orders"]
                    }"#
                    .into(),
                    is_read_only: true,
                },
                pb::ActionDefinition {
                    name: "get_order".into(),
                    description: "Fetch one order by id. Output is the same \
                                  slim shape as list_orders but as a single \
                                  object."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "id": {"type": "integer", "minimum": 1}
                        },
                        "required": ["id"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "order": {"type": "object"}
                        }
                    }"#
                    .into(),
                    is_read_only: true,
                },
                pb::ActionDefinition {
                    name: "update_order_note".into(),
                    description: "Set the operator-visible note on an order. \
                                  Input: { id, note }. Output: { ok, id }."
                        .into(),
                    input_schema: r#"{
                        "type": "object",
                        "properties": {
                            "id":   {"type": "integer", "minimum": 1},
                            "note": {"type": "string", "maxLength": 5000}
                        },
                        "required": ["id", "note"]
                    }"#
                    .into(),
                    output_schema: r#"{
                        "type": "object",
                        "properties": {
                            "ok": {"type": "boolean"},
                            "id": {"type": "integer"}
                        },
                        "required": ["ok"]
                    }"#
                    .into(),
                    is_read_only: false,
                },
            ],
            required_credentials: vec![
                "SHOPIFY_ADMIN_TOKEN".into(),
                "read_orders".into(),
                "write_orders".into(),
            ],
        }))
    }

    type ExecuteStream = tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

    async fn execute(
        &self,
        req: Request<pb::PluginRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let token = resolve_token(req.metadata(), &self.admin_token);
        let inner = req.into_inner();
        let body: Value = serde_json::from_slice(&inner.input_json)
            .map_err(|e| Status::invalid_argument(format!("input is not valid JSON: {e}")))?;

        let (tx, rx) = mpsc::channel::<Result<pb::PluginResponse, Status>>(8);
        let me = self.clone();
        tokio::spawn(async move {
            let result = match inner.action.as_str() {
                "list_orders" => me.list_orders(body, &token).await,
                "get_order" => me.get_order(body, &token).await,
                "update_order_note" => me.update_order_note(body, &token).await,
                other => Err(ShopifyError::Unknown(other.to_owned())),
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
        // Cheapest auth probe: GET /shop.json. Returns the store object
        // when the admin token is valid. Uses the env-default token —
        // per-tenant tokens never route through the health probe.
        let url = format!("{}/admin/api/{API_VERSION}/shop.json", self.api_base);
        match self.http.get(&url).header("X-Shopify-Access-Token", self.env_token()).send().await {
            Ok(resp) if resp.status().is_success() => {
                Ok(Response::new(pb::HealthResponse { ok: true, detail: "shop.json ok".into() }))
            }
            Ok(resp) => Ok(Response::new(pb::HealthResponse {
                ok: false,
                detail: format!("shop.json returned {}", resp.status()),
            })),
            Err(e) => {
                warn!(?e, "shopify health probe failed");
                Ok(Response::new(pb::HealthResponse {
                    ok: false,
                    detail: format!("shop.json transport error: {e}"),
                }))
            }
        }
    }
}

impl ShopifyConnector {
    fn env_token(&self) -> String {
        self.admin_token.expose_secret().to_string()
    }

    async fn list_orders(
        &self,
        input: Value,
        token: &SecretString,
    ) -> Result<String, ShopifyError> {
        use std::fmt::Write as _;
        let mut url = format!("{}/admin/api/{API_VERSION}/orders.json?limit=50", self.api_base);
        if let Some(status) = input.get("status").and_then(Value::as_str) {
            let _ = write!(url, "&status={status}");
        }
        if let Some(min) = input.get("created_at_min").and_then(Value::as_str) {
            let _ = write!(url, "&created_at_min={}", urlencode(min));
        }
        let resp = self
            .http
            .get(&url)
            .header("X-Shopify-Access-Token", token.expose_secret())
            .send()
            .await
            .map_err(|e| ShopifyError::Transport(e.to_string()))?;
        let body = parse_shopify_response(resp).await?;
        let orders = body
            .get("orders")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().map(slim_order).collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(serde_json::json!({"orders": orders}).to_string())
    }

    async fn get_order(&self, input: Value, token: &SecretString) -> Result<String, ShopifyError> {
        let id = input
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ShopifyError::BadInput("id is required (integer)".into()))?;
        let url = format!("{}/admin/api/{API_VERSION}/orders/{id}.json", self.api_base);
        let resp = self
            .http
            .get(&url)
            .header("X-Shopify-Access-Token", token.expose_secret())
            .send()
            .await
            .map_err(|e| ShopifyError::Transport(e.to_string()))?;
        let body = parse_shopify_response(resp).await?;
        let order = body.get("order").map_or(Value::Null, slim_order);
        Ok(serde_json::json!({"order": order}).to_string())
    }

    async fn update_order_note(
        &self,
        input: Value,
        token: &SecretString,
    ) -> Result<String, ShopifyError> {
        let id = input
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ShopifyError::BadInput("id is required (integer)".into()))?;
        let note = input
            .get("note")
            .and_then(Value::as_str)
            .ok_or_else(|| ShopifyError::BadInput("note is required".into()))?;

        let url = format!("{}/admin/api/{API_VERSION}/orders/{id}.json", self.api_base);
        let body = serde_json::json!({"order": {"id": id, "note": note}});
        let resp = self
            .http
            .put(&url)
            .header("X-Shopify-Access-Token", token.expose_secret())
            .json(&body)
            .send()
            .await
            .map_err(|e| ShopifyError::Transport(e.to_string()))?;
        let _ = parse_shopify_response(resp).await?;
        Ok(serde_json::json!({"ok": true, "id": id}).to_string())
    }
}

async fn parse_shopify_response(resp: reqwest::Response) -> Result<Value, ShopifyError> {
    let status = resp.status();
    let body: Value =
        resp.json().await.map_err(|e| ShopifyError::Transport(format!("decode: {e}")))?;
    if !status.is_success() {
        let msg = body.get("errors").map_or_else(|| "unknown".into(), ToString::to_string);
        return Err(ShopifyError::Api(format!("HTTP {status}: {msg}")));
    }
    Ok(body)
}

/// Pull a small, LLM-friendly subset of order fields. Avoids dumping
/// the entire ~50KB order object into the model's context window.
fn slim_order(order: &Value) -> Value {
    serde_json::json!({
        "id":               order.get("id").cloned().unwrap_or(Value::Null),
        "name":             order.get("name").cloned().unwrap_or(Value::Null),
        "total_price":      order.get("total_price").cloned().unwrap_or(Value::Null),
        "currency":         order.get("currency").cloned().unwrap_or(Value::Null),
        "created_at":       order.get("created_at").cloned().unwrap_or(Value::Null),
        "financial_status": order.get("financial_status").cloned().unwrap_or(Value::Null),
    })
}

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
enum ShopifyError {
    #[error("invalid input: {0}")]
    BadInput(String),
    #[error("shopify api error: {0}")]
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

    fn connector(base: &str) -> ShopifyConnector {
        ShopifyConnector::new(base, SecretString::from("shpat_test_token"))
    }

    #[tokio::test]
    async fn manifest_advertises_three_actions_with_correct_read_only_bits() {
        let c = connector("http://localhost");
        let m = c.get_manifest(tonic::Request::new(())).await.unwrap().into_inner();
        assert_eq!(m.id, "shopify");
        assert_eq!(m.actions.len(), 3);
        let writes: Vec<&str> =
            m.actions.iter().filter(|a| !a.is_read_only).map(|a| a.name.as_str()).collect();
        assert_eq!(writes, vec!["update_order_note"]);
        let reads: Vec<&str> =
            m.actions.iter().filter(|a| a.is_read_only).map(|a| a.name.as_str()).collect();
        assert!(reads.contains(&"list_orders"));
        assert!(reads.contains(&"get_order"));
    }

    #[test]
    fn debug_redacts_token() {
        let c = connector("http://localhost");
        let s = format!("{c:?}");
        assert!(s.contains("<redacted>"));
        assert!(!s.contains("shpat_test_token"));
    }

    #[test]
    fn resolve_token_uses_metadata_when_present() {
        let env_default = SecretString::from("env-shpat");
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert(CRED_HEADER, "tenant-shpat".parse().unwrap());
        assert_eq!(resolve_token(&metadata, &env_default).expose_secret(), "tenant-shpat");
    }

    #[test]
    fn resolve_token_falls_back_to_env() {
        let env_default = SecretString::from("env-shpat");
        assert_eq!(
            resolve_token(&tonic::metadata::MetadataMap::new(), &env_default).expose_secret(),
            "env-shpat"
        );
    }

    #[test]
    fn slim_order_drops_unwanted_fields() {
        // Real Shopify order objects are ~50KB. We only forward the few
        // fields the LLM cares about for the Solen "today's sales"
        // pattern — make sure unrelated fields don't leak.
        let raw = serde_json::json!({
            "id": 4242,
            "name": "#1001",
            "total_price": "99.95",
            "currency": "USD",
            "created_at": "2026-04-19T12:00:00Z",
            "financial_status": "paid",
            "customer": {"email": "leak@example.com"},
            "line_items": [{"sku": "leak-sku"}]
        });
        let slim = slim_order(&raw);
        assert_eq!(slim["id"], 4242);
        assert_eq!(slim["total_price"], "99.95");
        assert!(slim.get("customer").is_none(), "must not leak customer fields");
        assert!(slim.get("line_items").is_none(), "must not leak line_items");
    }
}
