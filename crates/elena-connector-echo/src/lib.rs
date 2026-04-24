//! Reusable plugin sidecar logic for tests + the standalone binary.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use elena_plugins::proto::pb;
use serde_json::Value;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

/// gRPC service implementing one action: `reverse`.
#[derive(Debug, Default, Clone)]
pub struct EchoConnector;

#[tonic::async_trait]
impl pb::elena_plugin_server::ElenaPlugin for EchoConnector {
    async fn get_manifest(&self, _: Request<()>) -> Result<Response<pb::PluginManifest>, Status> {
        Ok(Response::new(pb::PluginManifest {
            id: "echo".into(),
            name: "Echo".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            actions: vec![pb::ActionDefinition {
                name: "reverse".into(),
                description:
                    "Reverse a string. Input: { \"word\": \"...\" }. Output: { \"reversed\": \"...\" }."
                        .into(),
                input_schema:
                    r#"{"type":"object","properties":{"word":{"type":"string","minLength":1}},"required":["word"]}"#
                        .into(),
                output_schema:
                    r#"{"type":"object","properties":{"reversed":{"type":"string"}},"required":["reversed"]}"#
                        .into(),
                is_read_only: true,
            }],
            required_credentials: vec![],
        }))
    }

    type ExecuteStream = tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

    async fn execute(
        &self,
        req: Request<pb::PluginRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let inner = req.into_inner();
        if inner.action != "reverse" {
            return Err(Status::invalid_argument(format!("unknown action {:?}", inner.action)));
        }
        let body: Value = serde_json::from_slice(&inner.input_json)
            .map_err(|e| Status::invalid_argument(format!("input is not valid JSON: {e}")))?;
        let word = body
            .get("word")
            .and_then(Value::as_str)
            .ok_or_else(|| Status::invalid_argument("missing required field: word"))?
            .to_owned();
        let reversed: String = word.chars().rev().collect();

        let (tx, rx) = mpsc::channel::<Result<pb::PluginResponse, Status>>(8);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Progress(pb::ProgressUpdate {
                        message: "echoing".into(),
                        data_json: br#"{"step":"start"}"#.to_vec(),
                    })),
                }))
                .await;
            let _ = tx
                .send(Ok(pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Progress(pb::ProgressUpdate {
                        message: "halfway".into(),
                        data_json: br#"{"step":"midway"}"#.to_vec(),
                    })),
                }))
                .await;
            let body = serde_json::to_vec(&serde_json::json!({ "reversed": reversed }))
                .unwrap_or_default();
            let _ = tx
                .send(Ok(pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Result(pb::FinalResult {
                        output_json: body,
                        is_error: false,
                    })),
                }))
                .await;
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
        Ok(Response::new(pb::HealthResponse { ok: true, detail: "ready".into() }))
    }
}
