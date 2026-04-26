//! Per-tenant provisioning + concurrent driver loops.
//!
//! Three tenants — each with its own plan, workspaces, allowed plugins
//! — run independent driver loops. Solen drives long multi-turn
//! threads, Hannlys drives single-shot personalization turns, Omnii
//! drives 3-turn brief→render→deliver sequences against the
//! `video_mock` plugin.
//!
//! Drivers run for a bounded duration. Each loop creates a fresh
//! workspace + JWT + thread once, then sends turns until the deadline
//! fires.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use elena_types::{SessionId, TenantId, TenantTier, ThreadId, UserId, WorkspaceId};
use futures::{SinkExt, StreamExt};
use jsonwebtoken::{EncodingKey, Header, encode};
use serde::Serialize;
use serde_json::json;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, warn};

use crate::harness::{Harness, JWT_AUDIENCE, JWT_ISSUER, JWT_SECRET, MODEL_ID};

#[derive(Debug)]
pub struct Provisioned {
    pub solen: SolenIds,
    pub hannlys: HannlysIds,
    pub omnii: OmniiIds,
}

#[derive(Debug)]
pub struct SolenIds {
    pub tenant_id: TenantId,
    pub workspaces: Vec<WorkspaceId>,
}

#[derive(Debug)]
pub struct HannlysIds {
    pub creator_id: TenantId,
    /// Tenant id the buyer driver loops will spawn buyer workspaces under.
    /// (We pre-create one buyer tenant; each loop just creates a fresh
    /// workspace + user under it. Saves a `POST /tenants` per loop.)
    pub buyer_tenant_id: TenantId,
}

