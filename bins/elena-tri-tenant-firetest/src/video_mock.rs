//! Embedded `video_mock` plugin — Omnii's stand-in for the real video
//! generation API. Single action `render`; sleeps 200–800 ms before
//! responding to simulate generative latency under concurrency.
//!
//! Implements the gRPC `ElenaPlugin` trait so the blanket `Arc<T>:
//! EmbeddedExecutor` impl in elena-plugins picks it up; the registry
//! sees the same `tonic::Request` shape it would see from a remote
//! sidecar.

use elena_plugins::proto::pb;
use serde_json::json;
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

#[derive(Debug, Default, Clone)]
pub struct VideoMockConnector;

#[tonic::async_trait]
impl pb::elena_plugin_server::ElenaPlugin for VideoMockConnector {
    async fn get_manifest(&self, _: Request<()>) -> Result<Response<pb::PluginManifest>, Status> {
        Ok(Response::new(pb::PluginManifest {
            id: "video_mock".into(),
            name: "Video Mock (Omnii)".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            actions: vec![pb::ActionDefinition {
                name: "render".into(),
                description:
                    "Render a short video brief into a (mock) clip. Input: { \"brief\": \"...\" }. \
                     Output: { \"video_url\": \"...\", \"subtitles_added\": true, \"duration_ms\": N }."
                        .into(),
                input_schema:
                    r#"{"type":"object","properties":{"brief":{"type":"string","minLength":1}},"required":["brief"]}"#
                        .into(),
                output_schema:
                    r#"{"type":"object","properties":{"video_url":{"type":"string"},"subtitles_added":{"type":"boolean"},"duration_ms":{"type":"integer"}},"required":["video_url"]}"#
                        .into(),
                is_read_only: false,
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
        if inner.action != "render" {
            return Err(Status::invalid_argument(format!(
                "video_mock: unknown action {:?}",
                inner.action
            )));
        }
        let body: serde_json::Value = serde_json::from_slice(&inner.input_json)
            .map_err(|e| Status::invalid_argument(format!("input not JSON: {e}")))?;
        let brief =
            body.get("brief").and_then(|v| v.as_str()).unwrap_or("(empty brief)").to_owned();

        // Deterministic but spread out: 200..=800 ms based on brief
        // length. Avoids flaky races but still shows concurrency.
        let sleep_ms = 200 + (brief.len() as u64 % 600);

        let (tx, rx) = mpsc::channel::<Result<pb::PluginResponse, Status>>(8);
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Progress(pb::ProgressUpdate {
                        message: "shots".into(),
                        data_json: br#"{"step":"shots"}"#.to_vec(),
                    })),
                }))
                .await;
            tokio::time::sleep(Duration::from_millis(sleep_ms / 2)).await;
            let _ = tx
                .send(Ok(pb::PluginResponse {
                    payload: Some(pb::plugin_response::Payload::Progress(pb::ProgressUpdate {
                        message: "motion+subtitles".into(),
                        data_json: br#"{"step":"motion"}"#.to_vec(),
                    })),
                }))
                .await;
            tokio::time::sleep(Duration::from_millis(sleep_ms / 2)).await;

            let video_id = uuid::Uuid::new_v4();
            let body = serde_json::to_vec(&json!({
                "video_url": format!("https://mock.omnii/videos/{video_id}.mp4"),
                "subtitles_added": true,
                "duration_ms": sleep_ms,
                "brief_excerpt": brief.chars().take(40).collect::<String>(),
            }))
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
