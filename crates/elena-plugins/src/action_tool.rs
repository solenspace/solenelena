//! [`PluginActionTool`] — bridges one plugin action to the [`Tool`] trait.
//!
//! One [`PluginActionTool`] is synthesised per advertised
//! [`ActionDefinition`] at registration time and inserted into the shared
//! [`elena_tools::ToolRegistry`]. From the orchestrator's perspective it
//! looks identical to a built-in tool — the gRPC plumbing is hidden behind
//! [`Tool::execute`].

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use elena_tools::{Tool, ToolContext, ToolOutput};
use elena_types::{StreamEvent, ToolError, ToolResultContent};
use serde_json::Value;
use tonic::Code;
use tracing::{debug, warn};

use crate::{client::PluginClient, health::HEALTH_DOWN, id::PluginId, proto::pb};

/// F2 — maximum tool-result body size before truncation, in bytes.
/// 200 KiB is enough for typical paginated REST responses but small
/// enough that one runaway plugin can't blow the LLM context window.
/// Operators can tune this in v1.0.x via plugin manifest if needed.
pub const MAX_TOOL_RESULT_BYTES: usize = 200 * 1024;

/// Truncate a tool-result body to [`MAX_TOOL_RESULT_BYTES`], appending a
/// human-readable marker so the LLM (and humans reading audit rows)
/// know the result was clipped. Splits on a UTF-8 char boundary so the
/// result remains valid UTF-8.
#[must_use]
pub fn truncate_tool_result(body: String) -> (String, bool) {
    use std::fmt::Write as _;
    let original_len = body.len();
    if original_len <= MAX_TOOL_RESULT_BYTES {
        return (body, false);
    }
    // Find the highest char boundary <= MAX_TOOL_RESULT_BYTES so the
    // slice we keep is valid UTF-8.
    let mut cut = MAX_TOOL_RESULT_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut truncated = body;
    truncated.truncate(cut);
    let _ = write!(truncated, "\n\n[truncated, {cut} of {original_len} bytes]");
    (truncated, true)
}

/// One callable plugin action surfaced as a Rust [`Tool`].
#[derive(Debug)]
pub struct PluginActionTool {
    plugin_id: PluginId,
    tool_name: String,
    action_name: String,
    description: String,
    input_schema: Value,
    is_read_only: bool,
    client: Arc<PluginClient>,
    execute_timeout: Duration,
    health: Option<Arc<AtomicU8>>,
}

impl PluginActionTool {
    /// Build a new bridge for the given action.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        plugin_id: PluginId,
        action_name: String,
        description: String,
        input_schema: Value,
        is_read_only: bool,
        client: Arc<PluginClient>,
        execute_timeout: Duration,
        health: Option<Arc<AtomicU8>>,
    ) -> Self {
        let tool_name = format!("{plugin_id}_{action_name}");
        Self {
            plugin_id,
            tool_name,
            action_name,
            description,
            input_schema,
            is_read_only,
            client,
            execute_timeout,
            health,
        }
    }

    /// The synthesised tool name visible to the LLM (`{plugin_id}_{action}`).
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    fn map_status(&self, status: &tonic::Status) -> ToolError {
        if matches!(status.code(), Code::Unavailable | Code::DeadlineExceeded)
            && let Some(h) = &self.health
        {
            h.store(HEALTH_DOWN, Ordering::Relaxed);
        }
        match status.code() {
            Code::InvalidArgument => ToolError::InvalidInput {
                message: format!("plugin {} rejected input: {}", self.plugin_id, status.message()),
            },
            _ => ToolError::Execution {
                message: format!(
                    "plugin {}: {} ({})",
                    self.plugin_id,
                    status.message(),
                    status.code()
                ),
            },
        }
    }
}

#[async_trait]
impl Tool for PluginActionTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    fn is_read_only(&self, _: &Value) -> bool {
        self.is_read_only
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let request_body = pb::PluginRequest {
            action: self.action_name.clone(),
            input_json: serde_json::to_vec(&input)
                .map_err(|e| ToolError::InvalidInput { message: e.to_string() })?,
            tool_call_id: ctx.tool_call_id.to_string(),
            context: Some(pb::RequestContext {
                tenant_id: ctx.tenant.tenant_id.to_string(),
                thread_id: ctx.thread_id.to_string(),
            }),
        };
        let mut request = tonic::Request::new(request_body);
        // Per-tenant credentials decrypted by the loop driver flow into
        // the connector as `x-elena-cred-<key>` metadata. Connectors
        // prefer metadata over their startup env, so this is the seam
        // that makes multi-tenant connector deployments truly isolated.
        for (key, value) in &ctx.credentials {
            let header = format!("x-elena-cred-{key}");
            if let (Ok(name), Ok(val)) = (
                tonic::metadata::AsciiMetadataKey::from_bytes(header.as_bytes()),
                tonic::metadata::AsciiMetadataValue::try_from(value.as_str()),
            ) {
                request.metadata_mut().insert(name, val);
            } else {
                warn!(
                    plugin = %self.plugin_id, key = %key,
                    "skipping credential with non-ASCII key or value"
                );
            }
        }

