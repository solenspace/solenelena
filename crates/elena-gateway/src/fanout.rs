//! X1 — Gateway-local NATS fanout.
//!
//! Pre-X1, every connected WebSocket subscribed to NATS individually for
//! its thread's events. At 500K concurrent sessions that's 500K NATS
//! interest records fanned across every NATS node — gateway pod count
//! doesn't bound it, client count does.
//!
//! Post-X1, each gateway pod holds **one** wildcard subscription on
//! `elena.thread.*.events`. Incoming events are routed by `ThreadId`
//! (parsed out of the subject) into a per-thread
//! [`tokio::sync::broadcast`] channel. WS handlers subscribe to the
//! local channel, so NATS interest scales with pod count rather than
//! client count.
//!
//! Failure mode: a gateway pod restart drops every subscriber and the
//! wildcard subscription. Reconnecting clients must refetch persisted
//! state from Postgres — same posture as pre-X1 (ephemeral stream,
//! D-stream-ephemeral in the plan). The X4 follow-up adds an offset +
//! Postgres-replay path that lets clients catch up cleanly.

use std::sync::Arc;

use bytes::Bytes;
use dashmap::DashMap;
use elena_types::ThreadId;
use futures::StreamExt;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Per-thread broadcast channel capacity. Matches the pre-X1
/// `EVENT_CHANNEL_CAP` in `elena-core::loop_driver`. Buffer overflow on
/// a slow subscriber surfaces as `RecvError::Lagged` and the WS
/// handler treats it the same as the pre-X1 backpressure path:
/// disconnect the client.
const PER_THREAD_BROADCAST_CAP: usize = 128;

/// Wildcard NATS subject this pod subscribes to once at startup.
const ALL_THREAD_EVENTS_SUBJECT: &str = "elena.thread.*.events";

/// In-memory map from `ThreadId` to a broadcast `Sender`. Cheap to
/// clone — `DashMap` is the lock-sharded primitive and the value is a
/// reference-counted `Sender` from `tokio::sync::broadcast`.
pub type FanoutTable = Arc<DashMap<ThreadId, broadcast::Sender<Bytes>>>;

/// Build an empty `FanoutTable`. Stored on `GatewayState`; the wildcard
/// subscriber task is started once via [`spawn_fanout_pump`].
#[must_use]
pub fn new_fanout_table() -> FanoutTable {
    Arc::new(DashMap::new())
}

/// Get-or-create a broadcast `Receiver` for `thread_id`. Inserts a new
/// `Sender` into the table when none exists. The first WS subscriber
/// for a thread allocates the channel; later subscribers just take a
/// fresh `Receiver` off the same `Sender`.
#[must_use]
pub fn subscribe(table: &FanoutTable, thread_id: ThreadId) -> broadcast::Receiver<Bytes> {
    table
        .entry(thread_id)
        .or_insert_with(|| {
            let (tx, _rx) = broadcast::channel(PER_THREAD_BROADCAST_CAP);
            tx
        })
        .subscribe()
}

/// Drop the per-thread channel when the last subscriber goes away.
/// Called by WS handlers in their cleanup path. Best-effort: a stale
/// `Sender` left in the map only costs an empty `broadcast::Sender`'s
/// memory until the next entry-check.
pub fn maybe_release(table: &FanoutTable, thread_id: ThreadId) {
    if let Some(entry) = table.get(&thread_id) {
        // `receiver_count` returns active receivers. When zero, no WS
        // is listening; safe to drop the sender so the next subscribe
        // starts with a fresh channel.
        if entry.receiver_count() == 0 {
            drop(entry);
            table.remove(&thread_id);
        }
    }
}

/// Spawn the wildcard NATS subscriber that fans every per-thread event
/// into the local broadcast table. Returns immediately; the task runs
/// for the lifetime of the process.
///
/// Subjects look like `elena.thread.<ULID>.events`; we parse the third
/// segment as a `ThreadId`. Malformed subjects are dropped with a WARN
/// — this only fires if an unrelated publisher is using the same
/// subject space, which would be a misconfiguration to flag.
pub fn spawn_fanout_pump(nats: async_nats::Client, table: FanoutTable) {
    tokio::spawn(async move {
        let mut sub = match nats.subscribe(ALL_THREAD_EVENTS_SUBJECT).await {
            Ok(s) => s,
            Err(e) => {
                warn!(?e, "fanout: wildcard subscribe failed; gateway will not fan events");
                return;
            }
        };
        debug!("fanout: subscribed to {ALL_THREAD_EVENTS_SUBJECT}");
        while let Some(msg) = sub.next().await {
            let Some(thread_id) = parse_thread_id_from_subject(&msg.subject) else {
                debug!(subject = %msg.subject, "fanout: subject didn't match expected pattern");
                continue;
            };
            // Only allocate / route when at least one WS handler
            // currently subscribes to this thread. No subscriber → drop
            // — the worker's NATS publish already succeeded; there's
            // nothing useful to do with an event no one's reading.
            let Some(sender) = table.get(&thread_id) else { continue };
            // `send` errors only when zero active receivers exist — a
            // race with the last subscriber dropping. Treat as a
            // benign "no one listening" case.
            let _ = sender.send(Bytes::copy_from_slice(&msg.payload));
        }
        warn!("fanout: wildcard subscription ended; gateway will no longer fan events");
    });
}

/// Parse `elena.thread.<id>.events` → `ThreadId`. Returns `None` for
/// any other shape (defensive — only the gateway's own publishers
/// should land on this wildcard, but we don't trust that).
fn parse_thread_id_from_subject(subject: &str) -> Option<ThreadId> {
    let mut parts = subject.split('.');
    if parts.next()? != "elena" {
        return None;
    }
    if parts.next()? != "thread" {
        return None;
    }
    let id_part = parts.next()?;
    if parts.next()? != "events" {
        return None;
    }
    if parts.next().is_some() {
        return None;
    }
    id_part.parse::<ThreadId>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_subject() {
        let id = ThreadId::new();
        let subject = elena_types::subjects::thread_events(id);
        assert_eq!(parse_thread_id_from_subject(&subject), Some(id));
    }

    #[test]
    fn rejects_unrelated_subject() {
        assert!(parse_thread_id_from_subject("elena.work.incoming").is_none());
        assert!(parse_thread_id_from_subject("elena.thread.abc.events").is_none());
        assert!(parse_thread_id_from_subject("elena.thread..events").is_none());
        let id = ThreadId::new();
        let suffixed = format!("elena.thread.{id}.events.extra");
        assert!(parse_thread_id_from_subject(&suffixed).is_none());
    }

    #[tokio::test]
    async fn subscribe_get_or_creates() {
        let table = new_fanout_table();
        let id = ThreadId::new();
        let mut rx1 = subscribe(&table, id);
        let mut rx2 = subscribe(&table, id);
        // Same channel — both receivers see the broadcast.
        let sender = table.get(&id).unwrap().clone();
        sender.send(Bytes::from_static(b"hello")).unwrap();
        assert_eq!(&*rx1.recv().await.unwrap(), b"hello");
        assert_eq!(&*rx2.recv().await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn maybe_release_drops_when_no_receivers() {
        let table = new_fanout_table();
        let id = ThreadId::new();
        {
            let _rx = subscribe(&table, id);
            assert!(table.contains_key(&id));
        }
        maybe_release(&table, id);
        assert!(!table.contains_key(&id));
    }
}
