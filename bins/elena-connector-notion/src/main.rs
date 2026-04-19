//! Notion connector binary.
//!
//! Reads `NOTION_TOKEN` (required) + `NOTION_API_BASE_URL` (defaults to
//! `https://api.notion.com`) + `ELENA_NOTION_ADDR` (defaults to
//! `127.0.0.1:50072`).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::net::SocketAddr;

use elena_connector_notion::NotionConnector;
use elena_plugins::proto::pb;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("info,elena_connector_notion=info"),
        ))
        .with_writer(std::io::stderr)
        .init();

    let addr: SocketAddr = std::env::var("ELENA_NOTION_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50072".to_owned())
        .parse()?;
    let connector = NotionConnector::from_env()?;
    info!(%addr, "elena-connector-notion listening");

    Server::builder()
        .add_service(pb::elena_plugin_server::ElenaPluginServer::new(connector))
        .serve(addr)
        .await?;
    Ok(())
}
