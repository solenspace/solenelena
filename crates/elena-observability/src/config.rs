//! OpenTelemetry configuration.

use serde::Deserialize;

/// Operator-facing OTel configuration.
///
/// When `endpoint` is `None`, tracing stays local-only (stderr fmt
/// subscriber). Set `endpoint` to an OTLP-speaking collector URL
/// (`http://otel-collector:4317` typically) to enable trace export.
#[derive(Debug, Clone, Deserialize)]
pub struct OtelConfig {
    /// OTLP endpoint. `None` disables OTel export entirely.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Service name attached to every span as the `service.name` resource
    /// attribute. Operators override per-binary (e.g. `elena-gateway`,
    /// `elena-worker`, `elena-connector-echo`).
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// Service version. Defaults to the crate version of the binary that
    /// initialises telemetry.
    #[serde(default)]
    pub service_version: Option<String>,

    /// Deployment environment name (`dev`, `staging`, `prod`). Attached as
    /// `deployment.environment` resource attribute.
    #[serde(default)]
    pub deployment_environment: Option<String>,

    /// Transport protocol. gRPC (OTLP/gRPC) is the default and preferred;
    /// HTTP/protobuf is supported for collectors behind proxies that
    /// don't speak HTTP/2.
    #[serde(default)]
    pub protocol: Protocol,

    /// Optional sampler ratio in `[0.0, 1.0]`. `None` = always-on.
    #[serde(default)]
    pub sample_ratio: Option<f64>,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            service_name: default_service_name(),
            service_version: None,
            deployment_environment: None,
            protocol: Protocol::default(),
            sample_ratio: None,
        }
    }
}

impl OtelConfig {
    /// True if OTLP export is enabled (i.e. endpoint is set and non-empty).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.endpoint.as_deref().is_some_and(|s| !s.is_empty())
    }
}

fn default_service_name() -> String {
    "elena".to_owned()
}

/// OTLP transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OTLP/gRPC (default).
    #[default]
    Grpc,
    /// OTLP/HTTP + protobuf.
    HttpProtobuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_no_endpoint() {
        assert!(!OtelConfig::default().is_enabled());
    }

    #[test]
    fn enabled_when_endpoint_set() {
        let cfg =
            OtelConfig { endpoint: Some("http://localhost:4317".into()), ..OtelConfig::default() };
        assert!(cfg.is_enabled());
    }

    #[test]
    fn empty_endpoint_counts_as_disabled() {
        let cfg = OtelConfig { endpoint: Some(String::new()), ..OtelConfig::default() };
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn deserialises_from_json_with_overrides() {
        let raw = serde_json::json!({
            "endpoint": "http://otel:4317",
            "service_name": "elena-worker",
            "protocol": "grpc",
            "sample_ratio": 0.1
        });
        let cfg: OtelConfig = serde_json::from_value(raw).unwrap();
        assert!(cfg.is_enabled());
        assert_eq!(cfg.service_name, "elena-worker");
        assert_eq!(cfg.protocol, Protocol::Grpc);
        assert_eq!(cfg.sample_ratio, Some(0.1));
    }
}
