//! Bootstrap `tracing` + optional OTLP export.
//!
//! Call [`init_tracing`] once at process start. The returned
//! [`TelemetryGuard`] must be kept alive for the lifetime of the process;
//! dropping it flushes the OTel batch exporter (so final spans don't drop
//! on shutdown).
//!
//! Behaviour:
//! - **`OtelConfig::endpoint == None`**: install a `tracing_subscriber::fmt`
//!   layer against stderr. No OTel machinery initialised.
//! - **`endpoint == Some(..)`**: install fmt **plus** a
//!   `tracing_opentelemetry::OpenTelemetryLayer` fed by an OTLP/gRPC
//!   exporter. Every `tracing` span/event becomes an OTel span/event; W3C
//!   trace context propagates automatically via the bridge.

use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource, runtime,
    trace::{Sampler, TracerProvider},
};
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::{OtelConfig, Protocol};

/// Handle that must be kept alive to keep telemetry flowing.
///
/// On drop: flushes the OTel batch exporter. If OTel is disabled this is a
/// zero-cost no-op.
pub struct TelemetryGuard {
    provider: Option<TracerProvider>,
}

impl std::fmt::Debug for TelemetryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryGuard").field("otel_enabled", &self.provider.is_some()).finish()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Best-effort flush. We deliberately do not `.await` here — a
            // Drop impl can't be async, and the batch exporter already
            // spawns its own flush task. Shutdown is synchronous in 0.27's
            // SDK API.
            let _ = provider.shutdown();
        }
    }
}

/// Initialise `tracing` + optional OTel exporter.
///
/// `default_filter` is the fallback `EnvFilter` when `ELENA_LOG` / `RUST_LOG`
/// is unset (e.g. `"warn,elena_gateway=info,elena_worker=info"`).
pub fn init_tracing(
    cfg: &OtelConfig,
    default_filter: &str,
) -> Result<TelemetryGuard, TelemetryError> {
    let filter = EnvFilter::try_from_env("ELENA_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(default_filter));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_level(true);

    if cfg.is_enabled() {
        let endpoint = cfg.endpoint.clone().unwrap_or_default();
        let exporter = build_exporter(&endpoint, cfg.protocol)?;
        let resource = build_resource(cfg);
        let sampler = match cfg.sample_ratio {
            Some(r) if (0.0..=1.0).contains(&r) => Sampler::TraceIdRatioBased(r),
            _ => Sampler::AlwaysOn,
        };

        let provider = TracerProvider::builder()
            .with_batch_exporter(exporter, runtime::Tokio)
            .with_resource(resource)
            .with_sampler(sampler)
            .build();
        let tracer = provider.tracer(cfg.service_name.clone());

        global::set_tracer_provider(provider.clone());

        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer.with_filter(EnvFilter::new("info")))
            .with(otel_layer)
            .try_init()
            .map_err(|e| TelemetryError::Init(e.to_string()))?;

        Ok(TelemetryGuard { provider: Some(provider) })
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .try_init()
            .map_err(|e| TelemetryError::Init(e.to_string()))?;
        Ok(TelemetryGuard { provider: None })
    }
}

fn build_exporter(endpoint: &str, protocol: Protocol) -> Result<SpanExporter, TelemetryError> {
    match protocol {
        Protocol::Grpc => SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| TelemetryError::Exporter(e.to_string())),
        Protocol::HttpProtobuf => Err(TelemetryError::Exporter(
            "HTTP/protobuf OTLP is not compiled in; enable the `http-proto` feature on \
             opentelemetry-otlp and rebuild. gRPC export is the default."
                .to_owned(),
        )),
    }
}

fn build_resource(cfg: &OtelConfig) -> Resource {
    let mut attrs: Vec<KeyValue> = vec![KeyValue::new("service.name", cfg.service_name.clone())];
    if let Some(v) = cfg.service_version.clone() {
        attrs.push(KeyValue::new("service.version", v));
    }
    if let Some(env) = cfg.deployment_environment.clone() {
        attrs.push(KeyValue::new("deployment.environment", env));
    }
    Resource::new(attrs)
}

/// Errors raised by [`init_tracing`].
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// Subscriber initialization failed (usually already initialised).
    #[error("tracing subscriber init failed: {0}")]
    Init(String),
    /// Building the OTLP exporter failed.
    #[error("OTLP exporter build failed: {0}")]
    Exporter(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_config_short_circuits_without_otel_setup() {
        // `init_tracing` installs a global subscriber that can only be set
        // once per process; cover the branching logic via the config helper.
        let cfg = OtelConfig::default();
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn http_protobuf_without_feature_gives_clear_error() {
        let err = build_exporter("http://x:4318", Protocol::HttpProtobuf).unwrap_err();
        assert!(err.to_string().contains("HTTP/protobuf"));
    }
}
