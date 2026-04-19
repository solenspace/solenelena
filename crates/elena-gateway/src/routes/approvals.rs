//! `POST /v1/threads/:id/approvals` — client responds to a paused loop.
//!
//! When the loop emits `StreamEvent::AwaitingApproval`, the worker
//! releases the claim and the stream pauses. The client renders the
//! pending tool-call envelopes, collects an allow/deny (plus optional
//! edits) per one, and POSTs the batch here. The gateway:
//!
//! 1. Writes every decision to `thread_approvals` (idempotent on
//!    `(thread_id, tool_use_id)`).
//! 2. Publishes a `WorkRequest { kind: ResumeFromApproval }` so any
//!    worker can re-claim and resume from the checkpoint.

use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use elena_types::{
    ApprovalDecision, AutonomyMode, ContentBlock, Message, MessageId, MessageKind, RequestId, Role,
    ThreadId, WorkRequest, WorkRequestKind,
};
use serde::{Deserialize, Serialize};

use crate::app::GatewayState;
use crate::auth::AuthedTenant;
use crate::error::GatewayError;
use crate::nats::publish_work;

/// Request body — a batch of approval decisions for a paused thread.
#[derive(Debug, Deserialize)]
pub struct PostApprovalsRequest {
    /// One decision per `PendingApproval` surfaced by the loop. The
    /// client is expected to send the full batch in a single POST; the
    /// worker only resumes once the count matches the checkpointed
    /// pending list.
    pub approvals: Vec<ApprovalDecision>,
}

/// Response body.
#[derive(Debug, Serialize)]
pub struct PostApprovalsResponse {
    /// Number of rows written.
    pub written: usize,
}

/// Handler: `POST /v1/threads/:id/approvals` with an auth'd tenant.
pub async fn post_approvals(
    State(state): State<GatewayState>,
    AuthedTenant(tenant): AuthedTenant,
    Path(thread_id): Path<ThreadId>,
    Json(body): Json<PostApprovalsRequest>,
) -> Result<Json<PostApprovalsResponse>, GatewayError> {
    if body.approvals.is_empty() {
        return Err(GatewayError::BadRequest {
            message: "approvals body must contain at least one decision".into(),
        });
    }

    state
        .store
        .approvals
        .write(tenant.tenant_id, thread_id, &body.approvals)
        .await
        .map_err(GatewayError::Store)?;

    // Publish a resume marker. The message payload is a placeholder —
    // the worker skips the append because `kind` is `ResumeFromApproval`.
    let placeholder = Message {
        id: MessageId::new(),
        thread_id,
        tenant_id: tenant.tenant_id,
        role: Role::User,
        kind: MessageKind::User,
        content: vec![ContentBlock::Text { text: String::new(), cache_control: None }],
        created_at: Utc::now(),
        token_count: None,
        parent_id: None,
    };

    let req = WorkRequest {
        request_id: RequestId::new(),
        tenant: tenant.clone(),
        thread_id,
        message: placeholder,
        model: None,
        system_prompt: Vec::new(),
        max_tokens_per_turn: None,
        max_turns: None,
        trace_parent: None,
        trace_state: None,
        autonomy: AutonomyMode::default(),
        kind: WorkRequestKind::ResumeFromApproval,
    };

    publish_work(&state.jet, &req).await?;

    Ok(Json(PostApprovalsResponse { written: body.approvals.len() }))
}
