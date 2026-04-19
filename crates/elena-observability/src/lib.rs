//! Observability glue for Elena.
//!
//! This crate owns three concerns:
//!
//! 1. **Metrics.** A [`LoopMetrics`] bundle of Prometheus handles (counters,
//!    histograms) threaded on `LoopDeps.metrics`. Rendered at `/metrics` by
//!    the gateway.
//! 2. **Traces.** [`init_tracing`] bootstraps `tracing` + an OTel OTLP
//!    exporter when `[otel]` is configured. Otherwise a no-op `tracing` fmt
//!    subscriber is installed.
//! 3. **Trace propagation.** [`TraceMeta`] carries W3C `traceparent` /
//!    `tracestate` through `WorkRequest` (NATS) and plugin `RequestContext`
//!    (gRPC) so one turn produces one trace end-to-end.
//!
//! The crate is cheap to pull in: the OTel dependency graph only fires when
//! a real endpoint is configured. Tests use [`TestTelemetry`] which collects
//! spans into an in-memory `Vec` for assertions.

#![warn(missing_docs)]
// Pedantic lints fight the prometheus-macro expansion + OTel builder
// style more than they help; suppress at crate level.
#![allow(
    clippy::doc_markdown,
    clippy::unnecessary_literal_bound,
    clippy::missing_errors_doc,
    clippy::unnecessary_wraps,
    clippy::useless_vec,
    clippy::module_name_repetitions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::assigning_clones,
    clippy::missing_const_for_fn
)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod metrics;
pub mod trace;
pub mod tracing_init;

pub use config::OtelConfig;
pub use metrics::LoopMetrics;
pub use trace::TraceMeta;
pub use tracing_init::{TelemetryGuard, init_tracing};
