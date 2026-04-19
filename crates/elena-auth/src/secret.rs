//! [`SecretProvider`] trait + built-in implementations.
//!
//! Callers hold an `Arc<dyn SecretProvider>` and ask for secrets by name.
//! The provider is the integration point: env vars, Vault, KMS,
//! Kubernetes secret-mount, whatever. Phase-7 ships the env-backed impl
//! (v1.0 minimum) with Vault/KMS opt-in via cargo features.
//!
//! Secrets are wrapped in [`secrecy::SecretString`] so they don't leak
//! through `Debug` impls or default log formatters.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use secrecy::SecretString;

/// A named secret provider.
///
/// Implementors return [`secrecy::SecretString`]s — wrapping ensures secrets
/// don't leak through Debug. `fetch` is async so remote backends (Vault,
/// KMS, k8s API) work naturally; local `EnvSecretProvider` resolves
/// synchronously under the hood but satisfies the async signature.
#[async_trait]
pub trait SecretProvider: Send + Sync + std::fmt::Debug + 'static {
    /// Short provider identifier — used in log lines.
    fn name(&self) -> &str;

    /// Fetch the current value of `name`. `SecretError::NotFound` when the
    /// backend has no value for that name; `SecretError::Backend` when the
    /// backend itself errored.
    async fn fetch(&self, name: &str) -> Result<SecretString, SecretError>;
}

/// Reference to a configured secret (`{provider, key}`). Serializable so it
/// can live in config and flow across the wire without exposing the value.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SecretRef {
    /// Logical provider name (`"env"`, `"vault"`, `"kms"`, etc.). Matches
    /// a [`SecretProvider::name`] registered in the app's provider map.
    pub provider: String,
    /// Key within the provider's namespace.
    pub key: String,
}

/// Errors from [`SecretProvider::fetch`].
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The provider does not know the requested key.
    #[error("secret not found: {0}")]
    NotFound(String),
    /// The backend itself reported an error.
    #[error("secret backend error: {0}")]
    Backend(String),
}

/// Abstraction over "where environment-shaped values come from".
/// Production uses [`process_env`]; tests can build their own in-memory map.
pub trait EnvSource: Send + Sync + std::fmt::Debug + 'static {
    /// Return the value for `key`, or `None` when unset.
    fn var(&self, key: &str) -> Option<String>;
}

/// Default [`EnvSource`] that reads `std::env`.
#[derive(Debug, Clone, Default)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// In-memory [`EnvSource`] fixture for tests. Not wired into production
/// call paths.
#[derive(Debug, Default, Clone)]
pub struct InMemoryEnv {
    inner: std::collections::HashMap<String, String>,
}

impl InMemoryEnv {
    /// Build from iterable of `(key, value)` pairs.
    #[must_use]
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self { inner: pairs.into_iter().map(|(k, v)| (k.into(), v.into())).collect() }
    }
}

impl EnvSource for InMemoryEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.inner.get(key).cloned()
    }
}

/// Environment-variable–backed provider. Looks up `ELENA_SECRET_<NAME>`
/// first, then `<NAME>` as a fallback (so `VaultSecretProvider`-style
/// keys cohabit with legacy keys like `ANTHROPIC_API_KEY`).
pub struct EnvSecretProvider {
    prefix: String,
    source: Arc<dyn EnvSource>,
}

impl std::fmt::Debug for EnvSecretProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvSecretProvider").field("prefix", &self.prefix).finish()
    }
}

impl Default for EnvSecretProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvSecretProvider {
    /// Build with the default prefix (`ELENA_SECRET_`) reading
    /// [`std::env`].
    #[must_use]
    pub fn new() -> Self {
        Self { prefix: "ELENA_SECRET_".to_owned(), source: Arc::new(ProcessEnv) }
    }

    /// Build with a custom prefix.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self { prefix: prefix.into(), source: Arc::new(ProcessEnv) }
    }

    /// Build against a custom [`EnvSource`] (typically [`InMemoryEnv`] for
    /// tests). The default prefix still applies.
    #[must_use]
    pub fn with_source(source: Arc<dyn EnvSource>) -> Self {
        Self { prefix: "ELENA_SECRET_".to_owned(), source }
    }
}

#[async_trait]
impl SecretProvider for EnvSecretProvider {
    fn name(&self) -> &str {
        "env"
    }

    async fn fetch(&self, name: &str) -> Result<SecretString, SecretError> {
        let prefixed = format!("{}{}", self.prefix, name.to_ascii_uppercase());
        if let Some(v) = self.source.var(&prefixed) {
            return Ok(SecretString::from(v));
        }
        if let Some(v) = self.source.var(name) {
            return Ok(SecretString::from(v));
        }
        Err(SecretError::NotFound(name.to_owned()))
    }
}

