//! `JetStream` consumer + dispatch loop.

use std::sync::Arc;

use async_nats::jetstream::consumer::{DeliverPolicy, pull::Config as PullConfig};
use elena_core::LoopDeps;
use elena_types::{WorkRequest, subjects};
use futures::StreamExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::{DEFAULT_DURABLE_NAME, WorkerConfig};
use crate::dispatch::handle;
use crate::error::WorkerError;

/// Subscribe to `elena.work.incoming` (queue group `workers`) and dispatch
/// every received `WorkRequest` under a semaphore.
pub async fn run_consumer(
    cfg: WorkerConfig,
    deps: Arc<LoopDeps>,
    cancel: CancellationToken,
) -> Result<(), WorkerError> {
    let client = async_nats::connect(&cfg.nats_url)
        .await
        .map_err(|e| WorkerError::Nats(format!("connect {}: {e}", cfg.nats_url)))?;
    let jet = async_nats::jetstream::new(client.clone());

    // Make sure the stream exists (the gateway also creates it; whoever
    // boots first wins).
    let stream_name = cfg.stream_name.clone().unwrap_or_else(|| subjects::WORK_STREAM.to_owned());
    let stream = jet
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: stream_name.clone(),
            subjects: vec![subjects::WORK_INCOMING.to_owned()],
            retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
            max_messages: 1_000_000,
            ..Default::default()
        })
        .await
        .map_err(|e| WorkerError::Nats(format!("ensure stream {stream_name}: {e}")))?;

    let durable = cfg.durable_name.clone().unwrap_or_else(|| DEFAULT_DURABLE_NAME.to_owned());
    // Phase-7 NATS tuning: `max_ack_pending` gives the consumer twice the
    // worker's concurrency budget so the pull pipeline stays full even
    // when one loop is momentarily stuck. `ack_wait` = 120 s is generous
    // enough for long tool calls + provider slowdown.
    let max_ack_pending =
        i64::try_from(cfg.max_concurrent_loops.saturating_mul(2)).unwrap_or(i64::MAX);
    let consumer = stream
        .get_or_create_consumer(
            &durable,
            PullConfig {
                durable_name: Some(durable.clone()),
                deliver_policy: DeliverPolicy::All,
                max_ack_pending,
                ack_wait: std::time::Duration::from_secs(120),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| WorkerError::Nats(format!("ensure consumer {durable}: {e}")))?;

    let semaphore = Arc::new(Semaphore::new(cfg.max_concurrent_loops.max(1)));
    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| WorkerError::Nats(format!("consumer.messages(): {e}")))?;

    info!(
        nats_url = %cfg.nats_url,
        worker_id = %cfg.worker_id,
        cap = cfg.max_concurrent_loops,
        "worker consumer started"
    );

    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!("worker shutting down");
                break;
            }
            next = messages.next() => {
                let Some(item) = next else { break; };
                let msg = match item {
                    Ok(m) => m,
                    Err(e) => {
                        error!(?e, "JetStream pull error; backing off");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                };

                let req: WorkRequest = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(?e, "unparseable WorkRequest; acking + dropping");
                        let _ = msg.ack().await;
                        continue;
                    }
                };

                let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else { break };
                let nats = client.clone();
                let deps = Arc::clone(&deps);
                let worker_id = cfg.worker_id.clone();
                let parent_cancel = cancel.clone();

                tokio::spawn(async move {
                    let _permit = permit;
                    handle(nats, deps, worker_id, req, parent_cancel).await;
                    if let Err(e) = msg.ack().await {
                        warn!(?e, "JetStream ack failed");
                    }
                });
            }
        }
    }

    Ok(())
}
