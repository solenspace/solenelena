//! Append-only audit log — one row per load-bearing action.
//!
//! Design points:
//!
//! 1. **Hot-path never blocks.** Writes go through a bounded
//!    `tokio::sync::mpsc` channel (cap `10_000`). On overflow the event is
//!    dropped and a counter increments. Losing audit rows is bad; stalling
//!    a user's turn is worse.
//! 2. **Trait-first.** Tests use [`NullAuditSink`]; production uses
//!    [`PostgresAuditSink`]. The loop consumes the trait so the core
//!    code has no Postgres imports.
//! 3. **Additive vocabulary.** The `kind` column is a free-form string so
//!    new event kinds don't need migrations. Payload schemas are
//!    documented on [`AuditEvent`] constructors.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use elena_types::{StoreError, TenantId, ThreadId, WorkspaceId};
use serde_json::Value;
use sqlx::{PgPool, QueryBuilder};
use tokio::sync::mpsc;
use tracing::warn;

use crate::sql_error::classify_sqlx;

/// Bounded audit-write queue depth. 10 000 events is ~1 MB of memory and
/// is enough to absorb a few seconds of burst under normal load.
pub const AUDIT_CHANNEL_CAP: usize = 10_000;

/// X10 — Maximum events flushed per multi-row INSERT. Postgres'
/// extended-protocol bind limit is 65535 parameters; 8 columns × 500
/// rows = 4000 binds, comfortably under. Larger batches don't gain
/// much (one round-trip dominates) and risk longer table-lock windows.
const AUDIT_BATCH_MAX: usize = 500;

/// One audit row. Construct via the helper constructors in [`Self`].
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Unique event id.
    pub id: uuid::Uuid,
    /// Always set — we never audit outside a tenant context.
    pub tenant_id: TenantId,
    /// Workspace, when the event is scoped to one.
    pub workspace_id: Option<WorkspaceId>,
    /// Thread, when the event is scoped to one.
    pub thread_id: Option<ThreadId>,
    /// `"system"` for server-originated, `"user"` for user-triggered
    /// (approvals). Clients sometimes want to filter per actor.
    pub actor: String,
    /// Short kebab-case kind — e.g. `"tool_use"`, `"tool_result"`,
    /// `"approval_decision"`, `"rate_limit_rejection"`.
    pub kind: String,
    /// Structured payload, kind-dependent.
    pub payload: Value,
    /// Event timestamp.
    pub created_at: DateTime<Utc>,
}

impl AuditEvent {
    /// Build a fresh event with a random id and current UTC timestamp.
    #[must_use]
    pub fn new(tenant_id: TenantId, kind: impl Into<String>, payload: Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            tenant_id,
            workspace_id: None,
            thread_id: None,
            actor: "system".to_owned(),
            kind: kind.into(),
            payload,
            created_at: Utc::now(),
        }
    }

    /// Attach a thread scope.
    #[must_use]
    pub fn with_thread(mut self, thread_id: ThreadId) -> Self {
        self.thread_id = Some(thread_id);
        self
    }

    /// Attach a workspace scope.
    #[must_use]
    pub fn with_workspace(mut self, workspace_id: WorkspaceId) -> Self {
        self.workspace_id = Some(workspace_id);
        self
    }

    /// Mark the actor — defaults to `"system"`.
    #[must_use]
    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = actor.into();
        self
    }
}

/// Interface the loop writes through. Callers treat `emit` as
/// fire-and-forget.
#[async_trait]
pub trait AuditSink: Send + Sync + std::fmt::Debug {
    /// Record an event. Must be non-blocking; must not return an error
    /// that the caller has to handle — drops are logged internally.
    async fn emit(&self, event: AuditEvent);
}

/// Discard-everything sink. Used in tests and for deployments that opt
/// out of the audit table entirely.
#[derive(Debug, Default, Clone)]
pub struct NullAuditSink;

#[async_trait]
impl AuditSink for NullAuditSink {
    async fn emit(&self, _event: AuditEvent) {}
}

/// Callback the sink invokes for every dropped event. Boot-path code
/// in the gateway/worker passes `Arc::new(|n| metrics::AUDIT_DROPS.inc_by(n))`
/// so a Prometheus alert can fire on the very first miss. Tests pass
/// the no-op default. Keeping this as a callback rather than a direct
/// dependency on `prometheus` keeps `elena-store` from pulling in the
/// observability crate (which would create a circular workspace edge).
pub type DropCallback = Arc<dyn Fn(u64) + Send + Sync>;

/// Postgres-backed sink. Writes go through a bounded mpsc to a
/// background flusher task that issues INSERTs.
#[derive(Clone)]
pub struct PostgresAuditSink {
    tx: mpsc::Sender<AuditEvent>,
    drops: Arc<AtomicU64>,
    on_drop: DropCallback,
}

