//! Plugin health gauges + background [`HealthMonitor`].
//!
//! Each plugin gets an `Arc<AtomicU8>` whose value is one of
//! [`HEALTH_UNKNOWN`], [`HEALTH_UP`], [`HEALTH_DOWN`]. The gauge is
//! informational; it's flipped by the background [`HealthMonitor`] at a
//! fixed interval and lazily by failed RPC calls in
//! [`PluginActionTool`](crate::PluginActionTool).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::{client::PluginClient, id::PluginId};

/// Initial value — plugin has never reported status.
pub const HEALTH_UNKNOWN: u8 = 0;
/// Last probe succeeded.
pub const HEALTH_UP: u8 = 1;
/// Last probe (or recent RPC) failed.
pub const HEALTH_DOWN: u8 = 2;

/// Human-readable health state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// Plugin has never reported status.
    Unknown,
    /// Last probe succeeded.
    Up,
    /// Last probe (or recent RPC) failed.
    Down,
}

impl From<u8> for HealthState {
    fn from(raw: u8) -> Self {
        match raw {
            HEALTH_UP => Self::Up,
            HEALTH_DOWN => Self::Down,
            _ => Self::Unknown,
        }
    }
}

/// Background task that polls every plugin's `Health` RPC at a fixed
/// interval and flips each plugin's gauge accordingly.
///
/// The monitor is started when a [`PluginRegistry`](crate::PluginRegistry)
/// has at least one plugin; dropped via [`Self::shutdown`] when the
/// registry shuts down.
#[derive(Debug)]
pub struct HealthMonitor {
    gauges: Arc<DashMap<PluginId, Arc<AtomicU8>>>,
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl HealthMonitor {
    /// Build a new monitor and spawn its background task.
    #[must_use]
    pub fn spawn(
        clients: Arc<DashMap<PluginId, Arc<PluginClient>>>,
        gauges: Arc<DashMap<PluginId, Arc<AtomicU8>>>,
        interval: Duration,
    ) -> Self {
        let cancel = CancellationToken::new();
        let cancel_child = cancel.clone();
        let gauges_for_task = Arc::clone(&gauges);

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    biased;
                    () = cancel_child.cancelled() => break,
                    _ = ticker.tick() => {
                        Self::probe_all(&clients, &gauges_for_task).await;
                    }
                }
            }
        });

        Self { gauges, cancel, handle: Some(handle) }
    }

    /// Probe every registered plugin in parallel.
    async fn probe_all(
        clients: &DashMap<PluginId, Arc<PluginClient>>,
        gauges: &DashMap<PluginId, Arc<AtomicU8>>,
    ) {
        let mut set = JoinSet::new();
        // DashMap iteration borrows internal shards; idiomatic access is via
        // `.iter()` so `clippy::explicit_iter_loop` doesn't apply here.
        #[allow(clippy::explicit_iter_loop)]
        for entry in clients.iter() {
            let plugin = entry.key().clone();
            let client = Arc::clone(entry.value());
            let gauge = gauges.get(&plugin).map(|g| Arc::clone(g.value()));
            if let Some(gauge) = gauge {
                set.spawn(async move {
                    let mut rpc = client.rpc();
                    let fut = rpc.health(());
                    match tokio::time::timeout(Duration::from_secs(5), fut).await {
                        Ok(Ok(resp)) => {
                            let new = if resp.into_inner().ok { HEALTH_UP } else { HEALTH_DOWN };
                            gauge.store(new, Ordering::Relaxed);
                            debug!(plugin = %plugin, state = new, "health probe");
                        }
                        Ok(Err(status)) => {
                            gauge.store(HEALTH_DOWN, Ordering::Relaxed);
                            warn!(plugin = %plugin, code = %status.code(), "health RPC failed");
                        }
                        Err(_elapsed) => {
                            gauge.store(HEALTH_DOWN, Ordering::Relaxed);
                            warn!(plugin = %plugin, "health RPC timed out");
                        }
                    }
                });
            }
        }
        while set.join_next().await.is_some() {}
    }

    /// Read the current state of a plugin's gauge.
    #[must_use]
    pub fn state_of(&self, plugin: &PluginId) -> HealthState {
        self.gauges
            .get(plugin)
            .map_or(HealthState::Unknown, |g| g.value().load(Ordering::Relaxed).into())
    }

    /// Stop the background task and await its exit.
    pub async fn shutdown(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for HealthMonitor {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Deliberately do not `.await` the handle here — callers wanting a
        // clean stop use [`Self::shutdown`]. Drop just best-effort cancels.
    }
}
