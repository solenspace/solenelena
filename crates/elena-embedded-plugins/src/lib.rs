//! X7 — Register first-party connectors directly into the
//! [`PluginRegistry`] without a per-plugin tonic server.
//!
//! Pre-X7 every embedded plugin spawned a `tonic::transport::Server`
//! on a loopback port and the gateway dialed it as if it were
//! external. For Solen's 20+ first-party connectors that's a lot of
//! tokio tasks + serdes for no benefit — same process, same trust
//! boundary.
//!
//! Post-X7 the connectors register via
//! [`PluginRegistry::register_embedded`], which wraps each in a
//! [`PluginBackend::Embedded(Arc<dyn EmbeddedExecutor>)`](elena_plugins::PluginBackend)
//! and dispatches by direct trait call. The `x-elena-cred-<key>`
//! metadata flow is preserved — connectors see the same
//! `tonic::Request` shape they always saw.
//!
//! ## Operator knob
//!
//! `ELENA_EMBEDDED_PLUGINS=slack,notion,sheets` — comma-separated
//! plugin ids. Unknown ids fail the process at boot so a typo
//! surfaces immediately instead of causing silent "tool not
//! registered" hallucinations at turn time.
//!
//! ## Adding a new embedded plugin
//!
//! 1. Add the connector crate to this crate's `[dependencies]`.
//! 2. Add one arm to [`register_embedded_plugins`].

#![warn(missing_docs)]

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use elena_plugins::{EmbeddedExecutor, PluginRegistry};
use tracing::info;

/// Parse the `ELENA_EMBEDDED_PLUGINS` env var. Empty/unset → empty vec.
/// Tokens are trimmed + lowercased; blank tokens are dropped.
#[must_use]
pub fn enabled_from_env() -> Vec<String> {
    let Ok(raw) = std::env::var("ELENA_EMBEDDED_PLUGINS") else {
        return Vec::new();
    };
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_lowercase).collect()
}

/// X7 — Register every requested connector into the supplied
/// [`PluginRegistry`] using the in-process [`EmbeddedExecutor`] path.
/// No loopback ports, no tonic servers, no gRPC round-trip.
///
/// Returns the count of connectors successfully registered.
pub async fn register_embedded_plugins(
    enabled: &[String],
    registry: &PluginRegistry,
) -> Result<usize> {
    let mut registered = 0;
    for plugin_id in enabled {
        let executor = build_executor(plugin_id)
            .with_context(|| format!("build embedded executor for {plugin_id}"))?;
        let id = registry
            .register_embedded(executor)
            .await
            .with_context(|| format!("register embedded plugin {plugin_id}"))?;
        info!(plugin_id = %id, "embedded plugin registered (in-process)");
        registered += 1;
    }
    Ok(registered)
}

fn build_executor(plugin_id: &str) -> Result<Arc<dyn EmbeddedExecutor>> {
    match plugin_id {
        "echo" => Ok(Arc::new(Arc::new(elena_connector_echo::EchoConnector))),
        "slack" => Ok(Arc::new(Arc::new(
            elena_connector_slack::SlackConnector::from_env()
                .context("construct SlackConnector from env")?,
        ))),
        "notion" => Ok(Arc::new(Arc::new(
            elena_connector_notion::NotionConnector::from_env()
                .context("construct NotionConnector from env")?,
        ))),
        "sheets" => Ok(Arc::new(Arc::new(
            elena_connector_sheets::SheetsConnector::from_env()
                .context("construct SheetsConnector from env")?,
        ))),
        "shopify" => Ok(Arc::new(Arc::new(
            elena_connector_shopify::ShopifyConnector::from_env()
                .context("construct ShopifyConnector from env")?,
        ))),
        other => Err(anyhow!(
            "ELENA_EMBEDDED_PLUGINS contains unknown plugin id {other:?} — update build_executor to add an arm"
        )),
    }
}