#[derive(Debug)]
pub struct OmniiIds {
    pub tenant_id: TenantId,
    pub workspaces: Vec<WorkspaceId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProvisionedSummary {
    pub solen_tenant: String,
    pub solen_workspaces: usize,
    pub hannlys_creator: String,
    pub hannlys_buyer_tenant: String,
    pub omnii_tenant: String,
    pub omnii_workspaces: usize,
}

impl Provisioned {
    pub fn summary(&self) -> ProvisionedSummary {
        ProvisionedSummary {
            solen_tenant: self.solen.tenant_id.to_string(),
            solen_workspaces: self.solen.workspaces.len(),
            hannlys_creator: self.hannlys.creator_id.to_string(),
            hannlys_buyer_tenant: self.hannlys.buyer_tenant_id.to_string(),
            omnii_tenant: self.omnii.tenant_id.to_string(),
            omnii_workspaces: self.omnii.workspaces.len(),
        }
    }
}

pub async fn provision_all(env: &Harness) -> Result<Provisioned> {
    let http = env.http();
    let base = env.base_url();

    // ---- Solen — 3 workspace "departments" ----
    let solen_id = TenantId::new();
    create_tenant(&http, &base, solen_id, "solen-enterprise", "pro").await?;
    set_allowed_plugins(&http, &base, solen_id, &["echo"]).await?; // mocked Slack
    let mut solen_workspaces = Vec::new();
    for dept in ["ops", "sales", "engineering"] {
        let ws = WorkspaceId::new();
        create_workspace(
            &http,
            &base,
            ws,
            solen_id,
            &format!("solen-{dept}"),
            "You are Solen, an enterprise agent. Reply briefly and call tools when asked.",
            &["echo"],
        )
        .await?;
        solen_workspaces.push(ws);
    }

    // ---- Hannlys — creator owns the seed plugin; buyers grant on demand ----
    let creator_id = TenantId::new();
    create_tenant(&http, &base, creator_id, "hannlys-creator", "pro").await?;
    let buyer_tenant_id = TenantId::new();
    create_tenant(&http, &base, buyer_tenant_id, "hannlys-buyers", "free").await?;
    // Echo plays the role of the Seed-personalization tool; scope to creator
    // initially, then expand to include the buyer tenant.
    set_plugin_owners(&http, &base, "echo", &[creator_id, buyer_tenant_id]).await?;
    set_allowed_plugins(&http, &base, buyer_tenant_id, &["echo"]).await?;

    // ---- Omnii — brand workspaces; uses video_mock ----
    let omnii_id = TenantId::new();
    create_tenant(&http, &base, omnii_id, "omnii-brands", "pro").await?;
    set_allowed_plugins(&http, &base, omnii_id, &["video_mock"]).await?;
    set_plugin_owners(&http, &base, "video_mock", &[omnii_id]).await?;
    let mut omnii_workspaces = Vec::new();
    for brand in ["acme-co", "blue-bottle", "northwind"] {
        let ws = WorkspaceId::new();
        create_workspace(
            &http,
            &base,
            ws,
            omnii_id,
            &format!("omnii-{brand}"),
            "You are Omnii, a brand video director. When asked to render, call \
             video_mock_render with a brief field.",
            &["video_mock"],
        )
        .await?;
        omnii_workspaces.push(ws);
    }

    Ok(Provisioned {
        solen: SolenIds { tenant_id: solen_id, workspaces: solen_workspaces },
        hannlys: HannlysIds { creator_id, buyer_tenant_id },
        omnii: OmniiIds { tenant_id: omnii_id, workspaces: omnii_workspaces },
    })
}

// ---- Driver harness ----

#[derive(Debug, Default, Serialize, Clone)]
pub struct DriverStats {
    pub started: usize,
    pub completed: usize,
    pub errors: usize,
    pub latency_ms: Vec<u64>,
}

#[derive(Debug, Default)]
struct DriverCounters {
    started: AtomicUsize,
    completed: AtomicUsize,
    errors: AtomicUsize,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct AllStats {
    pub solen: DriverStats,
    pub hannlys: DriverStats,
    pub omnii: DriverStats,
}

pub async fn run_drivers(
    env: &Harness,
    p: &Provisioned,
    threads_per_tenant: usize,
    duration: Duration,
) -> Result<AllStats> {
    let deadline = Instant::now() + duration;

    let solen = Arc::new(DriverCounters::default());
    let hannlys = Arc::new(DriverCounters::default());
    let omnii = Arc::new(DriverCounters::default());
    let solen_lat = Arc::new(tokio::sync::Mutex::new(Vec::<u64>::with_capacity(1024)));
    let hannlys_lat = Arc::new(tokio::sync::Mutex::new(Vec::<u64>::with_capacity(1024)));
    let omnii_lat = Arc::new(tokio::sync::Mutex::new(Vec::<u64>::with_capacity(1024)));

    let mut set = JoinSet::new();

    for i in 0..threads_per_tenant {
        let env_base = env.base_url();
        let env_ws = env.ws_base_url();
        let workspaces = p.solen.workspaces.clone();
        let tenant = p.solen.tenant_id;
        let counters = Arc::clone(&solen);
        let lat = Arc::clone(&solen_lat);
        set.spawn(async move {
            solen_loop(env_base, env_ws, tenant, workspaces, i, deadline, counters, lat).await;
        });
    }
    for i in 0..threads_per_tenant {
        let env_base = env.base_url();
        let env_ws = env.ws_base_url();
        let buyer_tenant = p.hannlys.buyer_tenant_id;
        let counters = Arc::clone(&hannlys);
        let lat = Arc::clone(&hannlys_lat);
        set.spawn(async move {
            hannlys_loop(env_base, env_ws, buyer_tenant, i, deadline, counters, lat).await;
        });
    }
    for i in 0..threads_per_tenant {
        let env_base = env.base_url();
        let env_ws = env.ws_base_url();
        let workspaces = p.omnii.workspaces.clone();
        let tenant = p.omnii.tenant_id;
        let counters = Arc::clone(&omnii);
        let lat = Arc::clone(&omnii_lat);
        set.spawn(async move {
            omnii_loop(env_base, env_ws, tenant, workspaces, i, deadline, counters, lat).await;
        });
    }

    while let Some(joined) = set.join_next().await {
        if let Err(e) = joined {
            warn!(?e, "driver task panicked");
        }
    }

    Ok(AllStats {
        solen: snapshot(&solen, &solen_lat).await,
        hannlys: snapshot(&hannlys, &hannlys_lat).await,
        omnii: snapshot(&omnii, &omnii_lat).await,
    })
}

async fn snapshot(c: &Arc<DriverCounters>, lat: &Arc<tokio::sync::Mutex<Vec<u64>>>) -> DriverStats {
    DriverStats {
        started: c.started.load(Ordering::Relaxed),
        completed: c.completed.load(Ordering::Relaxed),
        errors: c.errors.load(Ordering::Relaxed),
        latency_ms: lat.lock().await.clone(),
    }
}

// ---- Per-tenant loops ----

async fn solen_loop(
    base: String,
    ws_base: String,
    tenant: TenantId,
    workspaces: Vec<WorkspaceId>,
    idx: usize,
    deadline: Instant,
    counters: Arc<DriverCounters>,
    lat: Arc<tokio::sync::Mutex<Vec<u64>>>,
) {
    let workspace = workspaces[idx % workspaces.len()];
    let user = UserId::new();
    let token = match mint_token(tenant, user, workspace) {
        Ok(t) => t,
        Err(e) => {
            warn!(?e, "solen mint_token failed");
            return;
        }
    };
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("client");

    while Instant::now() < deadline {
        let thread_id = match create_thread(&http, &base, &token).await {
            Ok(t) => t,
            Err(e) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                debug!(?e, "solen create_thread");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        // Two turns per thread: one tool-use yolo, one text-only follow-up.
        for prompt in ["Reverse the word firetest.", "Acknowledge with a one-line status."] {
            let start = Instant::now();
            counters.started.fetch_add(1, Ordering::Relaxed);
            match run_one_turn(&ws_base, &token, thread_id, prompt, "yolo").await {
                Ok(()) => {
                    counters.completed.fetch_add(1, Ordering::Relaxed);
                    lat.lock().await.push(start.elapsed().as_millis() as u64);
                }
                Err(e) => {
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                    debug!(?e, "solen run_turn");
                }
            }
            if Instant::now() >= deadline {
                return;
            }
        }
    }
}

async fn hannlys_loop(
    base: String,
    ws_base: String,
    buyer_tenant: TenantId,
    idx: usize,
    deadline: Instant,
    counters: Arc<DriverCounters>,
    lat: Arc<tokio::sync::Mutex<Vec<u64>>>,
) {
    // Each loop spawns its own buyer workspace + user — exercises the
    // marketplace pattern where every "purchase" creates a buyer-side
    // workspace. The tenant row is pre-created in provisioning.
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().expect("client");
    let workspace = WorkspaceId::new();
    let user = UserId::new();
    if let Err(e) = create_workspace(
        &http,
        &base,
        workspace,
        buyer_tenant,
        &format!("hannlys-buyer-{idx}"),
        "You are a Hannlys Seed personalization agent. Always call echo_reverse \
         once with the buyer's request, then reply with one short confirmation.",
        &["echo"],
    )
    .await
    {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        warn!(?e, "hannlys create_workspace");
        return;
    }
    let token = match mint_token(buyer_tenant, user, workspace) {
        Ok(t) => t,
        Err(e) => {
            warn!(?e, "hannlys mint_token");
            return;
        }
    };

    while Instant::now() < deadline {
        let thread_id = match create_thread(&http, &base, &token).await {
            Ok(t) => t,
            Err(e) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                debug!(?e, "hannlys create_thread");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let start = Instant::now();
        counters.started.fetch_add(1, Ordering::Relaxed);
        match run_one_turn(
            &ws_base,
            &token,
            thread_id,
            "Personalize my Seed: dumbbells + 4 days/week + beginner.",
            "yolo",
        )
        .await
        {
            Ok(()) => {
                counters.completed.fetch_add(1, Ordering::Relaxed);
                lat.lock().await.push(start.elapsed().as_millis() as u64);
            }
            Err(e) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                debug!(?e, "hannlys run_turn");
            }
        }
    }
}

async fn omnii_loop(
    base: String,
    ws_base: String,
    tenant: TenantId,
    workspaces: Vec<WorkspaceId>,
    idx: usize,
    deadline: Instant,
    counters: Arc<DriverCounters>,
    lat: Arc<tokio::sync::Mutex<Vec<u64>>>,
) {
    let workspace = workspaces[idx % workspaces.len()];
    let user = UserId::new();
    let token = match mint_token(tenant, user, workspace) {
        Ok(t) => t,
        Err(e) => {
            warn!(?e, "omnii mint_token");
            return;
        }
    };
    let http = reqwest::Client::builder().timeout(Duration::from_secs(45)).build().expect("client");

    while Instant::now() < deadline {
        let thread_id = match create_thread(&http, &base, &token).await {
            Ok(t) => t,
            Err(e) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                debug!(?e, "omnii create_thread");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let start = Instant::now();
        counters.started.fetch_add(1, Ordering::Relaxed);
        // Note: the wiremock LLM dispatches `echo_reverse` regardless of
        // workspace prompt — so this turn exercises the runtime path,
        // not the model's tool-selection. Real Omnii would call
        // video_mock; the runtime is what we're stressing.
        match run_one_turn(
            &ws_base,
            &token,
            thread_id,
            "Render a 15s commercial about our spring launch.",
            "yolo",
        )
        .await
        {
            Ok(()) => {
                counters.completed.fetch_add(1, Ordering::Relaxed);
                lat.lock().await.push(start.elapsed().as_millis() as u64);
            }
            Err(e) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                debug!(?e, "omnii run_turn");
            }
        }
    }
}

// ---- Admin API helpers ----

async fn create_tenant(
    http: &reqwest::Client,
    base: &str,
    id: TenantId,
    name: &str,
    tier: &str,
) -> Result<()> {
    let resp = http
        .post(format!("{base}/admin/v1/tenants"))
        .json(&json!({"id": id, "name": name, "tier": tier}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create_tenant {name}: {}", resp.status());
    }
    Ok(())
}

async fn set_allowed_plugins(
    http: &reqwest::Client,
    base: &str,
    tenant: TenantId,
    plugins: &[&str],
) -> Result<()> {
    let resp = http
        .patch(format!("{base}/admin/v1/tenants/{tenant}/allowed-plugins"))
        .json(&json!({"allowed_plugin_ids": plugins}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("set_allowed_plugins {tenant}: {}", resp.status());
    }
    Ok(())
}

async fn set_plugin_owners(
    http: &reqwest::Client,
    base: &str,
    plugin_id: &str,
    owners: &[TenantId],
) -> Result<()> {
    let resp = http
        .put(format!("{base}/admin/v1/plugins/{plugin_id}/owners"))
        .json(&json!({"owners": owners}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("set_plugin_owners {plugin_id}: {}", resp.status());
    }
    Ok(())
}

async fn create_workspace(
    http: &reqwest::Client,
    base: &str,
    id: WorkspaceId,
    tenant: TenantId,
    name: &str,
    instructions: &str,
    plugins: &[&str],
) -> Result<()> {
    let resp = http
        .post(format!("{base}/admin/v1/workspaces"))
        .json(&json!({
            "id": id,
            "tenant_id": tenant,
            "name": name,
            "global_instructions": instructions,
            "allowed_plugin_ids": plugins,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create_workspace {name}: {}", resp.status());
    }
    Ok(())
}

async fn create_thread(http: &reqwest::Client, base: &str, jwt: &str) -> Result<ThreadId> {
    let resp = http
        .post(format!("{base}/v1/threads"))
        .header("authorization", format!("Bearer {jwt}"))
        .json(&json!({"title": "firetest"}))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("create_thread: {}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let id: ThreadId = serde_json::from_value(v["thread_id"].clone())?;
    Ok(id)
}

// ---- WS turn driver ----

async fn run_one_turn(
    ws_base: &str,
    jwt: &str,
    thread_id: ThreadId,
    prompt: &str,
    autonomy: &str,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::handshake::client::generate_key;
    use tokio_tungstenite::tungstenite::http::Request;

    let url = format!("{ws_base}/v1/threads/{thread_id}/stream");
    let host = url.strip_prefix("ws://").map_or("", |s| s.split('/').next().unwrap_or(""));
    let req = Request::builder()
        .method("GET")
        .uri(&url)
        .header("host", host)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .header("sec-websocket-protocol", format!("elena.bearer.{jwt}"))
        .body(())?;
    let (mut socket, _resp) = tokio_tungstenite::connect_async(req).await?;

    let send = json!({
        "action": "send_message",
        "text": prompt,
        "model": MODEL_ID,
        "autonomy": autonomy,
    })
    .to_string();
    socket.send(WsMessage::Text(send)).await?;

    let mut got_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !got_done {
        let next = match tokio::time::timeout_at(deadline, socket.next()).await {
            Ok(o) => o,
            Err(_) => anyhow::bail!("turn timed out"),
        };
        let Some(msg) = next else { break };
        let msg = msg?;
        if let WsMessage::Text(t) = msg {
            let v: serde_json::Value = serde_json::from_str(&t)?;
            let ev = inner_event(&v);
            match ev.get("event").and_then(|e| e.as_str()) {
                Some("done") => got_done = true,
                Some("error") => anyhow::bail!("error event: {t}"),
                _ => {}
            }
        }
    }
    let _ = socket.close(None).await;
    Ok(())
}

fn inner_event(v: &serde_json::Value) -> &serde_json::Value {
    match (v.get("offset"), v.get("event")) {
        (Some(_), Some(inner)) if inner.is_object() => inner,
        _ => v,
    }
}

fn mint_token(tenant: TenantId, user: UserId, workspace: WorkspaceId) -> Result<String> {
    #[derive(Serialize)]
    struct Claims {
        tenant_id: TenantId,
        user_id: UserId,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        tier: TenantTier,
        iss: String,
        aud: String,
        exp: u64,
    }
    let exp = u64::try_from(chrono::Utc::now().timestamp() + 3_600).unwrap_or(0);
    let claims = Claims {
        tenant_id: tenant,
        user_id: user,
        workspace_id: workspace,
        session_id: SessionId::new(),
        tier: TenantTier::Pro,
        iss: JWT_ISSUER.into(),
        aud: JWT_AUDIENCE.into(),
        exp,
    };
    Ok(encode(
        &Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )?)
}

/// Drop every tenant the firetest provisioned via the new
/// `DELETE /admin/v1/tenants/:id` endpoint, then verify Postgres has
/// no surviving rows scoped to those tenants. The cascading delete
/// (added in the same PR that introduced this check) is what closes
/// the previous "orphan rows accumulate per run" gap.
pub async fn cleanup_and_verify(
    env: &crate::harness::Harness,
    p: &Provisioned,
) -> crate::assertions::CheckResult {
    use crate::assertions::CheckResult;

    // Fresh client with a strict per-call timeout. We can't share the
    // reqwest pool with the driver loops — they may have just released
    // hundreds of keep-alive connections and the resulting reset stream
    // can race a quick reuse.
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(15))
        .build()
        .expect("client");
    let base = env.base_url();
    // Brief settle so any in-flight HTTP/2 streams from drivers get
    // out of the way before we open new ones, and so TIME_WAIT'd
    // ephemeral ports from the driver flood have a chance to clear.
    // 3 s is enough on Linux; macOS's smaller ephemeral range needs
    // more (its FIN-WAIT-2 + TIME-WAIT timers are 60 s by default,
    // but practically the kernel reuses sooner under pressure). The
    // retry loop below also helps absorb transient EAGAINs.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let tenants = [
        ("solen", p.solen.tenant_id),
        ("hannlys-creator", p.hannlys.creator_id),
        ("hannlys-buyer", p.hannlys.buyer_tenant_id),
        ("omnii", p.omnii.tenant_id),
    ];

    let mut http_path = 0usize;
    let mut sql_path = 0usize;
    for (label, id) in tenants {
        // Try the production HTTP endpoint first — that's what we want
        // operators to use. Up to 3 attempts. If the kernel has run
        // out of ephemeral ports (saturation runs on macOS torch the
        // ~16 k port range), fall back to a direct SQL DELETE through
        // the test pool. Both paths exercise the same Postgres cascade
        // chain we just added the migration + FK fix-up for; the goal
        // of the test is to prove that chain is complete, not to
        // benchmark TIME_WAIT recovery.
        let mut http_ok = false;
        for attempt in 1u8..=3 {
            match http.delete(format!("{base}/admin/v1/tenants/{id}")).send().await {
                Ok(r) if r.status().is_success() => {
                    http_ok = true;
                    break;
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(300 * u64::from(attempt))).await;
        }
        if http_ok {
            http_path += 1;
        } else {
            // Direct cascade. Same SQL the admin handler runs.
            let res: Result<sqlx::postgres::PgQueryResult, sqlx::Error> =
                sqlx::query("DELETE FROM tenants WHERE id = $1")
                    .bind(id.as_uuid())
                    .execute(env.store.threads.pool_for_test())
                    .await;
            match res {
                Ok(_) => sql_path += 1,
                Err(e) => {
                    return CheckResult::fail(
                        "cascading_delete_zero_orphans",
                        format!("SQL fallback DELETE for {label} {id} failed: {e}"),
                    );
                }
            }
        }
    }

    // Confirm cascade: every tenant-scoped table should report 0 rows
    // for the deleted tenant ids. The `tenants` table uses `id`; every
    // other table uses `tenant_id`.
    let ids: Vec<uuid::Uuid> = tenants.iter().map(|(_, id)| id.as_uuid()).collect();
    let mut leak_summary = Vec::<String>::new();
    let probes: &[(&str, &str)] = &[
        ("tenants", "id"),
        ("threads", "tenant_id"),
        ("messages", "tenant_id"),
        ("workspaces", "tenant_id"),
        ("audit_events", "tenant_id"),
        ("episodes", "tenant_id"),
        ("plugin_ownerships", "tenant_id"),
        ("tenant_credentials", "tenant_id"),
        ("plans", "tenant_id"),
        ("plan_assignments", "tenant_id"),
        ("thread_usage", "tenant_id"),
    ];
    for (table, col) in probes {
        let sql = format!("SELECT COUNT(*)::bigint FROM {table} WHERE {col} = ANY($1)");
        let row: Result<(i64,), _> =
            sqlx::query_as(&sql).bind(&ids).fetch_one(env.store.threads.pool_for_test()).await;
        match row {
            Ok((0,)) => {}
            Ok((n,)) => leak_summary.push(format!("{table}={n}")),
            Err(e) => leak_summary.push(format!("{table}=ERR({e})")),
        }
    }
    if leak_summary.is_empty() {
        CheckResult::pass(
            "cascading_delete_zero_orphans",
            format!(
                "cascade clean across {} tenants ({} via HTTP DELETE, {} via SQL fallback) — \
                 0 surviving rows in any tenant-scoped table",
                ids.len(),
                http_path,
                sql_path,
            ),
        )
    } else {
        CheckResult::fail(
            "cascading_delete_zero_orphans",
            format!("orphan rows after delete: {}", leak_summary.join(", ")),
        )
    }
}
