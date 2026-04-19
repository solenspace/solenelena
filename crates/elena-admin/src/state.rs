//! Shared state held by admin routes.

use std::sync::Arc;

use elena_store::Store;

/// State shared by every admin route.
///
/// Cheap to clone — every field is `Arc`-shaped.
#[derive(Clone)]
pub struct AdminState {
    /// Persistence handles.
    pub store: Arc<Store>,
    /// NATS core client, for health probes. `None` disables the NATS
    /// probe in `health/deep`.
    pub nats: Option<async_nats::Client>,
}

impl std::fmt::Debug for AdminState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminState").field("nats_connected", &self.nats.is_some()).finish()
    }
}

impl AdminState {
    /// Build from an existing `Store` + optional NATS client.
    pub fn new(store: Arc<Store>, nats: Option<async_nats::Client>) -> Self {
        Self { store, nats }
    }
}
