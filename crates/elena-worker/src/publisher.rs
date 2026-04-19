//! Drains a [`ReceiverStream<StreamEvent>`] from `elena-core` and publishes
//! every event to the per-thread NATS subject. Spawned per work-request.

use elena_types::{StreamEvent, ThreadId, subjects};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

/// Publish every event from `stream` to `elena.thread.{thread_id}.events`.
///
/// Returns once the stream is exhausted (i.e. the loop emitted its final
/// `Done` event and dropped the channel sender).
pub async fn pump_events(
    nats: &async_nats::Client,
    thread_id: ThreadId,
    mut stream: ReceiverStream<StreamEvent>,
) {
    let subject = subjects::thread_events(thread_id);
    while let Some(event) = stream.next().await {
        let payload = match serde_json::to_vec(&event) {
            Ok(p) => p,
            Err(e) => {
                warn!(?e, "failed to serialize StreamEvent");
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
