//! [`PluginRegistry`] — owns the clients, manifests, health gauges, and
//! background health monitor.
//!
//! On [`register`](Self::register), the registry connects to the endpoint,
//! fetches the manifest, validates it, and synthesises one
//! [`PluginActionTool`] per advertised action. Each synthesised tool is
//! inserted into the shared [`ToolRegistry`] — the orchestrator and LLM see
//! them as ordinary tools.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use elena_tools::ToolRegistry;
use tracing::info;

use crate::{
    action_tool::PluginActionTool,
    client::PluginClient,
    config::PluginsConfig,
    error::PluginError,
    health::{HEALTH_UNKNOWN, HEALTH_UP, HealthMonitor, HealthState},
    id::PluginId,
    manifest::PluginManifest,
};

/// Owns the set of registered plugins.
///
/// Cloning is cheap (inner state is `Arc`-shared) — the worker holds one
/// `Arc<PluginRegistry>` on its [`LoopDeps`](elena_core::LoopDeps).
#[derive(Debug, Clone)]
pub struct PluginRegistry {
    tools: ToolRegistry,
    clients: Arc<DashMap<PluginId, Arc<PluginClient>>>,
    manifests: Arc<DashMap<PluginId, PluginManifest>>,
    gauges: Arc<DashMap<PluginId, Arc<AtomicU8>>>,
    execute_timeout: Duration,
    monitor: Arc<Mutex<Option<HealthMonitor>>>,
    health_interval: Duration,
    connect_timeout: Duration,
    tls: Option<elena_auth::TlsConfig>,
}

impl PluginRegistry {
    /// Build an empty registry that shares `tools` with the orchestrator.
    #[must_use]
    pub fn empty(tools: ToolRegistry) -> Self {
        Self::with_config(tools, &PluginsConfig::default())
    }

    /// Build an empty registry with configured timeouts.
    #[must_use]
    pub fn with_config(tools: ToolRegistry, cfg: &PluginsConfig) -> Self {
        Self {
            tools,
            clients: Arc::new(DashMap::new()),
            manifests: Arc::new(DashMap::new()),
            gauges: Arc::new(DashMap::new()),
            execute_timeout: cfg.execute_timeout(),
            monitor: Arc::new(Mutex::new(None)),
            health_interval: cfg.health_interval(),
            connect_timeout: cfg.connect_timeout(),
            tls: cfg.tls.clone(),
        }
    }

    /// Register a single plugin endpoint: connect, fetch manifest, synthesise
    /// tools, insert into the shared [`ToolRegistry`].
    ///
    /// Re-registering the same `PluginId` replaces the client + tools. Tool
    /// name collisions *across different plugins* return
    /// [`PluginError::NameCollision`].
    pub async fn register(&self, endpoint: &str) -> Result<PluginId, PluginError> {
        let client = Arc::new(
            PluginClient::connect_with_tls(endpoint, self.connect_timeout, self.tls.as_ref())
                .await?,
        );
        let manifest = self.fetch_manifest(&client).await?;
        let plugin_id = manifest.id.clone();

        // Name-collision check against *other* plugins. Re-registering the
        // same plugin is allowed — the loop below overwrites those tools.
        for action in &manifest.actions {
            let synthesised = manifest.tool_name_for(&action.name);
            for entry in self.manifests.iter() {
                if entry.key() == &plugin_id {
                    continue;
                }
                if Self::tool_name_owned_by(entry.value(), &synthesised) {
                    return Err(PluginError::NameCollision { tool_name: synthesised });
                }
            }
        }

        // Schema-drift check. On re-registration, refuse if any
        // action's input schema narrowed vs the previous revision —
        // callers would then emit inputs the sidecar rejects. Widening
        // (adding optional fields) is allowed; narrowing is not.
        if let Some(prev) = self.manifests.get(&plugin_id) {
            for new_action in &manifest.actions {
                if let Some(prev_action) = prev.actions.iter().find(|a| a.name == new_action.name)
                    && let Some(reason) =
                        schema_narrowed(&prev_action.input_schema, &new_action.input_schema)
                {
                    return Err(PluginError::SchemaDrift {
                        plugin_id: plugin_id.clone(),
                        action: new_action.name.clone(),
                        reason,
                    });
                }
            }
        }

        // Health gauge starts Unknown and will flip on the first probe.
        let gauge = Arc::new(AtomicU8::new(HEALTH_UNKNOWN));
        // Mark Up immediately since `fetch_manifest` succeeded.
        gauge.store(HEALTH_UP, Ordering::Relaxed);

        // Synthesise + register each action-tool. Re-registrations with
        // the same plugin id overwrite via `ToolRegistry::register_arc`'s
        // last-write-wins semantics, which is correct as long as the
        // same action set is re-advertised. Connectors don't shrink
        // their action set at runtime; explicit removal is not yet
        // supported here.
        for action in &manifest.actions {
            let tool = PluginActionTool::remote(
                plugin_id.clone(),
                action.name.clone(),
                action.description.clone(),
                action.input_schema.clone(),
                action.is_read_only,
                Arc::clone(&client),
                self.execute_timeout,
                Some(Arc::clone(&gauge)),
            );
            self.tools.register_arc(Arc::new(tool));
        }

        self.clients.insert(plugin_id.clone(), client);
        self.manifests.insert(plugin_id.clone(), manifest);
        self.gauges.insert(plugin_id.clone(), gauge);

        info!(plugin = %plugin_id, %endpoint, actions = self.manifests.get(&plugin_id).map_or(0, |m| m.actions.len()),
              "plugin registered");

        self.ensure_monitor_started();
        Ok(plugin_id)
    }

