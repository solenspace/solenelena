//! Operator-facing configuration for the plugin layer.

use std::time::Duration;

use elena_auth::TlsConfig;
use serde::Deserialize;

/// Per-worker plugin configuration.
///
/// Loaded from the `[worker.plugins]` section of the operator's Elena
/// config (or, in tests, constructed by hand).
#[derive(Debug, Clone, Deserialize)]
pub struct PluginsConfig {
    /// gRPC endpoints to connect to at boot. Each entry is parsed into a
    /// [`tonic::transport::Endpoint`] and registered with the
    /// [`PluginRegistry`](crate::PluginRegistry).
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// How long to wait for the initial gRPC connection per endpoint
    /// before giving up on registration. Defaults to 5 s.
    #[serde(default = "default_connect_timeout_ms", rename = "connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// How often the background health monitor polls each plugin.
    /// Defaults to 30 s.
    #[serde(default = "default_health_interval_ms", rename = "health_interval_ms")]
    pub health_interval_ms: u64,
    /// Per-Execute timeout. Defaults to 60 s. The synthesised
    /// [`PluginActionTool`](crate::PluginActionTool) returns
    /// [`elena_types::ToolError::Timeout`] when this elapses.
    #[serde(default = "default_execute_timeout_ms", rename = "execute_timeout_ms")]
    pub execute_timeout_ms: u64,
    /// Optional mTLS client configuration for dialing plugin sidecars.
    /// When `None`, all dials are plaintext HTTP/2 — only safe on a
    /// trusted private network (or for in-process loopback tests).
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            connect_timeout_ms: default_connect_timeout_ms(),
            health_interval_ms: default_health_interval_ms(),
            execute_timeout_ms: default_execute_timeout_ms(),
            tls: None,
        }
    }
}

impl PluginsConfig {
    /// [`Duration`] form of `connect_timeout_ms`.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    /// [`Duration`] form of `health_interval_ms`.
    #[must_use]
    pub fn health_interval(&self) -> Duration {
        Duration::from_millis(self.health_interval_ms)
    }

    /// [`Duration`] form of `execute_timeout_ms`.
    #[must_use]
    pub fn execute_timeout(&self) -> Duration {
        Duration::from_millis(self.execute_timeout_ms)
    }
}

fn default_connect_timeout_ms() -> u64 {
    5_000
}

fn default_health_interval_ms() -> u64 {
    30_000
}

fn default_execute_timeout_ms() -> u64 {
    60_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_sensible_values() {
        let cfg = PluginsConfig::default();
        assert!(cfg.endpoints.is_empty());
        assert_eq!(cfg.connect_timeout(), Duration::from_secs(5));
        assert_eq!(cfg.health_interval(), Duration::from_secs(30));
        assert_eq!(cfg.execute_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn deserialises_from_json_with_overrides() {
        let cfg: PluginsConfig = serde_json::from_str(
            r#"{"endpoints":["http://127.0.0.1:50061"],"execute_timeout_ms":15000}"#,
        )
        .unwrap();
        assert_eq!(cfg.endpoints, vec!["http://127.0.0.1:50061".to_owned()]);
        assert_eq!(cfg.execute_timeout(), Duration::from_secs(15));
        assert_eq!(cfg.health_interval(), Duration::from_secs(30));
    }
}
