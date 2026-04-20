//! Host plugin connectors as in-process tonic sidecars on loopback ports.
//!
//! Elena's plugin contract is gRPC (`crates/elena-plugins/src/proto/*.proto`).
//! In production deploys, a "plugin" is a separate process that listens on
//! a gRPC port and the gateway dials it at boot. That was built for heavy
//! or untrusted plugins that need their own container.
//!
//! For the long tail of plugins that are thin wrappers around a REST API
//! (Slack, Notion, Sheets, ...), forcing a dedicated Railway service per
//! plugin doesn't scale. Instead we keep the exact same gRPC contract but
//! host the sidecar in the same process as the gateway — each connector
//! gets its own `tokio::spawn` running a `tonic::transport::Server` on a
//! loopback `127.0.0.1:<random>` port. From the gateway's point of view
//! these are indistinguishable from external sidecars.
//!
//! ## Operator knob
//!
//! `ELENA_EMBEDDED_PLUGINS=slack,notion,sheets` — comma-separated plugin
//! ids. Unknown ids fail the process at boot so a typo surfaces
//! immediately instead of causing silent "tool not registered"
//! hallucinations at turn time.
//!
//! Each connector reads its own secrets from env (`SLACK_BOT_TOKEN`,
//! `NOTION_TOKEN`, ...). Per-tenant credentials flow through the
//! `x-elena-cred-*` gRPC metadata at execute time, same as the
//! out-of-process sidecars.
//!
//! ## Adding a new embedded plugin
//!
//! 1. Add the connector crate to `[dependencies]` in this crate's
//!    `Cargo.toml`.
//! 2. Add one arm to the match in [`spawn_one`] mapping its plugin id to
//!    a `tonic::codegen::Service` built from the connector's
//!    `from_env()` constructor.

#![warn(missing_docs)]

use std::net::SocketAddr;

use anyhow::{Context as _, Result, anyhow};
use elena_plugins::proto::pb::elena_plugin_server::ElenaPluginServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};

/// Parse the `ELENA_EMBEDDED_PLUGINS` env var. Empty/unset → empty vec.
/// Tokens are trimmed + lowercased; blank tokens are dropped.
#[must_use]
pub fn enabled_from_env() -> Vec<String> {
    let Ok(raw) = std::env::var("ELENA_EMBEDDED_PLUGINS") else {
        return Vec::new();
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Spawn one in-process tonic server per requested plugin id and return
/// their `http://127.0.0.1:<port>` URLs. The returned URLs are intended
/// to be appended to `ELENA_PLUGIN_ENDPOINTS` so the existing
/// `PluginRegistry::register_all` code path dials them uniformly.
///
/// Each spawned server runs for the lifetime of the process. There is
/// no graceful-shutdown hook today — when the gateway exits, the tokio
/// runtime tears all tasks down together.
pub async fn spawn_embedded(enabled: &[String]) -> Result<Vec<String>> {
    if enabled.is_empty() {
        return Ok(Vec::new());
    }

    let mut urls = Vec::with_capacity(enabled.len());
    for plugin_id in enabled {
        let url = spawn_one(plugin_id)
            .await
            .with_context(|| format!("failed to start embedded plugin {plugin_id}"))?;
        info!(%plugin_id, endpoint = %url, "embedded plugin sidecar bound");
        urls.push(url);
    }
    Ok(urls)
}

async fn spawn_one(plugin_id: &str) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind loopback listener for embedded plugin")?;
    let addr: SocketAddr = listener.local_addr().context("read listener addr")?;
    let incoming = TcpListenerStream::new(listener);
    let label = plugin_id.to_owned();

    match plugin_id {
        "echo" => {
            let svc = ElenaPluginServer::new(elena_connector_echo::EchoConnector);
            tokio::spawn(async move {
                if let Err(e) = Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(incoming)
                    .await
                {
                    warn!(plugin = %label, error = ?e, "embedded plugin tonic server exited");
                }
            });
        }
        "slack" => {
            let connector = elena_connector_slack::SlackConnector::from_env()
                .context("construct SlackConnector from env")?;
            let svc = ElenaPluginServer::new(connector);
            tokio::spawn(async move {
                if let Err(e) = Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(incoming)
                    .await
                {
                    warn!(plugin = %label, error = ?e, "embedded plugin tonic server exited");
                }
            });
        }
        "notion" => {
            let connector = elena_connector_notion::NotionConnector::from_env()
                .context("construct NotionConnector from env")?;
            let svc = ElenaPluginServer::new(connector);
            tokio::spawn(async move {
                if let Err(e) = Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(incoming)
                    .await
                {
                    warn!(plugin = %label, error = ?e, "embedded plugin tonic server exited");
                }
            });
        }
        "sheets" => {
            let connector = elena_connector_sheets::SheetsConnector::from_env()
                .context("construct SheetsConnector from env")?;
            let svc = ElenaPluginServer::new(connector);
            tokio::spawn(async move {
                if let Err(e) = Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(incoming)
                    .await
                {
                    warn!(plugin = %label, error = ?e, "embedded plugin tonic server exited");
                }
            });
        }
        "shopify" => {
            let connector = elena_connector_shopify::ShopifyConnector::from_env()
                .context("construct ShopifyConnector from env")?;
            let svc = ElenaPluginServer::new(connector);
            tokio::spawn(async move {
                if let Err(e) = Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(incoming)
                    .await
                {
                    warn!(plugin = %label, error = ?e, "embedded plugin tonic server exited");
                }
            });
        }
        other => {
            return Err(anyhow!(
                "ELENA_EMBEDDED_PLUGINS contains unknown plugin id {other:?} — update elena-embedded-plugins/src/lib.rs to add an arm"
            ));
        }
    }

    Ok(format!("http://{addr}"))
}