impl std::fmt::Debug for PostgresAuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresAuditSink")
            .field("drops_observed", &self.drops.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl PostgresAuditSink {
    /// Spawn a flusher task with no drop telemetry. Equivalent to
    /// [`Self::spawn_with_callback`] with a no-op callback. Used by
    /// tests and by minimal deployments that don't run Prometheus.
    #[must_use]
    pub fn spawn(pool: PgPool) -> Self {
        Self::spawn_with_callback(pool, Arc::new(|_| {}))
    }

    /// Spawn a flusher task that drains audit events from the channel
    /// and writes them to `audit_events`. Each dropped event invokes
    /// `on_drop(1)` synchronously so a Prometheus counter (or any
    /// caller-supplied counter) can pick it up.
    ///
    /// X10 — the flusher batches up to [`AUDIT_BATCH_MAX`] events per
    /// `INSERT`. Pre-X10 the loop issued one `INSERT` per event,
    /// which capped throughput at the per-row round-trip latency. The
    /// batch issues a single multi-row VALUES insert so a 50-call turn
    /// becomes one DB round-trip instead of fifty.
    #[must_use]
    pub fn spawn_with_callback(pool: PgPool, on_drop: DropCallback) -> Self {
        let (tx, mut rx) = mpsc::channel::<AuditEvent>(AUDIT_CHANNEL_CAP);
        let drops = Arc::new(AtomicU64::new(0));
        let drops_for_task = Arc::clone(&drops);
        let on_drop_for_task = Arc::clone(&on_drop);

        tokio::spawn(async move {
            let mut batch: Vec<AuditEvent> = Vec::with_capacity(AUDIT_BATCH_MAX);
            while let Some(first) = rx.recv().await {
                batch.clear();
                batch.push(first);
                // Greedy drain: take whatever's already buffered. Bounded
                // by AUDIT_BATCH_MAX so a flooded channel doesn't starve
                // the DB write with one giant INSERT.
                while batch.len() < AUDIT_BATCH_MAX {
                    match rx.try_recv() {
                        Ok(ev) => batch.push(ev),
                        Err(_) => break,
                    }
                }
                if let Err(e) = write_batch(&pool, &batch).await {
                    let n = u64::try_from(batch.len()).unwrap_or(0);
                    drops_for_task.fetch_add(n, Ordering::Relaxed);
                    on_drop_for_task(n);
                    warn!(?e, batch_size = batch.len(), "audit batch write failed");
                }
            }
        });

        Self { tx, drops, on_drop }
    }

    /// Total number of events dropped — either from channel overflow or
    /// DB write failure. Exposed so the metrics crate can surface it.
    #[must_use]
    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl AuditSink for PostgresAuditSink {
    async fn emit(&self, event: AuditEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(dropped)) => {
                self.drops.fetch_add(1, Ordering::Relaxed);
                (self.on_drop)(1);
                warn!(kind = %dropped.kind, "audit channel full; dropping event");
            }
            Err(mpsc::error::TrySendError::Closed(dropped)) => {
                self.drops.fetch_add(1, Ordering::Relaxed);
                (self.on_drop)(1);
                warn!(kind = %dropped.kind, "audit channel closed; dropping event");
            }
        }
    }
}

/// X10 — Multi-row INSERT for one batch drained off the channel.
/// `QueryBuilder::push_values` handles parameter binding cleanly; one
/// round-trip flushes the whole batch.
async fn write_batch(pool: &PgPool, events: &[AuditEvent]) -> Result<(), StoreError> {
    if events.is_empty() {
        return Ok(());
    }
    let mut qb = QueryBuilder::<sqlx::Postgres>::new(
        "INSERT INTO audit_events
            (id, tenant_id, workspace_id, thread_id, actor, kind, payload, created_at) ",
    );
    qb.push_values(events, |mut b, ev| {
        b.push_bind(ev.id)
            .push_bind(ev.tenant_id.as_uuid())
            .push_bind(ev.workspace_id.map(|w| w.as_uuid()))
            .push_bind(ev.thread_id.map(|t| t.as_uuid()))
            .push_bind(&ev.actor)
            .push_bind(&ev.kind)
            .push_bind(&ev.payload)
            .push_bind(ev.created_at);
    });
    qb.build().execute(pool).await.map_err(classify_sqlx)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use elena_types::{TenantId, ThreadId};

    use super::*;

    #[tokio::test]
    async fn null_sink_absorbs_all_events() {
        let sink = NullAuditSink;
        let ev = AuditEvent::new(TenantId::new(), "test", serde_json::json!({"ok": true}));
        sink.emit(ev).await; // must not panic or block
    }

    #[test]
    fn event_builders_scope_correctly() {
        let tenant_id = TenantId::new();
        let thread_id = ThreadId::new();
        let ev = AuditEvent::new(tenant_id, "tool_use", serde_json::json!({}))
            .with_thread(thread_id)
            .with_actor("worker");
        assert_eq!(ev.tenant_id, tenant_id);
        assert_eq!(ev.thread_id, Some(thread_id));
        assert_eq!(ev.actor, "worker");
        assert_eq!(ev.kind, "tool_use");
    }

    #[test]
    fn audit_channel_cap_is_power_of_ten() {
        assert_eq!(AUDIT_CHANNEL_CAP, 10_000);
    }

    /// Sanity check that the drop callback is plumbed through the
    /// closed-channel branch. We can't easily test the bounded-mpsc
    /// overflow path without a live tokio runtime + a flusher that
    /// blocks; the closed-channel path exercises the same callback and
    /// is the analogous failure mode for a degraded process.
    #[tokio::test]
    async fn drop_callback_fires_when_channel_is_closed() {
        let observed = Arc::new(AtomicU64::new(0));
        let observed_for_cb = Arc::clone(&observed);
        let on_drop: DropCallback = Arc::new(move |n| {
            observed_for_cb.fetch_add(n, Ordering::Relaxed);
        });
        let (tx, rx) = mpsc::channel::<AuditEvent>(1);
        // Drop the receiver to close the channel.
        drop(rx);
        let sink = PostgresAuditSink {
            tx,
            drops: Arc::new(AtomicU64::new(0)),
            on_drop: Arc::clone(&on_drop),
        };
        sink.emit(AuditEvent::new(TenantId::new(), "test", serde_json::json!({}))).await;
        assert_eq!(observed.load(Ordering::Relaxed), 1);
        assert_eq!(sink.drops(), 1);
    }
}
