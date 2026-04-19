//! Phase-1 smoke binary.
//!
//! Proves the full data path works end-to-end against a real Postgres and
//! Redis. Intended to be run against the local dev services (`docker compose
//! up postgres redis`) or any environment where `ELENA_POSTGRES__URL` and
//! `ELENA_REDIS__URL` are set.
//!
//! Exit codes:
//! - `0` — Phase 1 is healthy end-to-end.
//! - non-zero — see the error line printed to stderr.

// A smoke binary is a linear script: splitting `run` into helpers would hurt
// readability, not improve it.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::too_many_lines)]

use std::collections::HashMap;
use std::process::ExitCode;

use chrono::Utc;
use elena_store::{Store, TenantRecord};
use elena_types::{
    BudgetLimits, CacheControl, CacheTtl, ContentBlock, Message, MessageId, MessageKind,
    PermissionSet, Role, StopReason, StoreError, TenantId, TenantTier, ToolCallId,
    ToolResultContent, Usage, UserId, WorkspaceId,
};

#[tokio::main]
async fn main() -> ExitCode {
    // A tiny hand-rolled tracing init — the smoke binary doesn't need the
    // full subscriber stack; info-level to stderr is enough.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run().await {
        Ok(()) => {
            println!("elena-phase1-smoke: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("elena-phase1-smoke: FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let cfg = elena_config::load()
        .or_else(|_| {
            // Fall back to the .env layer for local dev: if /etc/elena/elena.toml
            // is missing we try to read ELENA_CONFIG_FILE then the env alone.
            elena_config::load_with(None, None, true)
        })
        .map_err(|e| anyhow::anyhow!("config: {e}"))?;

    let store = Store::connect(&cfg).await.map_err(map_store)?;
    store.run_migrations().await.map_err(map_store)?;

    // 1. Tenant
    let tenant_id = TenantId::new();
    let tenant = TenantRecord {
        id: tenant_id,
        name: "smoke-test".into(),
        tier: TenantTier::Pro,
        budget: BudgetLimits::DEFAULT_PRO,
        permissions: PermissionSet::default(),
        metadata: HashMap::new(),
        allowed_plugin_ids: vec![],
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    store.tenants.upsert_tenant(&tenant).await.map_err(map_store)?;
    println!("  ✓ upserted tenant {tenant_id}");

    // 2. Thread
    let user_id = UserId::new();
    let workspace_id = WorkspaceId::new();
    let thread_id = store
        .threads
        .create_thread(tenant_id, user_id, workspace_id, Some("smoke test"))
        .await
        .map_err(map_store)?;
    println!("  ✓ created thread {thread_id}");

    // 3. User message
    let user_msg = Message::user_text(tenant_id, thread_id, "What's the weather?");
    let user_msg_id = user_msg.id;
    store.threads.append_message(&user_msg).await.map_err(map_store)?;

    // 4. Assistant message with thinking + tool_use
    let call_id = ToolCallId::new();
    let asst = Message {
        id: MessageId::new(),
        thread_id,
        tenant_id,
        role: Role::Assistant,
        kind: MessageKind::Assistant {
            stop_reason: Some(StopReason::ToolUse),
            is_api_error: false,
        },
        content: vec![
            ContentBlock::Thinking {
                thinking: "User wants weather — call the weather tool.".into(),
                signature: "sig-abc".into(),
                cache_control: Some(CacheControl::ephemeral().with_ttl(CacheTtl::FiveMin)),
            },
            ContentBlock::ToolUse {
                id: call_id,
                name: "weather".into(),
                input: serde_json::json!({"city": "NYC"}),
                cache_control: None,
            },
        ],
        created_at: Utc::now(),
        token_count: Some(42),
        parent_id: Some(user_msg_id),
    };
    let asst_id = asst.id;
    store.threads.append_message(&asst).await.map_err(map_store)?;

    // 5. Tool result
    let tool_result = Message {
        id: MessageId::new(),
        thread_id,
        tenant_id,
        role: Role::Tool,
        kind: MessageKind::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call_id,
            content: ToolResultContent::text("72F sunny"),
            is_error: false,
            cache_control: None,
        }],
        created_at: Utc::now(),
        token_count: Some(5),
        parent_id: Some(asst_id),
    };
    store.threads.append_message(&tool_result).await.map_err(map_store)?;
    println!("  ✓ appended 3 messages (user, assistant+thinking+tool_use, tool_result)");

    // 6. List and verify
    let listed =
        store.threads.list_messages(tenant_id, thread_id, 100, None).await.map_err(map_store)?;
    anyhow::ensure!(listed.len() == 3, "expected 3 messages, got {}", listed.len());
    match &listed[1].content[0] {
        ContentBlock::Thinking { signature, cache_control, .. } => {
            anyhow::ensure!(signature == "sig-abc", "signature lost");
            anyhow::ensure!(cache_control.is_some(), "cache_control lost");
        }
        other => anyhow::bail!("expected thinking block, got {other:?}"),
    }
    println!("  ✓ list returned 3 messages with thinking + cache_control preserved");

    // 7. Tenant isolation
    let other_tenant = TenantId::new();
    let other_list =
        store.threads.list_messages(other_tenant, thread_id, 100, None).await.map_err(map_store)?;
    anyhow::ensure!(other_list.is_empty(), "cross-tenant list must be empty");

    let mismatch = store.threads.get_message(other_tenant, user_msg_id).await;
    anyhow::ensure!(
        matches!(mismatch, Err(StoreError::TenantMismatch { .. })),
        "expected TenantMismatch, got {mismatch:?}"
    );
    println!("  ✓ tenant isolation holds (cross-tenant invisible / mismatch error)");

    // 8. Redis claim
    let claim_ok = store.cache.claim_thread(thread_id, "worker-1").await.map_err(map_store)?;
    anyhow::ensure!(claim_ok, "first claim should succeed");
    let claim_contended =
        store.cache.claim_thread(thread_id, "worker-2").await.map_err(map_store)?;
    anyhow::ensure!(!claim_contended, "second claim should fail");
    store.cache.release_thread(thread_id, "worker-2").await.map_err(map_store)?; // no-op CAS
    let owner = store.cache.claim_owner(thread_id).await.map_err(map_store)?;
    anyhow::ensure!(owner.as_deref() == Some("worker-1"), "wrong claim owner: {owner:?}");
    store.cache.release_thread(thread_id, "worker-1").await.map_err(map_store)?;
    println!("  ✓ redis claim CAS semantics correct");

    // 9. Similarity (empty in Phase 1)
    let sim = store
        .threads
        .similar_messages(tenant_id, thread_id, &vec![0.1_f32; 384], 5)
        .await
        .map_err(map_store)?;
    anyhow::ensure!(sim.is_empty(), "Phase 1 must return no similarity hits");
    println!("  ✓ similar_messages API stable (empty until Phase 4)");

    // 10. Usage recording
    let usage = Usage { input_tokens: 100, output_tokens: 50, ..Default::default() };
    store.tenants.record_usage(tenant_id, usage.clone()).await.map_err(map_store)?;
    store.tenants.record_usage(tenant_id, usage).await.map_err(map_store)?;
    let state = store.tenants.get_budget_state(tenant_id).await.map_err(map_store)?;
    anyhow::ensure!(state.tokens_used_today == 300, "usage not accumulated: {state:?}");
    println!("  ✓ usage recording accumulates ({} tokens today)", state.tokens_used_today);

    Ok(())
}

fn map_store(e: StoreError) -> anyhow::Error {
    anyhow::anyhow!(e)
}