        let mut rpc = self.client.rpc();
        let exec_call = async { rpc.execute(request).await.map(tonic::Response::into_inner) };

        // Race the call against ctx.cancel.
        let mut stream = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => {
                return Err(ToolError::Execution { message: "aborted".into() });
            }
            res = exec_call => res.map_err(|s| self.map_status(&s))?,
        };

        let timeout_at = tokio::time::sleep(self.execute_timeout);
        tokio::pin!(timeout_at);

        let mut final_output: Option<ToolOutput> = None;
        loop {
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    return Err(ToolError::Execution { message: "aborted".into() });
                }
                () = &mut timeout_at => {
                    let timeout_ms = u64::try_from(self.execute_timeout.as_millis()).unwrap_or(u64::MAX);
                    return Err(ToolError::Timeout { timeout_ms });
                }
                msg = stream.message() => match msg {
                    Ok(None) => break,
                    Ok(Some(resp)) => match resp.payload {
                        Some(pb::plugin_response::Payload::Progress(p)) => {
                            let data: Value = if p.data_json.is_empty() {
                                Value::Null
                            } else {
                                serde_json::from_slice(&p.data_json).unwrap_or(Value::Null)
                            };
                            let event = StreamEvent::ToolProgress {
                                id: ctx.tool_call_id,
                                message: p.message,
                                data,
                            };
                            // The receiver is allowed to disappear (client
                            // dropped the WS) — keep working in that case.
                            let _ = ctx.progress_tx.send(event).await;
                        }
                        Some(pb::plugin_response::Payload::Result(r)) => {
                            if final_output.is_some() {
                                debug!(plugin = %self.plugin_id, action = %self.action_name,
                                    "plugin emitted multiple FinalResult; using last");
                            }
                            let body = String::from_utf8(r.output_json).map_err(|e| {
                                ToolError::Execution {
                                    message: format!("plugin output is not UTF-8: {e}"),
                                }
                            })?;
                            let (body, was_truncated) = truncate_tool_result(body);
                            if was_truncated {
                                debug!(plugin = %self.plugin_id, action = %self.action_name,
                                    "tool result truncated to {MAX_TOOL_RESULT_BYTES} bytes");
                            }
                            final_output = Some(ToolOutput {
                                content: ToolResultContent::text(body),
                                is_error: r.is_error,
                            });
                        }
                        None => {
                            warn!(plugin = %self.plugin_id, action = %self.action_name,
                                "plugin sent PluginResponse without payload; ignoring");
                        }
                    },
                    Err(status) => return Err(self.map_status(&status)),
                }
            }
        }

        final_output.ok_or_else(|| ToolError::Execution {
            message: format!(
                "plugin {} action {} closed stream without a final result",
                self.plugin_id, self.action_name
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;

    use elena_types::{
        BudgetLimits, PermissionSet, SessionId, TenantContext, TenantId, TenantTier, ThreadId,
        ToolCallId, UserId, WorkspaceId,
    };
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tonic::{Request, Response, Status, transport::Server};

    use super::*;

    // ----- a configurable fixture connector -------------------------

    #[derive(Debug, Default, Clone)]
    #[allow(clippy::struct_excessive_bools)] // test fixture toggles, not a real type
    struct EchoServer {
        // If Some, the server returns this status from execute() instead of
        // streaming.
        force_status: Option<tonic::Code>,
        // If true, the server streams two ProgressUpdates before the final
        // result; otherwise it streams one final result with the reversed input.
        emit_progress: bool,
        // If true, the server holds the call open forever (for cancel/timeout
        // tests).
        hang: bool,
        // If true, close stream without ever emitting a FinalResult.
        no_result: bool,
        // If true, emit a FinalResult whose output_json is invalid UTF-8.
        // Exercises C6.3 — plugin returns garbage bytes; the orchestrator
        // must surface ToolError::Execution rather than crash or hand the
        // bytes to the LLM.
        corrupt_utf8: bool,
    }

    #[tonic::async_trait]
    impl pb::elena_plugin_server::ElenaPlugin for EchoServer {
        async fn get_manifest(
            &self,
            _: Request<()>,
        ) -> Result<Response<pb::PluginManifest>, Status> {
            Ok(Response::new(pb::PluginManifest::default()))
        }

        type ExecuteStream =
            tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

        async fn execute(
            &self,
            req: Request<pb::PluginRequest>,
        ) -> Result<Response<Self::ExecuteStream>, Status> {
            if let Some(code) = self.force_status {
                return Err(Status::new(code, "forced"));
            }
            let body: serde_json::Value =
                serde_json::from_slice(&req.into_inner().input_json).unwrap_or(Value::Null);
            let word = body.get("word").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let reversed: String = word.chars().rev().collect();

            let (tx, rx) = mpsc::channel(8);
            let s = self.clone();
            tokio::spawn(async move {
                if s.hang {
                    futures::future::pending::<()>().await;
                    return;
                }
                if s.emit_progress {
                    let _ = tx
                        .send(Ok(pb::PluginResponse {
                            payload: Some(pb::plugin_response::Payload::Progress(
                                pb::ProgressUpdate {
                                    message: "starting".into(),
                                    data_json: br#"{"step":0}"#.to_vec(),
                                },
                            )),
                        }))
                        .await;
                    let _ = tx
                        .send(Ok(pb::PluginResponse {
                            payload: Some(pb::plugin_response::Payload::Progress(
                                pb::ProgressUpdate {
                                    message: "halfway".into(),
                                    data_json: br#"{"step":1}"#.to_vec(),
                                },
                            )),
                        }))
                        .await;
                }
                if !s.no_result {
                    let result_body = if s.corrupt_utf8 {
                        // 0xFF, 0xFE, 0xFD are invalid as UTF-8 start bytes.
                        vec![0xFF, 0xFE, 0xFD, b'g', b'a', b'r', b'b']
                    } else {
                        serde_json::to_vec(&serde_json::json!({"reversed": reversed})).unwrap()
                    };
                    let _ = tx
                        .send(Ok(pb::PluginResponse {
                            payload: Some(pb::plugin_response::Payload::Result(pb::FinalResult {
                                output_json: result_body,
                                is_error: false,
                            })),
                        }))
                        .await;
                }
            });
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
            Ok(Response::new(pb::HealthResponse { ok: true, detail: String::new() }))
        }
    }

    async fn spawn_with(server: EchoServer) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = Server::builder()
                .add_service(pb::elena_plugin_server::ElenaPluginServer::new(server))
                .serve_with_incoming(incoming)
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (addr, handle)
    }

    fn make_ctx() -> (ToolContext, mpsc::Receiver<StreamEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let tenant = TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::default(),
            tier: TenantTier::Pro,
            metadata: HashMap::new(),
        };
        let ctx = ToolContext {
            thread_id: tenant.thread_id,
            tenant,
            tool_call_id: ToolCallId::new(),
            cancel: CancellationToken::new(),
            progress_tx: tx,
            credentials: std::collections::BTreeMap::new(),
        };
        (ctx, rx)
    }

    fn build_tool(client: Arc<PluginClient>, timeout: Duration) -> PluginActionTool {
        PluginActionTool::new(
            PluginId::new("echo").unwrap(),
            "reverse".into(),
            "Reverses a string.".into(),
            serde_json::json!({"type":"object","properties":{"word":{"type":"string"}}}),
            true,
            client,
            timeout,
            None,
        )
    }

    // ----- tests ----------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn happy_path_returns_reversed() {
        let (addr, srv) = spawn_with(EchoServer::default()).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_secs(5));
        let (ctx, _rx) = make_ctx();

        let out = tool.execute(serde_json::json!({"word":"hello"}), ctx).await.unwrap();

        assert!(!out.is_error);
        assert_eq!(tool.tool_name(), "echo_reverse");
        let body = match &out.content {
            ToolResultContent::Text(text) => text.clone(),
            other @ ToolResultContent::Blocks(_) => panic!("expected text content, got {other:?}"),
        };
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["reversed"], "olleh");

        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_updates_fan_out_via_progress_tx() {
        let server = EchoServer { emit_progress: true, ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_secs(5));
        let (ctx, mut rx) = make_ctx();

        let out = tool.execute(serde_json::json!({"word":"hi"}), ctx).await.unwrap();
        assert!(!out.is_error);

        // Drain progress events.
        let mut progress_msgs = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let StreamEvent::ToolProgress { message, .. } = ev {
                progress_msgs.push(message);
            }
        }
        assert_eq!(progress_msgs, vec!["starting".to_owned(), "halfway".to_owned()]);

        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_returns_aborted() {
        let server = EchoServer { hang: true, ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_secs(60));
        let (ctx, _rx) = make_ctx();

        // Cancel after a short delay.
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });

        let err = tool.execute(serde_json::json!({"word":"x"}), ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution { message } if message == "aborted"));

        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_returns_timeout_error() {
        let server = EchoServer { hang: true, ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_millis(100));
        let (ctx, _rx) = make_ctx();

        let err = tool.execute(serde_json::json!({"word":"x"}), ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Timeout { timeout_ms: 100 }));

        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unavailable_status_flips_health_gauge_when_present() {
        let server = EchoServer { force_status: Some(Code::Unavailable), ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let health = Arc::new(AtomicU8::new(crate::health::HEALTH_UP));
        let mut tool = build_tool(client, Duration::from_secs(2));
        tool.health = Some(Arc::clone(&health));
        let (ctx, _rx) = make_ctx();

        let err = tool.execute(serde_json::json!({"word":"x"}), ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution { .. }));
        assert_eq!(health.load(Ordering::Relaxed), crate::health::HEALTH_DOWN);

        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_argument_status_maps_to_invalid_input() {
        let server = EchoServer { force_status: Some(Code::InvalidArgument), ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_secs(2));
        let (ctx, _rx) = make_ctx();

        let err = tool.execute(serde_json::json!({"word":"x"}), ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput { .. }));

        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_without_final_result_errors_cleanly() {
        let server = EchoServer { no_result: true, ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_secs(2));
        let (ctx, _rx) = make_ctx();

        let err = tool.execute(serde_json::json!({"word":"x"}), ctx).await.unwrap_err();
        match err {
            ToolError::Execution { message } => {
                assert!(message.contains("closed stream without a final result"), "got: {message}");
            }
            other => panic!("unexpected: {other:?}"),
        }

        srv.abort();
    }

    // ----- F2: tool-result truncation -----

    #[test]
    fn f2_below_cap_passes_through_verbatim() {
        let body = "x".repeat(1_024);
        let (out, truncated) = truncate_tool_result(body.clone());
        assert!(!truncated);
        assert_eq!(out, body);
    }

    #[test]
    fn f2_at_cap_still_passes_through() {
        let body = "y".repeat(MAX_TOOL_RESULT_BYTES);
        let (out, truncated) = truncate_tool_result(body.clone());
        assert!(!truncated);
        assert_eq!(out.len(), body.len());
    }

    #[test]
    fn f2_over_cap_gets_truncated_with_marker() {
        let body = "z".repeat(MAX_TOOL_RESULT_BYTES + 100);
        let (out, truncated) = truncate_tool_result(body);
        assert!(truncated);
        assert!(
            out.ends_with(&format!(
                "[truncated, {MAX_TOOL_RESULT_BYTES} of {} bytes]",
                MAX_TOOL_RESULT_BYTES + 100
            )),
            "expected truncation marker, got tail: {}",
            &out[out.len().saturating_sub(60)..]
        );
        // Kept prefix is the full cap, plus the marker.
        assert!(out.len() > MAX_TOOL_RESULT_BYTES);
    }

    #[test]
    fn f2_truncation_respects_utf8_char_boundary() {
        // Build a body whose byte length puts the cap mid-char.
        // Emoji is 4 bytes; repeating it ensures many boundary candidates.
        let one_char = "💾";
        let body_len_target = MAX_TOOL_RESULT_BYTES + one_char.len() + 1;
        let repeats = body_len_target / one_char.len() + 1;
        let body = one_char.repeat(repeats);
        assert!(body.len() > MAX_TOOL_RESULT_BYTES);
        let (out, truncated) = truncate_tool_result(body);
        assert!(truncated);
        // The output must be valid UTF-8 (push_str would have panicked
        // if we cut mid-char, but assert to make the contract explicit).
        assert!(out.chars().count() > 0);
    }

    /// C6.3 — A plugin returning non-UTF-8 bytes in `output_json` must
    /// surface as a clean `ToolError::Execution`, not panic, not get
    /// silently fed to the LLM as garbage. The orchestrator already
    /// validates UTF-8 at the boundary; this test pins that contract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn c63_corrupt_utf8_output_surfaces_as_execution_error() {
        let server = EchoServer { corrupt_utf8: true, ..Default::default() };
        let (addr, srv) = spawn_with(server).await;
        let client = Arc::new(
            PluginClient::connect(&format!("http://{addr}"), Duration::from_secs(2)).await.unwrap(),
        );
        let tool = build_tool(client, Duration::from_secs(2));
        let (ctx, _rx) = make_ctx();

        let err = tool.execute(serde_json::json!({"word": "x"}), ctx).await.unwrap_err();
        match err {
            ToolError::Execution { message } => {
                assert!(
                    message.contains("not UTF-8") || message.contains("UTF-8"),
                    "expected UTF-8 error message, got: {message}"
                );
            }
            other => panic!("expected ToolError::Execution for non-UTF-8 output, got: {other:?}"),
        }

        srv.abort();
    }
}
