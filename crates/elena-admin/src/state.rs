//! Shared state held by admin routes.

use std::sync::Arc;

use elena_store::Store;
use secrecy::SecretString;

/// State shared by every admin route.
///
/// Cheap to clone — every field is `Arc`-shaped or a small wrapper.
#[derive(Clone)]
pub struct AdminState {
    /// Persistence handles.
    pub store: Arc<Store>,
    /// NATS core client, for health probes. `None` disables the NATS
    /// probe in `health/deep`.
    pub nats: Option<async_nats::Client>,
    /// Shared secret required as the `X-Elena-Admin-Token` header on
    /// every admin call. `None` disables the check (used by tests and
    /// smoke binaries that exercise the admin surface without an
    /// auth setup); production deployments must set this from
    /// `ELENA_ADMIN_TOKEN`.
    pub admin_token: Option<SecretString>,
}

impl std::fmt::Debug for AdminState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminState")
            .field("nats_connected", &self.nats.is_some())
            .field("admin_token_set", &self.admin_token.is_some())
            .finish()
    }
}

impl AdminState {
    /// Build with no admin token configured (tests, smokes).
    pub fn new(store: Arc<Store>, nats: Option<async_nats::Client>) -> Self {
        Self { store, nats, admin_token: None }
    }

    /// Attach the production admin token. Once set every admin call
    /// must include `X-Elena-Admin-Token: <token>`.
    #[must_use]
    pub fn with_admin_token(mut self, token: SecretString) -> Self {
        self.admin_token = Some(token);
        self
    }
}
