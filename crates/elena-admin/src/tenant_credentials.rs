//! Per-tenant credential routes — `PUT /admin/v1/tenants/:id/credentials/:plugin_id`.
//!
//! Operator hits this once per tenant per plugin to provision the
//! credentials Elena will inject into that connector at dispatch time.
//! The body is a plain JSON `{ "key": "value" }` map; the store
//! encrypts it with `Aes256Gcm` using the master key on the server side
//! before persisting. Plaintext never touches disk.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use elena_types::TenantId;
use serde::Deserialize;

use crate::state::AdminState;

/// Request body for `PUT /admin/v1/tenants/:tenant_id/credentials/:plugin_id`.
#[derive(Debug, Deserialize)]
pub struct PutCredentialsRequest {
    /// Plain key/value map of credentials. Common shapes:
    /// `{ "token": "xoxb-..." }` for Slack,
    /// `{ "token": "secret_..." }` for Notion,
    /// `{ "token": "ya29...." }` for Sheets,
    /// `{ "token": "shpat_..." }` for Shopify.
    pub credentials: BTreeMap<String, String>,
}

/// `PUT /admin/v1/tenants/:tenant_id/credentials/:plugin_id` — persist
/// the encrypted credential map. Returns `204 No Content` on success
/// (no payload — the operator already knows what they sent), `503` if
/// the deployment never provisioned `ELENA_CREDENTIAL_MASTER_KEY`, and
/// `5xx` on encryption / database failure.
pub async fn put_credentials(
    State(state): State<AdminState>,
    Path((tenant_id, plugin_id)): Path<(TenantId, String)>,
    Json(req): Json<PutCredentialsRequest>,
) -> impl IntoResponse {
    let Some(creds) = state.store.tenant_credentials.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ELENA_CREDENTIAL_MASTER_KEY not provisioned on this server",
        )
            .into_response();
    };
    match creds.upsert(tenant_id, &plugin_id, &req.credentials).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(?e, %tenant_id, %plugin_id, "tenant credentials upsert failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// `DELETE /admin/v1/tenants/:tenant_id/credentials/:plugin_id` — drop
/// any stored credential row. Returns `204` whether or not a row was
/// present (idempotent).
pub async fn delete_credentials(
    State(state): State<AdminState>,
    Path((tenant_id, plugin_id)): Path<(TenantId, String)>,
) -> impl IntoResponse {
    let Some(creds) = state.store.tenant_credentials.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ELENA_CREDENTIAL_MASTER_KEY not provisioned on this server",
        )
            .into_response();
    };
    match creds.clear(tenant_id, &plugin_id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(?e, %tenant_id, %plugin_id, "tenant credentials delete failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}
