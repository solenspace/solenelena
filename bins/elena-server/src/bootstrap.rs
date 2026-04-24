//! Q5 — `LoopDeps` assembly extracted into a library-callable helper.
//!
//! The fields are unchanged from what `elena-server::main` and the
//! surviving smokes produce by hand; the helper just hides the
//! `Arc::new` boilerplate so a future field on `LoopDeps` lands in
//! exactly one place.

use std::sync::Arc;

use elena_config::ElenaConfig;
use elena_context::EpisodicMemory;
use elena_context::{ContextManager, ContextManagerOptions, NullEmbedder};
use elena_core::LoopDeps;
use elena_llm::{CacheAllowlist, CachePolicy, LlmClient};
use elena_observability::LoopMetrics;
use elena_plugins::PluginRegistry;
use elena_router::ModelRouter;
use elena_store::Store;
use elena_tools::ToolRegistry;
use elena_types::TenantTier;

/// Caller-supplied pieces that [`build_loop_deps`] glues together.
///
/// Each field is something the caller had to construct anyway (the
/// store needs migrations applied; the LLM multiplexer wires
/// per-provider HTTP clients; the plugin registry depends on the
/// caller's gRPC endpoint discovery). The helper just stitches them
/// into the `LoopDeps` shape `elena-core` consumes.
#[derive(Debug)]
pub struct BuildLoopDepsOptions {
    /// Persistence handles. Caller is responsible for `connect` +
    /// `run_migrations`.
    pub store: Arc<Store>,
    /// LLM multiplexer (or any [`LlmClient`] impl).
    pub llm: Arc<dyn LlmClient>,
    /// Tool registry, possibly pre-populated with built-in tools.
    pub tools: ToolRegistry,
    /// Plugin registry (caller calls `register_all` before passing in).
    pub plugins: Arc<PluginRegistry>,
    /// Process-wide metrics handle. Shared with the worker for
    /// per-pod metrics.
    pub metrics: Arc<LoopMetrics>,
    /// Number of cascade escalations the router may attempt within
    /// one thread before giving up.
    pub max_cascade_escalations: u32,
    /// Tier→model routing table.
    pub tier_routing: elena_config::TierModels,
    /// Full config (used for `defaults` + `rate_limits` payloads).
    pub cfg: Arc<ElenaConfig>,
}

/// Assemble a [`LoopDeps`] from the caller-supplied parts.
///
/// Intentionally synchronous — every field is already constructed.
/// Async would only buy us the chance to reach for the network and
/// hide the latency from operators, which is exactly what we don't
/// want here.
#[must_use]
pub fn build_loop_deps(opts: BuildLoopDepsOptions) -> Arc<LoopDeps> {
    let context =
        Arc::new(ContextManager::new(Arc::new(NullEmbedder), ContextManagerOptions::default()));
    let memory = Arc::new(EpisodicMemory::new(Arc::new(opts.store.episodes.clone())));
    let router = Arc::new(ModelRouter::new(opts.tier_routing, opts.max_cascade_escalations));

    Arc::new(LoopDeps {
        store: opts.store.clone(),
        llm: opts.llm,
        cache_policy: CachePolicy::new(TenantTier::Pro, CacheAllowlist::default()),
        tools: opts.tools,
        context,
        memory,
        router,
        plugins: opts.plugins,
        rate_limits: Arc::new(opts.cfg.rate_limits.clone()),
        metrics: opts.metrics,
        defaults: Arc::new(opts.cfg.defaults.clone()),
    })
}
