//! [`PluginClient`] — a tonic client wrapper with Elena's reconnect policy.
//!
//! One [`tonic::transport::Channel`] per plugin. The channel is built with
//! HTTP/2 keepalive so that a sidecar that drops its TCP connection (K8s
//! pod restart, network blip) is reconnected transparently by tonic on the
//! next RPC rather than making Elena carry the bookkeeping.
//!
//! Wrapped rather than aliased so that Phase 7 can hang mTLS / metadata
//! injection / per-tenant auth off it without touching every call site.

use std::time::Duration;

use elena_auth::TlsConfig;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::{error::PluginError, proto::pb};

/// Elena's gRPC client to a plugin sidecar.
#[derive(Debug, Clone)]
pub struct PluginClient {
    channel: Channel,
    endpoint_uri: String,
}

impl PluginClient {
    /// Build an endpoint, dial it with a connect timeout, and return the
    /// wrapped channel. Plaintext HTTP/2.
    ///
    /// Uses TCP keepalive (30 s) plus HTTP/2 keepalive (20 s) so a sidecar
    /// whose TCP dies silently surfaces on the next RPC as `Unavailable`.
    pub async fn connect(uri: &str, connect_timeout: Duration) -> Result<Self, PluginError> {
        Self::connect_with_tls(uri, connect_timeout, None).await
    }

    /// Connect variant that applies mTLS when `tls` is `Some`. Reuses
    /// [`elena_auth::TlsConfig`] so the same TLS policy drives gateway and
    /// worker dials.
    ///
    /// When `tls` is `Some` the endpoint scheme MUST be `https://`. Plain
    /// `http://` is rejected by tonic at connect time.
    pub async fn connect_with_tls(
        uri: &str,
        connect_timeout: Duration,
        tls: Option<&TlsConfig>,
    ) -> Result<Self, PluginError> {
        let mut endpoint = Endpoint::from_shared(uri.to_owned())?
            .connect_timeout(connect_timeout)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(20))
            .keep_alive_while_idle(true)
            .timeout(Duration::from_secs(120));

        if let Some(tls_cfg) = tls {
            let client_tls = build_tonic_client_tls(tls_cfg)?;
            endpoint = endpoint.tls_config(client_tls)?;
        }

        let channel = endpoint.connect().await?;
        Ok(Self { channel, endpoint_uri: uri.to_owned() })
    }

    /// Accessor for the endpoint URI — used by logging and health-check
    /// reporting.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint_uri
    }

    /// Construct a fresh tonic client against the shared channel. A new
    /// client is cheap — it's a thin reference to the channel underneath.
    #[must_use]
    pub fn rpc(&self) -> pb::elena_plugin_client::ElenaPluginClient<Channel> {
        pb::elena_plugin_client::ElenaPluginClient::new(self.channel.clone())
    }
}

/// Translate our [`elena_auth::TlsConfig`] into tonic's
/// [`tonic::transport::ClientTlsConfig`]. Reads certs/keys/CA from the
/// paths named in the config so rotation is a matter of re-reading files
/// rather than re-serialising keys.
fn build_tonic_client_tls(cfg: &TlsConfig) -> Result<ClientTlsConfig, PluginError> {
    use std::fs;

    let read = |path: &str| {
        fs::read(path).map_err(|e| PluginError::Manifest {
            plugin_id: String::new(),
            reason: format!("read TLS file {path}: {e}"),
        })
    };

    let cert = read(&cfg.cert_file)?;
    let key = read(&cfg.key_file)?;
    let identity = tonic::transport::Identity::from_pem(cert, key);

    let mut client_tls = ClientTlsConfig::new().identity(identity);
    if let Some(ca_path) = cfg.client_ca_file.as_deref() {
        let ca = read(ca_path)?;
        client_tls = client_tls.ca_certificate(tonic::transport::Certificate::from_pem(ca));
    }
    Ok(client_tls)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use tokio::net::TcpListener;
    use tonic::{Request, Response, Status, transport::Server};

    use super::*;

    // ----- a tiny in-process server fixture --------------------------

    #[derive(Debug, Default, Clone)]
    struct Fake;

    #[tonic::async_trait]
    impl pb::elena_plugin_server::ElenaPlugin for Fake {
        async fn get_manifest(
            &self,
            _: Request<()>,
        ) -> Result<Response<pb::PluginManifest>, Status> {
            Ok(Response::new(pb::PluginManifest {
                id: "fake".into(),
                name: "Fake".into(),
                version: "0.0.1".into(),
                actions: vec![pb::ActionDefinition {
                    name: "noop".into(),
                    description: "no-op".into(),
                    input_schema: r#"{"type":"object"}"#.into(),
                    output_schema: r#"{"type":"object"}"#.into(),
                    is_read_only: true,
                }],
                required_credentials: vec![],
            }))
        }

        type ExecuteStream =
            tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;

        async fn execute(
            &self,
            _: Request<pb::PluginRequest>,
        ) -> Result<Response<Self::ExecuteStream>, Status> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tx.send(Ok(pb::PluginResponse {
                payload: Some(pb::plugin_response::Payload::Result(pb::FinalResult {
                    output_json: b"{}".to_vec(),
                    is_error: false,
                })),
            }))
            .await
            .ok();
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
            Ok(Response::new(pb::HealthResponse { ok: true, detail: "ready".into() }))
        }
    }

    async fn spawn_fake() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = Server::builder()
                .add_service(pb::elena_plugin_server::ElenaPluginServer::new(Fake))
                .serve_with_incoming(incoming)
                .await;
        });
        // Give the server a moment to start accepting.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (addr, handle)
    }

    // ----- tests -----------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connects_and_calls_health() {
        let (addr, handle) = spawn_fake().await;
        let uri = format!("http://{addr}");
        let client = PluginClient::connect(&uri, Duration::from_secs(2)).await.expect("connect");
        let mut rpc = client.rpc();
        let resp = rpc.health(()).await.expect("health").into_inner();
        assert!(resp.ok);
        assert_eq!(resp.detail, "ready");
        handle.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connect_failure_surfaces_as_plugin_error() {
        // Unreachable port on loopback — refuse quickly.
        let err = PluginClient::connect("http://127.0.0.1:1", Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Connect(_)));
    }
}
