//! Shopify connector binary.
//!
//! Reads `SHOPIFY_ADMIN_TOKEN` (required) plus either
//! `SHOPIFY_API_BASE_URL` (full base, used for wiremock tests) or
//! `SHOPIFY_SHOP_DOMAIN` (`mystore.myshopify.com`). Listens on
//! `ELENA_SHOPIFY_ADDR` (defaults to `127.0.0.1:50074`).

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::net::SocketAddr;

use elena_connector_shopify::ShopifyConnector;
use elena_plugins::proto::pb;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("info,elena_connector_shopify=info"),
        ))
        .with_writer(std::io::stderr)
        .init();

    let addr: SocketAddr = std::env::var("ELENA_SHOPIFY_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50074".to_owned())
        .parse()?;
    let connector = ShopifyConnector::from_env()?;
    info!(%addr, "elena-connector-shopify listening");

    Server::builder()
        .add_service(pb::elena_plugin_server::ElenaPluginServer::new(connector))
        .serve(addr)
        .await?;
    Ok(())
}
