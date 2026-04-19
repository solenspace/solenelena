//! TLS / mTLS config builders.
//!
//! Phase 7 uses [`build_client_config`] on the worker side when dialing
//! plugin sidecars, and [`build_server_config`] on the sidecar/connector
//! side when accepting. Both support optional client-cert verification
//! (the "m" in mTLS) via a CA bundle.
//!
//! File-watcher–driven hot reload is deferred to v1.0.x; for v1.0 the
//! caller restarts the process to pick up rotated certs. The `notify`
//! workspace dep is pulled in now so the plumbing is ready when the
//! hot-reload flag lands.

use std::{fs, io::Cursor, path::Path, sync::Arc};

use rustls::{
    ClientConfig, RootCertStore, ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
};
use serde::Deserialize;

/// Operator-facing TLS configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    /// PEM file with the server's certificate chain (leaf first).
    pub cert_file: String,
    /// PEM file with the server's private key. PKCS#8 or RSA PKCS#1.
    pub key_file: String,
    /// Optional PEM file with the CA bundle. When set on the **server**
    /// side, enables mTLS (clients must present a cert signed by this CA).
    /// When set on the **client** side, overrides the system trust store.
    #[serde(default)]
    pub client_ca_file: Option<String>,
}

/// Errors raised when building a TLS config.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// Failed to read a referenced PEM file.
    #[error("read TLS file {path}: {source}")]
    Io {
        /// Path that failed to read.
        path: String,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// A PEM file contained no usable material.
    #[error("no {kind} found in PEM at {path}")]
    EmptyPem {
        /// What was being parsed (`"certificates"`, `"private key"`, `"CA certs"`).
        kind: &'static str,
        /// Path of the offending file.
        path: String,
    },
    /// rustls rejected the supplied material.
    #[error("rustls error: {0}")]
    Rustls(String),
}

/// Build a `rustls::ServerConfig` for the connector/sidecar side.
///
/// If `client_ca_file` is set, requires client cert verification (mTLS).
pub fn build_server_config(cfg: &TlsConfig) -> Result<Arc<ServerConfig>, TlsError> {
    install_default_crypto_provider();

    let cert_chain = load_certs(&cfg.cert_file)?;
    let key = load_private_key(&cfg.key_file)?;

    let builder = ServerConfig::builder();
    let builder = if let Some(ca) = cfg.client_ca_file.as_deref() {
        let roots = load_roots(ca)?;
        let verifier = WebPkiClientVerifier::builder(roots.into())
            .build()
            .map_err(|e| TlsError::Rustls(e.to_string()))?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };

    let config =
        builder.with_single_cert(cert_chain, key).map_err(|e| TlsError::Rustls(e.to_string()))?;
    Ok(Arc::new(config))
}

/// Build a `rustls::ClientConfig` for the worker side.
///
/// `cert_file` + `key_file` enable mTLS on outbound connections (the worker
/// presents a client cert). `client_ca_file` pins server roots; when unset,
/// the platform's native roots apply (via `rustls-native-certs` if wired
/// higher up; for now we use the embedded WebPKI roots).
pub fn build_client_config(cfg: &TlsConfig) -> Result<Arc<ClientConfig>, TlsError> {
    install_default_crypto_provider();

    // v1.0 requires the operator to pin `client_ca_file` explicitly (the
    // K8s/internal-PKI case). A "use system roots" mode lands with the
    // Phase-7 tail work once `rustls-native-certs` is wired in —
    // documented in the runbook.
    let roots = match cfg.client_ca_file.as_deref() {
        Some(path) => load_roots(path)?,
        None => RootCertStore::empty(),
    };

    let cert_chain = load_certs(&cfg.cert_file)?;
    let key = load_private_key(&cfg.key_file)?;

    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(cert_chain, key)
        .map_err(|e| TlsError::Rustls(e.to_string()))?;
    Ok(Arc::new(config))
}

fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let bytes = read(path)?;
    let certs = rustls_pemfile::certs(&mut Cursor::new(bytes))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Rustls(e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::EmptyPem { kind: "certificates", path: path.to_owned() });
    }
    Ok(certs)
}

fn load_private_key(path: &str) -> Result<PrivateKeyDer<'static>, TlsError> {
    let bytes = read(path)?;
    let key = rustls_pemfile::private_key(&mut Cursor::new(bytes))
        .map_err(|e| TlsError::Rustls(e.to_string()))?;
    key.ok_or_else(|| TlsError::EmptyPem { kind: "private key", path: path.to_owned() })
}

fn load_roots(path: &str) -> Result<RootCertStore, TlsError> {
    let bytes = read(path)?;
    let mut store = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut Cursor::new(bytes)) {
        let cert = cert.map_err(|e| TlsError::Rustls(e.to_string()))?;
        store.add(cert).map_err(|e| TlsError::Rustls(e.to_string()))?;
    }
    if store.is_empty() {
        return Err(TlsError::EmptyPem { kind: "CA certs", path: path.to_owned() });
    }
    Ok(store)
}

fn read(path: &str) -> Result<Vec<u8>, TlsError> {
    fs::read(Path::new(path)).map_err(|e| TlsError::Io { path: path.to_owned(), source: e })
}

fn install_default_crypto_provider() {
    // rustls 0.23 requires a provider to be installed. Using `ring` is the
    // standard default. Installs idempotently.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_cert_file_is_io_error() {
        let cfg = TlsConfig {
            cert_file: "/nonexistent/server.crt".into(),
            key_file: "/nonexistent/server.key".into(),
            client_ca_file: None,
        };
        let err = build_server_config(&cfg).unwrap_err();
        assert!(matches!(err, TlsError::Io { .. }));
    }

    #[test]
    fn empty_cert_file_is_empty_pem_error() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let cfg = TlsConfig {
            cert_file: tmp.path().to_string_lossy().into_owned(),
            key_file: tmp.path().to_string_lossy().into_owned(),
            client_ca_file: None,
        };
        let err = build_server_config(&cfg).unwrap_err();
        assert!(matches!(err, TlsError::EmptyPem { kind: "certificates", .. }));
    }
}