/// Cache-in-front wrapper. Fetches from the underlying provider once and
/// serves the cached value for `ttl` before re-fetching.
///
/// Used by LLM clients to absorb cheap key rotation without every RPC
/// hitting the secret backend; on 401 the caller can call
/// [`Self::invalidate`] to force a re-fetch on the next read.
pub struct CachedSecret {
    provider: Arc<dyn SecretProvider>,
    name: String,
    ttl: Duration,
    slot: Mutex<Option<(SecretString, Instant)>>,
}

impl std::fmt::Debug for CachedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedSecret")
            .field("provider", &self.provider.name())
            .field("name", &self.name)
            .field("ttl_secs", &self.ttl.as_secs())
            .finish()
    }
}

impl CachedSecret {
    /// Build a new cache with the given TTL.
    #[must_use]
    pub fn new(provider: Arc<dyn SecretProvider>, name: impl Into<String>, ttl: Duration) -> Self {
        Self { provider, name: name.into(), ttl, slot: Mutex::new(None) }
    }

    /// Read the current value, fetching from the backend if the cache is
    /// empty or the TTL has expired.
    pub async fn get(&self) -> Result<SecretString, SecretError> {
        if let Some(value) = self.cached_value() {
            return Ok(value);
        }
        let fresh = self.provider.fetch(&self.name).await?;
        let Ok(mut guard) = self.slot.lock() else {
            return Ok(fresh);
        };
        *guard = Some((fresh.clone(), Instant::now()));
        Ok(fresh)
    }

    /// Drop the cached value. Next [`Self::get`] re-fetches.
    pub fn invalidate(&self) {
        if let Ok(mut guard) = self.slot.lock() {
            *guard = None;
        }
    }

    fn cached_value(&self) -> Option<SecretString> {
        let guard = self.slot.lock().ok()?;
        let (value, at) = guard.as_ref()?;
        if at.elapsed() < self.ttl { Some(value.clone()) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn env_provider_reads_prefixed_var() {
        let source: Arc<dyn EnvSource> =
            Arc::new(InMemoryEnv::from_pairs([("ELENA_SECRET_MY_KEY", "hunter2")]));
        let provider = EnvSecretProvider::with_source(source);
        let got = provider.fetch("my_key").await.unwrap();
        assert_eq!(got.expose_secret(), "hunter2");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn env_provider_falls_back_to_bare_name() {
        let source: Arc<dyn EnvSource> =
            Arc::new(InMemoryEnv::from_pairs([("MY_FALLBACK_KEY", "backup-value")]));
        let provider = EnvSecretProvider::with_source(source);
        let got = provider.fetch("MY_FALLBACK_KEY").await.unwrap();
        assert_eq!(got.expose_secret(), "backup-value");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn env_provider_reports_not_found() {
        let source: Arc<dyn EnvSource> = Arc::new(InMemoryEnv::default());
        let provider = EnvSecretProvider::with_source(source);
        let err = provider.fetch("definitely_not_set_xyz").await.unwrap_err();
        assert!(matches!(err, SecretError::NotFound(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_secret_returns_same_value_within_ttl() {
        let counter = Arc::new(AtomicU32::new(0));
        let provider: Arc<dyn SecretProvider> =
            Arc::new(CountingProvider { calls: Arc::clone(&counter) });
        let cached = CachedSecret::new(provider, "any", Duration::from_secs(30));
        let a = cached.get().await.unwrap();
        let b = cached.get().await.unwrap();
        assert_eq!(a.expose_secret(), b.expose_secret());
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_secret_refetches_on_invalidate() {
        let counter = Arc::new(AtomicU32::new(0));
        let provider: Arc<dyn SecretProvider> =
            Arc::new(CountingProvider { calls: Arc::clone(&counter) });
        let cached = CachedSecret::new(provider, "any", Duration::from_secs(30));
        let _ = cached.get().await.unwrap();
        cached.invalidate();
        let _ = cached.get().await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_secret_refetches_after_ttl_expiry() {
        let counter = Arc::new(AtomicU32::new(0));
        let provider: Arc<dyn SecretProvider> =
            Arc::new(CountingProvider { calls: Arc::clone(&counter) });
        let cached = CachedSecret::new(provider, "any", Duration::from_millis(5));
        let _ = cached.get().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = cached.get().await.unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Debug)]
    struct CountingProvider {
        calls: Arc<AtomicU32>,
    }

    #[async_trait]
    impl SecretProvider for CountingProvider {
        fn name(&self) -> &str {
            "counting"
        }
        async fn fetch(&self, _: &str) -> Result<SecretString, SecretError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(SecretString::from(format!("v{n}")))
        }
    }
}