    /// X7 — Register an in-process embedded plugin (no gRPC dial).
    ///
    /// Calls the executor's `manifest()` to discover actions, then
    /// installs one `PluginActionTool` per action wired to
    /// `PluginBackend::Embedded(executor)`. The connector code runs
    /// in the same tokio runtime as the loop — no socket round-trip,
    /// no tonic server.
    ///
    /// Embedded plugins skip the background health monitor: an
    /// in-process connector that's broken means the process is
    /// broken, and there's no socket to drop.
    pub async fn register_embedded(
        &self,
        executor: Arc<dyn crate::embedded::EmbeddedExecutor>,
    ) -> Result<PluginId, PluginError> {
        let wire_manifest = executor.manifest().await.map_err(|s| PluginError::Manifest {
            plugin_id: "<unknown>".to_owned(),
            reason: s.message().to_owned(),
        })?;
        let manifest = PluginManifest::from_wire(wire_manifest)?;
        let plugin_id = manifest.id.clone();

        // Same name-collision check as the remote path.
        for action in &manifest.actions {
            let synthesised = manifest.tool_name_for(&action.name);
            for entry in self.manifests.iter() {
                if entry.key() == &plugin_id {
                    continue;
                }
                if Self::tool_name_owned_by(entry.value(), &synthesised) {
                    return Err(PluginError::NameCollision { tool_name: synthesised });
                }
            }
        }

        let backend = Arc::new(crate::embedded::PluginBackend::Embedded(executor));
        for action in &manifest.actions {
            let tool = PluginActionTool::new(
                plugin_id.clone(),
                action.name.clone(),
                action.description.clone(),
                action.input_schema.clone(),
                action.is_read_only,
                Arc::clone(&backend),
                self.execute_timeout,
                None, // no health gauge for embedded
            );
            self.tools.register_arc(Arc::new(tool));
        }
        self.manifests.insert(plugin_id.clone(), manifest);
        info!(plugin = %plugin_id, "embedded plugin registered (in-process, no gRPC)");
        Ok(plugin_id)
    }

    /// Register every endpoint in the configuration. Returns the list of IDs
    /// registered; a failure on any endpoint is returned immediately and the
    /// registry is left in a partial state (operator is expected to restart).
    pub async fn register_all(&self, cfg: &PluginsConfig) -> Result<Vec<PluginId>, PluginError> {
        let mut ids = Vec::with_capacity(cfg.endpoints.len());
        for ep in &cfg.endpoints {
            ids.push(self.register(ep).await?);
        }
        Ok(ids)
    }

    /// Snapshot of registered manifests.
    #[must_use]
    pub fn manifests(&self) -> Vec<PluginManifest> {
        self.manifests.iter().map(|entry| entry.value().clone()).collect()
    }

    /// Current health state of a plugin.
    #[must_use]
    pub fn health(&self, plugin_id: &PluginId) -> HealthState {
        self.gauges
            .get(plugin_id)
            .map_or(HealthState::Unknown, |g| g.value().load(Ordering::Relaxed).into())
    }

