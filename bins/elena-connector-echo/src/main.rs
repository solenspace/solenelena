//! Reference Elena plugin sidecar binary.
//!
//! Wraps [`elena_connector_echo::EchoConnector`] in a tonic server. Listens
//! on `127.0.0.1:50061` by default; override with `ELENA_ECHO_ADDR`.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::net::SocketAddr;

use elena_connector_echo::EchoConnector;
use elena_plugins::proto::pb;
use tonic::transport::Server;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("info,elena_connector_echo=info"),
        ))
        .with_writer(std::io::stderr)
        .init();

    let addr: SocketAddr = std::env::var("ELENA_ECHO_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:50061".to_owned())
        .parse()?;
    info!(%addr, "elena-connector-echo listening");

    Server::builder()
        .add_service(pb::elena_plugin_server::ElenaPluginServer::new(EchoConnector))
        .serve(addr)
        .await?;
    Ok(())
}
