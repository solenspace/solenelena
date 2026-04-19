//! `POST /v1/threads` — create a new thread scoped to the caller's tenant.

use axum::Json;
use axum::extract::State;
use elena_types::ThreadId;
use serde::{Deserialize, Serialize};

use crate::app::GatewayState;
use crate::auth::AuthedTenant;
use crate::error::GatewayError;

/// Request body for thread creation. All fields are optional today — the
/// tenant is pulled from the JWT.
#[derive(Debug, Default, Deserialize)]
pub struct CreateThreadRequest {
    /// Optional human-readable title. Stored on the thread row.
    #[serde(default)]
    pub title: Option<String>,
}

/// Response body for a successful thread creation.
#[derive(Debug, Serialize)]
pub struct CreateThreadResponse {
    /// The new thread's id — clients use this for subsequent WebSocket
    /// upgrades.
    pub thread_id: ThreadId,
}

/// Handler: `POST /v1/threads` with an auth'd tenant.
pub async fn create_thread(
    State(state): State<GatewayState>,
    AuthedTenant(tenant): AuthedTenant,
    body: Option<Json<CreateThreadRequest>>,
) -> Result<Json<CreateThreadResponse>, GatewayError> {
    let title = body.and_then(|Json(b)| b.title);
    let thread_id = state
        .store
        .threads
        .create_thread(tenant.tenant_id, tenant.user_id, tenant.workspace_id, title.as_deref())
        .await
        .map_err(GatewayError::Store)?;
    Ok(Json(CreateThreadResponse { thread_id }))
}