    /// Stop the background health monitor. Safe to call multiple times.
    pub async fn shutdown(&self) {
        let monitor = match self.monitor.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => None,
        };
        if let Some(mut m) = monitor {
            m.shutdown().await;
        }
    }

    // ------ internals -------------------------------------------------

    async fn fetch_manifest(&self, client: &PluginClient) -> Result<PluginManifest, PluginError> {
        let mut rpc = client.rpc();
        let wire = rpc
            .get_manifest(())
            .await
            .map_err(|status| PluginError::Manifest {
                plugin_id: String::new(),
                reason: format!("GetManifest failed: {status}"),
            })?
            .into_inner();
        PluginManifest::from_wire(wire)
    }

    fn tool_name_owned_by(manifest: &PluginManifest, tool_name: &str) -> bool {
        manifest.actions.iter().any(|a| manifest.tool_name_for(&a.name) == tool_name)
    }

    fn ensure_monitor_started(&self) {
        let Ok(mut slot) = self.monitor.lock() else {
            return;
        };
        if slot.is_none() {
            let monitor = HealthMonitor::spawn(
                Arc::clone(&self.clients),
                Arc::clone(&self.gauges),
                self.health_interval,
            );
            *slot = Some(monitor);
        }
    }
}

/// Return `Some(reason)` if `new` narrows `old` — i.e. the new schema
/// would reject inputs that the old schema accepted.
///
/// The check is deliberately conservative: it flags the two narrowing
/// patterns that matter for LLM-emitted tool args: (a) properties
/// removed outright, (b) previously-optional properties marked
/// required. Everything else (including type-tightening) passes
/// through; richer structural drift lands in v1.0.x.
fn schema_narrowed(old: &serde_json::Value, new: &serde_json::Value) -> Option<String> {
    let old_props = old.get("properties").and_then(|p| p.as_object())?;
    let new_props = new.get("properties").and_then(|p| p.as_object());

    if let Some(new_props) = new_props {
        for name in old_props.keys() {
            if !new_props.contains_key(name) {
                return Some(format!("property `{name}` removed"));
            }
        }
    } else {
        return Some("`properties` object dropped from schema".to_owned());
    }

    let old_required: std::collections::HashSet<&str> = old
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let new_required: std::collections::HashSet<&str> = new
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if let Some(name) = new_required.difference(&old_required).next() {
        return Some(format!("property `{name}` is newly required"));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::net::TcpListener;
    use tonic::{Request, Response, Status, transport::Server};

    use super::*;
    use crate::proto::pb;

    // ----- configurable manifest fixture -----------------------------

    #[derive(Debug, Clone, Default)]
    struct Fixture {
        manifest: pb::PluginManifest,
    }

    #[tonic::async_trait]
    impl pb::elena_plugin_server::ElenaPlugin for Fixture {
        async fn get_manifest(
            &self,
            _: Request<()>,
        ) -> Result<Response<pb::PluginManifest>, Status> {
            Ok(Response::new(self.manifest.clone()))
        }
        type ExecuteStream =
            tokio_stream::wrappers::ReceiverStream<Result<pb::PluginResponse, Status>>;
        async fn execute(
            &self,
            _: Request<pb::PluginRequest>,
        ) -> Result<Response<Self::ExecuteStream>, Status> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }
        async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
            Ok(Response::new(pb::HealthResponse { ok: true, detail: String::new() }))
        }
    }

    async fn spawn(manifest: pb::PluginManifest) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fx = Fixture { manifest };
        let handle = tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = Server::builder()
                .add_service(pb::elena_plugin_server::ElenaPluginServer::new(fx))
                .serve_with_incoming(incoming)
                .await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        (addr, handle)
    }

    fn manifest(id: &str, actions: &[&str]) -> pb::PluginManifest {
        pb::PluginManifest {
            id: id.into(),
            name: format!("{id} plugin"),
            version: "0.1.0".into(),
            actions: actions
                .iter()
                .map(|a| pb::ActionDefinition {
                    name: (*a).to_owned(),
                    description: format!("{a} action"),
                    input_schema: r#"{"type":"object"}"#.into(),
                    output_schema: r#"{"type":"object"}"#.into(),
                    is_read_only: true,
                })
                .collect(),
            required_credentials: vec![],
        }
    }

    // ----- tests -----------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn register_synthesises_tools() {
        let (addr, srv) = spawn(manifest("echo", &["reverse", "upper"])).await;
        let tools = ToolRegistry::new();
        let reg = PluginRegistry::empty(tools.clone());

        let id = reg.register(&format!("http://{addr}")).await.unwrap();
        assert_eq!(id.as_str(), "echo");

        assert!(tools.get("echo_reverse").is_some());
        assert!(tools.get("echo_upper").is_some());
        assert_eq!(tools.len(), 2);
        assert_eq!(reg.manifests().len(), 1);
        assert_eq!(reg.health(&id), HealthState::Up);

        reg.shutdown().await;
        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_register_same_plugin_replaces_tools() {
        let (addr, srv) = spawn(manifest("echo", &["reverse"])).await;
        let tools = ToolRegistry::new();
        let reg = PluginRegistry::empty(tools.clone());

        let _ = reg.register(&format!("http://{addr}")).await.unwrap();
        let _ = reg.register(&format!("http://{addr}")).await.unwrap();

        assert!(tools.get("echo_reverse").is_some());
        assert_eq!(reg.manifests().len(), 1);

        reg.shutdown().await;
        srv.abort();
    }

    /// E4 — Schema-drift detection on re-registration.
    ///
    /// A plugin is registered with an action whose `input_schema`
    /// requires field `a` only. A second sidecar at a different
    /// endpoint advertises the same plugin id but with `input_schema`
    /// that newly requires field `b` — a narrowing change that would
    /// reject prior LLM-emitted tool calls. Re-registration must fail
    /// with `PluginError::SchemaDrift`, and the original tool stays
    /// live in the registry (the bad upgrade is rejected, not applied).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e4_re_register_with_narrowed_schema_returns_schema_drift() {
        // Manifest A: input requires only `a`.
        let mut manifest_a = manifest("driftplug", &["run"]);
        manifest_a.actions[0].input_schema = r#"{
            "type":"object",
            "properties":{"a":{"type":"string"},"b":{"type":"string"}},
            "required":["a"]
        }"#
        .into();

        // Manifest A': adds `b` to required (narrowing — old tool calls
        // that omitted `b` would now be rejected).
        let mut manifest_b = manifest("driftplug", &["run"]);
        manifest_b.actions[0].input_schema = r#"{
            "type":"object",
            "properties":{"a":{"type":"string"},"b":{"type":"string"}},
            "required":["a","b"]
        }"#
        .into();

        let (addr_a, srv_a) = spawn(manifest_a).await;
        let (addr_b, srv_b) = spawn(manifest_b).await;

        let tools = ToolRegistry::new();
        let reg = PluginRegistry::empty(tools.clone());

        // Initial registration succeeds.
        let id = reg.register(&format!("http://{addr_a}")).await.unwrap();
        assert_eq!(id.as_str(), "driftplug");
        assert!(tools.get("driftplug_run").is_some());

        // Re-registration with the narrowed schema must error.
        let err = reg.register(&format!("http://{addr_b}")).await.unwrap_err();
        match err {
            PluginError::SchemaDrift { plugin_id, action, ref reason } => {
                assert_eq!(plugin_id.as_str(), "driftplug");
                assert_eq!(action, "run");
                assert!(reason.contains("newly required"), "got reason: {reason}");
            }
            other => panic!("expected SchemaDrift, got {other:?}"),
        }

        // Original tool remains live (the failed re-registration did
        // not unregister or replace it).
        assert!(
            tools.get("driftplug_run").is_some(),
            "schema-drift rejection must NOT take down the live tool"
        );

        reg.shutdown().await;
        srv_a.abort();
        srv_b.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn name_collision_between_plugins_errors() {
        // Plugin `a` exposes action `b_c` → synthesises to `a_b_c`.
        // Plugin `a_b` exposes action `c`  → also synthesises to `a_b_c`.
        let (addr_first, srv_first) = spawn(manifest("a", &["b_c"])).await;
        let (addr_second, srv_second) = spawn(manifest("a_b", &["c"])).await;

        let tools = ToolRegistry::new();
        let reg = PluginRegistry::empty(tools.clone());

        reg.register(&format!("http://{addr_first}")).await.unwrap();
        let err = reg.register(&format!("http://{addr_second}")).await.unwrap_err();
        assert!(
            matches!(err, PluginError::NameCollision { ref tool_name } if tool_name == "a_b_c"),
            "expected NameCollision for `a_b_c`, got {err:?}"
        );

        reg.shutdown().await;
        srv_first.abort();
        srv_second.abort();
    }
}
