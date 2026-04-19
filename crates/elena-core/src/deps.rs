//! [`LoopDeps`] — the bundle of handles a loop run needs.
//!
//! Cheap to clone (`Arc` inside). Built once per worker and shared by every
//! [`run_loop`](crate::loop_driver::run_loop) invocation.

use std::sync::Arc;

use elena_config::{DefaultsConfig, RateLimitsConfig};
use elena_context::ContextManager;
use elena_llm::{CachePolicy, LlmClient};
use elena_memory::EpisodicMemory;
use elena_observability::LoopMetrics;
use elena_plugins::PluginRegistry;
use elena_router::ModelRouter;
use elena_store::Store;
use elena_tools::ToolRegistry;

/// All handles one loop run needs.
#[derive(Clone)]
pub struct LoopDeps {
    /// Persistence (Postgres + Redis).
    pub store: Arc<Store>,
    /// LLM streaming client — typically a
    /// [`LlmMultiplexer`](elena_llm::LlmMultiplexer) wrapping one or more
    /// concrete provider clients. Dispatch happens via
    /// [`LlmRequest::provider`](elena_llm::LlmRequest).
    pub llm: Arc<dyn LlmClient>,
    /// Cache policy latched at session start.
    pub cache_policy: CachePolicy,
    /// Registered tools.
    pub tools: ToolRegistry,
    /// Phase-4 context builder — embedding + retrieval + packing.
    pub context: Arc<ContextManager>,
    /// Phase-4 per-workspace memory.
    pub memory: Arc<EpisodicMemory>,
    /// Phase-4 heuristic model router.
    pub router: Arc<ModelRouter>,
    /// Phase-6 plugin registry. Owns gRPC clients, manifests, and the
    /// background health monitor. Plugin actions register themselves as
    /// synthetic tools inside `tools`, so the orchestrator sees them as
    /// ordinary [`elena_tools::Tool`]s.
    pub plugins: Arc<PluginRegistry>,
    /// Phase-7 metrics handles (RED counters, histograms). Cheap to clone.
    pub metrics: Arc<LoopMetrics>,
    /// Phase-7 rate-limit policy. Worker dispatch checks per-tenant RPM +
    /// inflight before claiming. Defaults to unlimited so existing
    /// deployments don't regress.
    pub rate_limits: Arc<RateLimitsConfig>,
    /// Loop knobs (max turns default, context window, max concurrent tools).
    pub defaults: Arc<DefaultsConfig>,
}

impl std::fmt::Debug for LoopDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoopDeps")
            .field("tools", &self.tools)
            .field("context", &self.context)
            .field("plugins", &self.plugins.manifests().len())
            .field("defaults", &*self.defaults)
            .finish_non_exhaustive()
    }
}
