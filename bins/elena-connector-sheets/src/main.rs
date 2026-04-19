//! Google Sheets connector binary.
//!
//! Reads `GOOGLE_ACCESS_TOKEN` (required, mint via gcloud or token-broker),
//! `GOOGLE_SHEETS_API_BASE_URL` (defaults to <https://sheets.googleapis.com>),
//! and `ELENA_SHEETS_ADDR` (defaults to `127.0.0.1:50073`).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::net::SocketAddr;

use elena_connector_sheets::SheetsConnector;
use elena_plugins::proto::pb;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("info,elena_connector_sheets=info"),
        ))
        .with_writer(std::io::stderr)
        .init();

    let addr: SocketAddr = std::env::var("ELENA_SHEETS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50073".to_owned())
        .parse()?;
    let connector = SheetsConnector::from_env()?;
    info!(%addr, "elena-connector-sheets listening");

    Server::builder()
        .add_service(pb::elena_plugin_server::ElenaPluginServer::new(connector))
        .serve(addr)
        .await?;
    Ok(())
}
