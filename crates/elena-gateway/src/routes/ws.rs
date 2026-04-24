//! `GET /v1/threads/:id/stream` — WebSocket upgrade for live turn streaming.

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use chrono::Utc;
use elena_types::{
    AutonomyMode, ContentBlock, Message, MessageId, MessageKind, ModelId, RequestId, Role,
    StreamEnvelope, StreamEvent, ThreadId, WorkRequest, WorkRequestKind, subjects,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::app::GatewayState;
use crate::auth::AuthedTenant;
use crate::error::GatewayError;
use crate::nats::publish_work;

/// Client → server WebSocket frame.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Send a new user message and start a turn.
    SendMessage {
        /// User text body.
        text: String,
        /// Optional explicit model override; otherwise the router picks.
        #[serde(default)]
        model: Option<ModelId>,
        /// Optional max-turn cap override.
        #[serde(default)]
        max_turns: Option<u32>,
        /// Autonomy policy for this turn. Defaults to Moderate if the
        /// client omits it.
        #[serde(default)]
        autonomy: AutonomyMode,
    },
    /// Cancel any in-flight turn for this thread.
    Abort,
}

/// Lightweight reply shape for non-`StreamEvent` messages from the gateway
/// (e.g., status acks). The actual event stream uses
/// [`StreamEvent`](elena_types::StreamEvent)'s tagged JSON directly.
#[derive(Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum GatewayFrame {
    /// Acknowledges the WebSocket upgrade. Sent once before any model events.
    SessionStarted {
        /// The thread the session is bound to.
        thread_id: ThreadId,
    },
}

/// X4 — Query params on the WS upgrade.
#[derive(Debug, Default, Deserialize)]
pub struct StreamQuery {
    /// Last `StreamEnvelope.offset` the client successfully processed.
    /// On reconnect the gateway replays persisted messages newer than
    /// the corresponding point in the thread before joining the live
    /// stream. Omitted on first-ever connect — clients have nothing
    /// to replay.
    #[serde(default)]
    pub since: Option<u64>,
}

/// HTTP handler: upgrade `GET /v1/threads/:id/stream` to a WebSocket.
pub async fn ws_upgrade(
    State(state): State<GatewayState>,
    AuthedTenant(tenant): AuthedTenant,
    Path(thread_id): Path<ThreadId>,
    Query(query): Query<StreamQuery>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<impl IntoResponse, GatewayError> {
    let since = query.since;
    // RFC 6455 §4.2.2: when the client sends Sec-WebSocket-Protocol the
    // server MUST echo back the agreed subprotocol or the upgrade fails
    // strict clients (e.g. node-ws). Echo whichever `elena.bearer.<jwt>`
    // value was sent so the JWT-via-subprotocol auth path completes the
    // handshake cleanly.
    let upgrade = if let Some(matched) = headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(',').map(str::trim).find(|p| p.starts_with("elena.bearer."))
        })
        .map(str::to_owned)
    {
        upgrade.protocols([matched])
    } else {
        upgrade
    };
    Ok(upgrade.on_upgrade(move |socket| handle_socket(state, tenant, thread_id, since, socket)))
}

async fn handle_socket(
    state: GatewayState,
    tenant: elena_types::TenantContext,
    thread_id: ThreadId,
    since: Option<u64>,
    socket: WebSocket,
) {
    if let Err(e) = run_socket(state, tenant, thread_id, since, socket).await {
        warn!(?e, %thread_id, "ws session ended with error");
    }
}

