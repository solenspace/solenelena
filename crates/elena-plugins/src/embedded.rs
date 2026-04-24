//! X7 — In-process plugin backend (no gRPC server required).
//!
//! Pre-X7 every embedded plugin spawned its own tonic server on a
//! loopback port and the gateway dialed it as if it were external —
//! same gRPC pipe, just over loopback. For Solen's 20+ first-party
//! connectors that's a lot of unnecessary tokio tasks and serdes.
//!
//! Post-X7 a [`PluginBackend::Embedded`] variant lets the registry
//! call the connector's `ElenaPlugin` trait methods directly. The
//! credential metadata flow (`x-elena-cred-<key>`) is preserved
//! verbatim — connectors see the same `tonic::Request` shape they
//! always saw, just without the round-trip through a server.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::client::PluginClient;
use crate::proto::pb;

/// Object-safe shape for an in-process plugin executor. Mirrors the
/// subset of `ElenaPlugin` the registry actually needs (`get_manifest`
/// + `execute`).
#[async_trait]
pub trait EmbeddedExecutor: Send + Sync + std::fmt::Debug + 'static {
    /// Fetch the plugin's manifest. Called once at register time.
    async fn manifest(&self) -> Result<pb::PluginManifest, tonic::Status>;

    /// Execute one action. Returns the response stream the connector
    /// emits (zero-or-more progress updates followed by one final
    /// result). The boxed stream lifetime is `'static` so the action
    /// tool can drive it across `tokio::select!` arms.
    async fn execute(
        &self,
        req: tonic::Request<pb::PluginRequest>,
    ) -> Result<BoxStream<'static, Result<pb::PluginResponse, tonic::Status>>, tonic::Status>;
}

/// Blanket impl: any concrete connector implementing the gRPC
/// `ElenaPlugin` trait gets an `EmbeddedExecutor` for free. The
/// connector's `Self::ExecuteStream` is `'static + Send` per the
/// generated trait, so boxing it is straight-through.
#[async_trait]
impl<T> EmbeddedExecutor for Arc<T>
where
    T: pb::elena_plugin_server::ElenaPlugin + std::fmt::Debug,
{
    async fn manifest(&self) -> Result<pb::PluginManifest, tonic::Status> {
        let resp = T::get_manifest(self.as_ref(), tonic::Request::new(())).await?;
        Ok(resp.into_inner())
    }

    async fn execute(
        &self,
        req: tonic::Request<pb::PluginRequest>,
    ) -> Result<BoxStream<'static, Result<pb::PluginResponse, tonic::Status>>, tonic::Status> {
        let resp = T::execute(self.as_ref(), req).await?;
        Ok(resp.into_inner().boxed())
    }
}

/// Backend an action tool dispatches through. Pre-X7 every backend
/// was [`Self::Remote`] (gRPC dial). Post-X7 first-party connectors
/// register as [`Self::Embedded`] and skip the network entirely.
#[derive(Debug)]
pub enum PluginBackend {
    /// Out-of-process gRPC sidecar — production-grade, process-isolated.
    Remote(Arc<PluginClient>),
    /// In-process embedded executor — same trait surface, no
    /// loopback / network.
    Embedded(Arc<dyn EmbeddedExecutor>),
}
