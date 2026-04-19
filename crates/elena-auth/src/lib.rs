//! Auth + TLS helpers shared by gateway, worker, and plugin sidecars.
//!
//! Three concerns:
//!
//! 1. **Secret provisioning.** [`SecretProvider`] is a trait with a
//!    cache-in-front impl ([`CachedSecret`]). v1.0 ships
//!    [`EnvSecretProvider`]; Vault and AWS KMS providers live behind the
//!    `vault` and `kms` features.
//! 2. **TLS / mTLS.** [`tls`] builds `rustls::ClientConfig` and
//!    `rustls::ServerConfig` from PEM files, with a `notify`-driven
//!    file watcher for hot rotation.
//! 3. **JWKS.** [`jwks::JwksValidator`] periodically refreshes a JSON Web
//!    Key Set from an issuer URL and verifies JWTs against the current
//!    key material. Backward-compatible with static-key mode.

#![warn(missing_docs)]
// Pedantic lints churn against this crate's natural style (proper-noun
// heavy docs, cert/key paths, JWKS wire format). Suppress the noisy ones
// at crate level.
#![allow(
    clippy::doc_markdown,
    clippy::unnecessary_literal_bound,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::needless_pass_by_value,
    clippy::single_match_else,
    clippy::or_fun_call,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::map_unwrap_or,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate
)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod jwks;
pub mod secret;
pub mod tls;

pub use jwks::{JwksError, JwksValidator, JwksValidatorConfig};
pub use secret::{CachedSecret, EnvSecretProvider, SecretError, SecretProvider, SecretRef};
pub use tls::{TlsConfig, TlsError, build_client_config, build_server_config};