async fn run_socket(
    state: GatewayState,
    tenant: elena_types::TenantContext,
    thread_id: ThreadId,
    since: Option<u64>,
    socket: WebSocket,
) -> Result<(), GatewayError> {
    info!(%thread_id, tenant = %tenant.tenant_id, since = ?since, "ws session opened");

    let (mut sink, mut stream) = socket.split();

    // Send the session-started ack.
    let ack = serde_json::to_string(&GatewayFrame::SessionStarted { thread_id })
        .map_err(|e| GatewayError::Internal(format!("ack serde: {e}")))?;
    sink.send(WsMessage::Text(ack.into())).await.ok();

    // X1 — subscribe to the local broadcast channel BEFORE replay so
    // any live events published while we're reading Postgres are
    // buffered in the broadcast channel and we send them after the
    // replay batch. Subscribing first guarantees no live event is
    // missed in the gap between replay-fetch and live-forward.
    let mut events = crate::fanout::subscribe(&state.fanout, thread_id);

    // X4 — Postgres replay. If the client provided `?since=<offset>`,
    // synthesize replay envelopes from every persisted message in this
    // thread newer than the message at-or-before `since`. Replay
    // envelopes carry `offset = REPLAY_OFFSET (0)` so the client can
    // distinguish them from live events.
    if let Some(since) = since {
        if let Err(e) = replay_since(&state, &tenant, thread_id, since, &mut sink).await {
            warn!(?e, %thread_id, "replay failed; continuing with live stream only");
        }
    }

    loop {
        tokio::select! {
            // Client → server frame.
            client_msg = stream.next() => {
                let Some(client_msg) = client_msg else { break; };
                match client_msg {
                    Ok(WsMessage::Text(text)) => {
                        match serde_json::from_str::<ClientFrame>(&text) {
                            Ok(frame) => {
                                if let Err(e) = handle_client_frame(&state, &tenant, thread_id, frame).await {
                                    warn!(?e, "client frame failed");
                                    let _ = sink.send(error_frame(&e)).await;
                                }
                            }
                            Err(e) => {
                                warn!(?e, raw = %text, "unparseable client frame");
                                let _ = sink
                                    .send(error_frame(&GatewayError::BadRequest {
                                        message: format!("invalid frame: {e}"),
                                    }))
                                    .await;
                            }
                        }
                    }
                    Ok(WsMessage::Close(_)) => break,
                    Ok(_) => {} // ignore binary / ping / pong / etc.
                    Err(e) => {
                        debug!(?e, "ws transport error");
                        break;
                    }
                }
            }
            // Worker → gateway event (via local broadcast).
            event = events.recv() => {
                let payload = match event {
                    Ok(p) => p,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Slow consumer; broadcast dropped n messages
                        // for us. Mirror the pre-X1 backpressure
                        // posture: the client missed events, close
                        // the session and let them reconnect.
                        warn!(%thread_id, dropped = n, "ws subscriber lagged; closing session");
                        break;
                    }
                };
                // Backpressure: a slow client shouldn't stall the whole
                // worker fan-out. 500 ms is generous for a well-behaved
                // client on any realistic link; anything slower is blocking
                // and should be dropped.
                let payload_str = String::from_utf8_lossy(&payload).into_owned();
                let send = sink.send(WsMessage::Text(payload_str.into()));
                match tokio::time::timeout(std::time::Duration::from_millis(500), send).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        debug!("ws sink closed by client");
                        break;
                    }
                    Err(_) => {
                        warn!("ws client backpressure stall; closing session");
                        break;
                    }
                }
            }
        }
    }

    // Drop the receiver before we ask the table to garbage-collect the
    // entry — `maybe_release` only reaps when receiver_count == 0.
    drop(events);
    crate::fanout::maybe_release(&state.fanout, thread_id);
    info!(%thread_id, "ws session closed");
    Ok(())
}

async fn handle_client_frame(
    state: &GatewayState,
    tenant: &elena_types::TenantContext,
    thread_id: ThreadId,
    frame: ClientFrame,
) -> Result<(), GatewayError> {
    match frame {
        ClientFrame::SendMessage { text, model, max_turns, autonomy } => {
            // Inject the workspace's global instructions as a system
            // block. Missing workspace row, empty fragment, or DB error
            // all degrade to "no guardrail" — we don't want an admin-API
            // outage to stall user traffic.
            let workspace_fragment = match state
                .store
                .workspaces
                .instructions(tenant.tenant_id, tenant.workspace_id)
                .await
            {
                Ok(fragment) if !fragment.is_empty() => Some(fragment),
                Ok(_) => None,
                Err(e) => {
                    warn!(?e, "workspace instructions load failed; continuing without");
                    None
                }
            };
            let req = build_send_message_request(
                tenant,
                thread_id,
                text,
                model,
                max_turns,
                autonomy,
                workspace_fragment,
            );
            publish_work(&state.jet, &req).await
        }
        ClientFrame::Abort => {
            state
                .nats
                .publish(subjects::thread_abort(thread_id), Vec::new().into())
                .await
                .map_err(|e| GatewayError::Nats(format!("abort publish: {e}")))?;
            Ok(())
        }
    }
}

