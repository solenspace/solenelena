//! Drains a [`ReceiverStream<StreamEvent>`] from `elena-core` and publishes
//! every event to the per-thread NATS subject. Spawned per work-request.

use elena_store::SessionCache;
use elena_types::{StreamEnvelope, StreamEvent, ThreadId, subjects};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

/// Publish every event from `stream` to `elena.thread.{thread_id}.events`.
///
/// X4 — Each event is wrapped in a [`StreamEnvelope`] carrying a
/// per-thread monotonic offset (Redis INCR). Clients track the
/// last-seen offset and reconnect with `?since=<offset>` for replay.
/// On Redis failure the publish falls open with offset = 0; downstream
/// gap-detection treats the missing increments as a benign blip.
///
/// Returns once the stream is exhausted (i.e. the loop emitted its final
/// `Done` event and dropped the channel sender).
pub async fn pump_events(
    nats: &async_nats::Client,
    cache: &SessionCache,
    thread_id: ThreadId,
    mut stream: ReceiverStream<StreamEvent>,
) {
    let subject = subjects::thread_events(thread_id);
    while let Some(event) = stream.next().await {
        let offset = match cache.next_event_offset(thread_id).await {
            Ok(n) => n,
            Err(e) => {
                warn!(?e, %thread_id, "next_event_offset failed; using offset=0");
                0
            }
        };
        let envelope = StreamEnvelope::new(offset, event);
        let payload = match serde_json::to_vec(&envelope) {
            Ok(p) => p,
            Err(e) => {
                warn!(?e, "failed to serialize StreamEnvelope");
                continue;
            }
        };
        if let Err(e) = nats.publish(subject.clone(), payload.into()).await {
            warn!(?e, %thread_id, "nats publish failed; dropping event");
            // Don't break — the loop may keep producing useful events even
            // if one publish fails (e.g., transient network blip).
        }
    }
    // Best-effort flush before returning so the gateway sees the tail
    // events before we ack the work message.
    let _ = nats.flush().await;
}
