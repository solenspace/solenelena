//! Shared state held by admin routes.

use std::sync::Arc;

use elena_plugins::PluginRegistry;
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
    /// In-memory plugin registry. Backs `GET /admin/v1/plugins` so
    /// operators (and our BFF) can confirm which sidecars actually
    /// answered `GetManifest()` at boot. `None` for tests + smokes
    /// that don't construct a registry.
    pub plugins: Option<Arc<PluginRegistry>>,
}

impl std::fmt::Debug for AdminState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminState")
            .field("nats_connected", &self.nats.is_some())
            .field("admin_token_set", &self.admin_token.is_some())
            .field("plugins_attached", &self.plugins.is_some())
            .finish()
    }
}

impl AdminState {
    /// Build with no admin token configured (tests, smokes).
    pub fn new(store: Arc<Store>, nats: Option<async_nats::Client>) -> Self {
        Self { store, nats, admin_token: None, plugins: None }
    }

    /// Attach the production admin token. Once set every admin call
    /// must include `X-Elena-Admin-Token: <token>`.
    #[must_use]
    pub fn with_admin_token(mut self, token: SecretString) -> Self {
        self.admin_token = Some(token);
        self
    }

    /// Attach the live plugin registry so `GET /admin/v1/plugins` can
    /// report registered manifests. The gateway boot path passes the
    /// same `Arc<PluginRegistry>` it hands to the worker, so the admin
    /// view stays in lock-step with what the LLM actually sees.
    #[must_use]
    pub fn with_plugins(mut self, plugins: Arc<PluginRegistry>) -> Self {
        self.plugins = Some(plugins);
        self
    }
}
