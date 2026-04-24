//! End-to-end registry → tool → gRPC bridge test.
//!
//! Spins the *real* [`elena_connector_echo::EchoConnector`] in-process on
//! a free TCP port, registers it through [`PluginRegistry::register`], then
//! drives the synthesised `echo_reverse` tool through [`Tool::execute`] and
//! asserts:
//!   - the manifest validated and the tool registered
//!   - progress events fan out via [`ToolContext::progress_tx`]
//!   - the final body matches `{"reversed":"olleh"}`
//!   - the plugin's health gauge flipped to `Up`
//!
//! Mirrors the path Phase 6 ships, minus the LLM round-trip (which is the
//! `bins/elena-phase6-smoke` job).

// B1.6 — soak deprecated TenantTier warnings until the type is removed.
#![allow(deprecated)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::print_stderr)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use elena_connector_echo::EchoConnector;
use elena_plugins::{HealthState, PluginId, PluginRegistry, proto::pb};
use elena_tools::{ToolContext, ToolRegistry};
use elena_types::{
    BudgetLimits, PermissionSet, SessionId, StreamEvent, TenantContext, TenantId, TenantTier,
    ThreadId, ToolCallId, ToolResultContent, UserId, WorkspaceId,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;

async fn spawn_echo() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(pb::elena_plugin_server::ElenaPluginServer::new(EchoConnector))
            .serve_with_incoming(incoming)
            .await;
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    (addr, handle)
}

fn make_ctx() -> (ToolContext, mpsc::Receiver<StreamEvent>) {
    let (tx, rx) = mpsc::channel(16);
    let tenant = TenantContext {
        tenant_id: TenantId::new(),
        user_id: UserId::new(),
        workspace_id: WorkspaceId::new(),
        thread_id: ThreadId::new(),
        session_id: SessionId::new(),
        permissions: PermissionSet::default(),
        budget: BudgetLimits::default(),
        tier: TenantTier::Pro,
        plan: None,
        metadata: HashMap::new(),
    };
    let ctx = ToolContext {
        thread_id: tenant.thread_id,
        tenant,
        tool_call_id: ToolCallId::new(),
        cancel: CancellationToken::new(),
        progress_tx: tx,
        credentials: std::collections::BTreeMap::new(),
    };
    (ctx, rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn echo_connector_round_trip_through_registry() {
    let (addr, srv) = spawn_echo().await;
    let endpoint = format!("http://{addr}");

    let tools = ToolRegistry::new();
    let registry = PluginRegistry::empty(tools.clone());

    // Register and assert manifest came through.
    let plugin_id = registry.register(&endpoint).await.expect("register echo");
    assert_eq!(plugin_id, PluginId::new("echo").unwrap());
    assert_eq!(registry.manifests().len(), 1);
    assert_eq!(registry.health(&plugin_id), HealthState::Up);

    // The synthesised tool exists in the shared registry.
    let tool = tools.get("echo_reverse").expect("echo_reverse tool registered");
    assert_eq!(tool.name(), "echo_reverse");
    assert!(tool.is_read_only(&serde_json::json!({"word":"hello"})));

    // Drive the tool — it should round-trip through the gRPC bridge.
    let (ctx, mut rx) = make_ctx();
    let out = tool
        .execute(serde_json::json!({"word":"hello"}), ctx)
        .await
        .expect("echo_reverse should succeed");

    assert!(!out.is_error);
    let body = match &out.content {
        ToolResultContent::Text(text) => text.clone(),
        other @ ToolResultContent::Blocks(_) => panic!("expected text content, got {other:?}"),
    };
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("output is JSON");
    assert_eq!(parsed["reversed"], "olleh");

    // Two progress events should have been delivered.
    let mut progress_msgs = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let StreamEvent::ToolProgress { message, .. } = ev {
            progress_msgs.push(message);
        }
    }
    assert_eq!(progress_msgs, vec!["echoing".to_owned(), "halfway".to_owned()]);

    registry.shutdown().await;
    srv.abort();
}
