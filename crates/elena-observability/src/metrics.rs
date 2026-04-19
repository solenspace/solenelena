//! [`LoopMetrics`] — Prometheus handles threaded on `LoopDeps.metrics`.
//!
//! Handles are cheap (`Arc` inside). One `LoopMetrics` is built per process
//! at startup; every crate that has one can call `.turns_total.inc()` or
//! `.plugin_rpc_duration_seconds.with_label_values(...).observe(ms)`.
//!
//! All metrics live in one shared [`prometheus::Registry`] so the gateway's
//! `/metrics` endpoint can render them in one pass.

use std::sync::Arc;

use prometheus::{
    CounterVec, HistogramVec, IntCounter, IntCounterVec, IntGaugeVec, Registry, histogram_opts,
    opts, register_counter_vec_with_registry, register_histogram_vec_with_registry,
    register_int_counter_vec_with_registry, register_int_counter_with_registry,
    register_int_gauge_vec_with_registry,
};

/// All of Elena's RED + USE metrics in one bundle.
#[derive(Clone)]
pub struct LoopMetrics {
    registry: Arc<Registry>,

    /// Total number of turns the worker has processed (completed or failed).
    pub turns_total: IntCounter,

    /// Turn wall-clock duration, seconds. Buckets tuned for LLM latencies.
    pub turn_duration_seconds: HistogramVec,

    /// Tool calls dispatched, keyed by `tool` (name), `outcome` (ok|error).
    pub tool_calls_total: IntCounterVec,

    /// LLM token usage, keyed by `provider`, `model`, `kind`
    /// (input|output|cache_read|cache_write).
    pub llm_tokens_total: CounterVec,

    /// Plugin gRPC RPC duration, seconds, keyed by `plugin`, `action`,
    /// `code` (ok|unavailable|invalid_argument|timeout|...).
    pub plugin_rpc_duration_seconds: HistogramVec,

    /// Rate-limit rejections, keyed by `scope`
    /// (tenant_rpm|tenant_inflight|provider_concurrency|plugin_concurrency).
    pub rate_limit_rejections_total: IntCounterVec,

    /// In-flight loops per worker. Used to observe saturation.
    pub loops_in_flight: IntGaugeVec,

    /// Total audit events dropped — either from channel overflow or DB
    /// write failure. **Operators should alert on any non-zero value**:
    /// a non-zero counter means the audit log is no longer a complete
    /// record of what the agent did, which breaks the compliance story.
    pub audit_drops_total: IntCounter,
}

impl std::fmt::Debug for LoopMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopMetrics").finish_non_exhaustive()
    }
}

impl LoopMetrics {
    /// Build a fresh metrics bundle + its own registry.
    pub fn new() -> Result<Self, prometheus::Error> {
        let registry = Arc::new(Registry::new());
        Self::build(registry)
    }

    /// Build on top of a caller-supplied registry (e.g. for tests that want
    /// to inspect a single shared registry across fixtures).
    pub fn with_registry(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        Self::build(registry)
    }

    fn build(registry: Arc<Registry>) -> Result<Self, prometheus::Error> {
        let turns_total = register_int_counter_with_registry!(
            opts!("elena_turns_total", "Total loop turns processed."),
            registry
        )?;

        let turn_duration_seconds = register_histogram_vec_with_registry!(
            histogram_opts!(
                "elena_turn_duration_seconds",
                "Turn wall-clock duration.",
                vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0]
            ),
            &["outcome"],
            registry
        )?;

        let tool_calls_total = register_int_counter_vec_with_registry!(
            opts!("elena_tool_calls_total", "Tool calls dispatched."),
            &["tool", "outcome"],
            registry
        )?;

        let llm_tokens_total = register_counter_vec_with_registry!(
            opts!("elena_llm_tokens_total", "LLM token usage."),
            &["provider", "model", "kind"],
            registry
        )?;

        let plugin_rpc_duration_seconds = register_histogram_vec_with_registry!(
            histogram_opts!(
                "elena_plugin_rpc_duration_seconds",
                "Plugin gRPC Execute RPC duration.",
                vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0]
            ),
            &["plugin", "action", "code"],
            registry
        )?;

        let rate_limit_rejections_total = register_int_counter_vec_with_registry!(
            opts!("elena_rate_limit_rejections_total", "Rate-limit rejections."),
            &["scope"],
            registry
        )?;

        let loops_in_flight = register_int_gauge_vec_with_registry!(
            opts!("elena_loops_in_flight", "Currently-executing loops per worker."),
            &["worker_id"],
            registry
        )?;

        let audit_drops_total = register_int_counter_with_registry!(
            opts!(
                "elena_audit_drops_total",
                "Audit events dropped (channel overflow or DB write failure). \
                 Non-zero means the audit log is incomplete — alert on this."
            ),
            registry
        )?;

        Ok(Self {
            registry,
            turns_total,
            turn_duration_seconds,
            tool_calls_total,
            llm_tokens_total,
            plugin_rpc_duration_seconds,
            rate_limit_rejections_total,
            loops_in_flight,
            audit_drops_total,
        })
    }

    /// The shared registry — the gateway renders this at `/metrics`.
    #[must_use]
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// Render the registry to Prometheus text format.
    pub fn render_text(&self) -> Result<String, prometheus::Error> {
        use prometheus::Encoder;
        let encoder = prometheus::TextEncoder::new();
        let mut buf = Vec::new();
        encoder.encode(&self.registry.gather(), &mut buf)?;
        String::from_utf8(buf).map_err(|e| prometheus::Error::Msg(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_registered_and_usable() {
        let m = LoopMetrics::new().unwrap();
        m.turns_total.inc();
        m.tool_calls_total.with_label_values(&["echo_reverse", "ok"]).inc();
        m.llm_tokens_total
            .with_label_values(&["groq", "llama-3.3-70b-versatile", "input"])
            .inc_by(12.0);
        m.rate_limit_rejections_total.with_label_values(&["tenant_rpm"]).inc();

        let text = m.render_text().unwrap();
        assert!(text.contains("elena_turns_total"));
        assert!(text.contains("elena_tool_calls_total{"));
        assert!(text.contains("elena_llm_tokens_total{"));
        assert!(text.contains("elena_rate_limit_rejections_total{"));
    }

    #[test]
    fn histogram_observation_shows_up_in_text() {
        let m = LoopMetrics::new().unwrap();
        m.plugin_rpc_duration_seconds.with_label_values(&["echo", "reverse", "ok"]).observe(0.02);
        let text = m.render_text().unwrap();
        assert!(text.contains("elena_plugin_rpc_duration_seconds_bucket{"));
        assert!(text.contains("elena_plugin_rpc_duration_seconds_count{"));
    }

    #[test]
    fn separate_instances_have_separate_registries() {
        let a = LoopMetrics::new().unwrap();
        let b = LoopMetrics::new().unwrap();
        a.turns_total.inc_by(3);
        let a_text = a.render_text().unwrap();
        let b_text = b.render_text().unwrap();
        assert!(a_text.contains("elena_turns_total 3"));
        assert!(b_text.contains("elena_turns_total 0"));
    }
}