/// X4 — Replay every persisted message in the thread (synthetic
/// envelopes with `offset = REPLAY_OFFSET = 0`) and forward them to
/// the client.
///
/// This is intentionally **coarse**: we don't have a per-event index
/// in Postgres, only per-message rows. So `since` is treated as a
/// "client missed something" trigger rather than a precise resume
/// point — we replay the full message list. Most clients use this on
/// reconnect; doing the full pass once costs no more than the
/// existing `GET /v1/threads/{id}/messages` bootstrap path.
///
/// Each persisted Message becomes a small synthetic event sequence
/// (`MessageStart` + `ToolUseComplete` or `ToolResult` per content block)
/// that the renderer can apply incrementally — same shape the live
/// stream emits, just with offset = 0.
async fn replay_since(
    state: &GatewayState,
    tenant: &elena_types::TenantContext,
    thread_id: ThreadId,
    since: u64,
    sink: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
) -> Result<(), GatewayError> {
    let msgs = state
        .store
        .threads
        .list_messages(tenant.tenant_id, thread_id, 1_000, None)
        .await
        .map_err(GatewayError::Store)?;
    info!(%thread_id, since, replayed = msgs.len(), "x4 replay starting");
    for msg in msgs {
        for ev in synthesize_events(&msg) {
            let envelope = StreamEnvelope::replay(ev);
            let payload = match serde_json::to_string(&envelope) {
                Ok(p) => p,
                Err(e) => {
                    warn!(?e, "replay serde failed");
                    continue;
                }
            };
            if sink.send(WsMessage::Text(payload.into())).await.is_err() {
                debug!(%thread_id, "client closed socket during replay");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// X4 — Project a persisted [`Message`] back into a small
/// `StreamEvent` sequence the renderer can apply. Best-effort: we
/// emit the structural events (`MessageStart`, `ToolUseComplete`,
/// `ToolResult`) but not the per-token deltas, because deltas weren't
/// persisted.
fn synthesize_events(msg: &Message) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    if matches!(msg.kind, MessageKind::Assistant { .. }) {
        out.push(StreamEvent::MessageStart { message_id: msg.id });
    }
    for block in &msg.content {
        match block {
            ContentBlock::ToolUse { id, name, input, .. } => {
                out.push(StreamEvent::ToolUseComplete {
                    id: *id,
                    name: name.clone(),
                    input: input.clone(),
                });
            }
            ContentBlock::ToolResult { tool_use_id, is_error, .. } => {
                out.push(StreamEvent::ToolResult { id: *tool_use_id, is_error: *is_error });
            }
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Image { .. } => {
                // Text + thinking content lives in Postgres; clients
                // refetch via REST when they need to re-render the
                // body. Replay focuses on structural events the
                // renderer's incremental state machine consumes.
            }
        }
    }
    out
}

fn error_frame(err: &GatewayError) -> WsMessage {
    let body = serde_json::json!({"event": "error", "message": err.to_string()});
    WsMessage::Text(body.to_string().into())
}

/// Build the [`WorkRequest`] published for a `SendMessage` frame.
///
/// Pure: no I/O, no async. Extracted so the workspace-fragment injection
/// (A2) can be unit-tested without spinning up Postgres or NATS.
///
/// `workspace_fragment` is `None` when the tenant has no workspace row
/// or its `global_instructions` is empty; in either case the resulting
/// `WorkRequest.system_prompt` is empty.
pub(crate) fn build_send_message_request(
    tenant: &elena_types::TenantContext,
    thread_id: ThreadId,
    text: String,
    model: Option<ModelId>,
    max_turns: Option<u32>,
    autonomy: AutonomyMode,
    workspace_fragment: Option<String>,
) -> WorkRequest {
    let msg = Message {
        id: MessageId::new(),
        thread_id,
        tenant_id: tenant.tenant_id,
        role: Role::User,
        kind: MessageKind::User,
        content: vec![ContentBlock::Text { text, cache_control: None }],
        created_at: Utc::now(),
        token_count: None,
        parent_id: None,
    };
    let system_prompt = match workspace_fragment {
        Some(fragment) => vec![ContentBlock::Text { text: fragment, cache_control: None }],
        None => Vec::new(),
    };
    WorkRequest {
        request_id: RequestId::new(),
        tenant: tenant.clone(),
        thread_id,
        message: msg,
        model,
        system_prompt,
        max_tokens_per_turn: None,
        max_turns,
        trace_parent: None,
        trace_state: None,
        autonomy,
        kind: WorkRequestKind::Turn,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_send_message_minimal() {
        let raw = json!({"action": "send_message", "text": "hi"}).to_string();
        let frame: ClientFrame = serde_json::from_str(&raw).unwrap();
        match frame {
            ClientFrame::SendMessage { text, model, max_turns, autonomy: _ } => {
                assert_eq!(text, "hi");
                assert!(model.is_none());
                assert!(max_turns.is_none());
            }
            other @ ClientFrame::Abort => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn parse_send_message_with_overrides() {
        let raw = json!({
            "action": "send_message",
            "text": "explain",
            "model": "claude-opus-4-7",
            "max_turns": 3
        })
        .to_string();
        let frame: ClientFrame = serde_json::from_str(&raw).unwrap();
        match frame {
            ClientFrame::SendMessage { text, model, max_turns, autonomy: _ } => {
                assert_eq!(text, "explain");
                assert_eq!(model.unwrap().as_str(), "claude-opus-4-7");
                assert_eq!(max_turns, Some(3));
            }
            other @ ClientFrame::Abort => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn parse_abort() {
        let raw = json!({"action": "abort"}).to_string();
        let frame: ClientFrame = serde_json::from_str(&raw).unwrap();
        assert!(matches!(frame, ClientFrame::Abort));
    }

    #[test]
    fn unknown_action_rejected() {
        let raw = json!({"action": "something_else"}).to_string();
        assert!(serde_json::from_str::<ClientFrame>(&raw).is_err());
    }

    #[test]
    fn missing_text_rejected_for_send_message() {
        let raw = json!({"action": "send_message"}).to_string();
        assert!(serde_json::from_str::<ClientFrame>(&raw).is_err());
    }

    // ----- Workspace guardrail injection -----

    use std::collections::HashMap;

    use elena_types::{
        BudgetLimits, PermissionSet, SessionId, TenantContext, TenantId, TenantTier, UserId,
        WorkspaceId,
    };

    fn synthetic_tenant() -> TenantContext {
        TenantContext {
            tenant_id: TenantId::new(),
            user_id: UserId::new(),
            workspace_id: WorkspaceId::new(),
            thread_id: ThreadId::new(),
            session_id: SessionId::new(),
            permissions: PermissionSet::default(),
            budget: BudgetLimits::DEFAULT_PRO,
            tier: TenantTier::Pro,
            plan: None,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn d3_no_workspace_fragment_yields_empty_system_prompt() {
        let tenant = synthetic_tenant();
        let req = build_send_message_request(
            &tenant,
            ThreadId::new(),
            "send daily summary".into(),
            None,
            None,
            AutonomyMode::Moderate,
            None,
        );
        assert!(
            req.system_prompt.is_empty(),
            "expected empty system_prompt with no workspace fragment, got {:?}",
            req.system_prompt
        );
    }

    #[test]
    fn d3_workspace_fragment_lands_as_system_block() {
        let tenant = synthetic_tenant();
        let guardrail = "Never send Stripe payments above $500 without explicit user confirmation.";
        let req = build_send_message_request(
            &tenant,
            ThreadId::new(),
            "send a payout".into(),
            None,
            None,
            AutonomyMode::Cautious,
            Some(guardrail.to_owned()),
        );
        assert_eq!(req.system_prompt.len(), 1);
        match &req.system_prompt[0] {
            ContentBlock::Text { text, cache_control } => {
                assert_eq!(text, guardrail);
                assert!(
                    cache_control.is_none(),
                    "guardrail block must not pin cache_control by default"
                );
            }
            other => panic!("expected Text block, got {other:?}"),
        }
        // Autonomy + kind round-trip too.
        assert_eq!(req.autonomy, AutonomyMode::Cautious);
        assert_eq!(req.kind, WorkRequestKind::Turn);
    }

    #[test]
    fn d3_message_carries_user_text_and_tenant_scope() {
        let tenant = synthetic_tenant();
        let thread_id = ThreadId::new();
        let req = build_send_message_request(
            &tenant,
            thread_id,
            "hola, ¿qué tal?".into(),
            None,
            Some(7),
            AutonomyMode::Yolo,
            Some("Always reply in Spanish.".into()),
        );
        // Embedded user message preserves UTF-8 + tenant + thread scope.
        assert_eq!(req.message.thread_id, thread_id);
        assert_eq!(req.message.tenant_id, tenant.tenant_id);
        let text_blocks: Vec<&str> = req
            .message
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text { text, .. } = b { Some(text.as_str()) } else { None }
            })
            .collect();
        assert_eq!(text_blocks, vec!["hola, ¿qué tal?"]);
        // System prompt carries the guardrail in Spanish.
        assert!(req.system_prompt.iter().any(|b| matches!(
            b,
            ContentBlock::Text { text, .. } if text.contains("Spanish")
        )));
        // Max-turns override flows through.
        assert_eq!(req.max_turns, Some(7));
        // YOLO autonomy round-trips.
        assert_eq!(req.autonomy, AutonomyMode::Yolo);
    }

    #[test]
    fn d3_empty_workspace_fragment_treated_as_no_guardrail_at_caller() {
        // Caller responsibility: when the DB returns Ok("") the caller
        // passes None into this builder. We assert that an explicitly
        // empty `Some("")` would still produce one block — the DB path
        // is responsible for the empty-string filter (see the WS
        // handler's `Ok(fragment) if !fragment.is_empty()` arm).
        let tenant = synthetic_tenant();
        let req_some_empty = build_send_message_request(
            &tenant,
            ThreadId::new(),
            "x".into(),
            None,
            None,
            AutonomyMode::Moderate,
            Some(String::new()),
        );
        // We do create a block — the caller is responsible for
        // collapsing empty fragments. This documents the contract.
        assert_eq!(req_some_empty.system_prompt.len(), 1);
        match &req_some_empty.system_prompt[0] {
            ContentBlock::Text { text, .. } => assert_eq!(text, ""),
            other => panic!("expected Text block, got {other:?}"),
        }
    }
}
