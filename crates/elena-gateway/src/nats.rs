//! NATS client wiring for the gateway.
//!
//! Phase 5 needs two surfaces:
//! - **`JetStream` publish** to `elena.work.incoming` — work units survive a
//!   worker crash via at-least-once redelivery. The
//!   [`bootstrap_work_stream`] helper makes the stream durable with a
//!   modest retention window.
//! - **Core pub/sub subscribe** on `elena.thread.{id}.events` — events are
//!   ephemeral (clients reconnecting replay from Postgres, not NATS).

use async_nats::jetstream::stream::{Config as StreamConfig, RetentionPolicy};
use elena_types::{WorkRequest, subjects};

use crate::error::GatewayError;

/// Connect to a NATS server and bootstrap the `JetStream` stream backing
/// [`subjects::WORK_INCOMING`] if it doesn't already exist.
pub async fn connect(
    nats_url: &str,
) -> Result<(async_nats::Client, async_nats::jetstream::Context), GatewayError> {
    let client = async_nats::connect(nats_url)
        .await
        .map_err(|e| GatewayError::Nats(format!("connect {nats_url}: {e}")))?;
    let jet = async_nats::jetstream::new(client.clone());
    bootstrap_work_stream(&jet).await?;
    Ok((client, jet))
}

/// Idempotently create the work-queue `JetStream`. Existing streams with the
/// same name are accepted as-is.
pub async fn bootstrap_work_stream(
    jet: &async_nats::jetstream::Context,
) -> Result<(), GatewayError> {
    let cfg = StreamConfig {
        name: subjects::WORK_STREAM.to_owned(),
        subjects: vec![subjects::WORK_INCOMING.to_owned()],
        retention: RetentionPolicy::WorkQueue,
        max_messages: 1_000_000,
        ..Default::default()
    };
    jet.get_or_create_stream(cfg)
        .await
        .map_err(|e| GatewayError::Nats(format!("create work stream: {e}")))?;
    Ok(())
}

/// Publish a [`WorkRequest`] to `JetStream`. Resolves once the broker has
/// acked the message; failures bubble up so the WebSocket handler can tell
/// the client the turn never started.
pub async fn publish_work(
    jet: &async_nats::jetstream::Context,
    req: &WorkRequest,
) -> Result<(), GatewayError> {
    let payload = serde_json::to_vec(req)
        .map_err(|e| GatewayError::Internal(format!("WorkRequest serde: {e}")))?;
    jet.publish(subjects::WORK_INCOMING.to_owned(), payload.into())
        .await
        .map_err(|e| GatewayError::Nats(format!("publish work: {e}")))?
        .await
        .map_err(|e| GatewayError::Nats(format!("publish ack: {e}")))?;
    Ok(())
}
