//! Admin-API auth middleware.
//!
//! Cleanup-2 — moved into [`elena_auth::admin`]. Re-exported here so
//! existing `elena_admin::auth::*` imports keep compiling.

pub use elena_auth::{
    ADMIN_TOKEN_HEADER, TENANT_SCOPE_HEADER, hash_scope_token, require_admin_token,
    require_tenant_scope,
};
