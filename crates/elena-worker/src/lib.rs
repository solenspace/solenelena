//! NATS `JetStream` consumer running Elena's agentic loop.
//!
//! Phase 5. Subscribes to `elena.work.incoming` under the `workers` queue
//! group and runs every received [`WorkRequest`](elena_types::WorkRequest)
//! through `elena-core::run_loop`. Every emitted
//! [`StreamEvent`](elena_types::StreamEvent) is published to
//! `elena.thread.{id}.events` so any subscribed gateway WebSocket can
//! relay it to the client. Aborts arrive on `elena.thread.{id}.abort`.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod config;
pub mod consumer;
pub mod dispatch;
pub mod error;
pub mod publisher;

use std::sync::Arc;

use elena_core::LoopDeps;
use tokio_util::sync::CancellationToken;

pub use config::{DEFAULT_DURABLE_NAME, WorkerConfig};
pub use error::WorkerError;

/// Run the worker until `cancel` fires or the NATS connection ends.
///
/// Bind once per worker process; safe to call from a `tokio::main` body.
pub async fn run_worker(
    cfg: WorkerConfig,
    deps: Arc<LoopDeps>,
    cancel: CancellationToken,
) -> Result<(), WorkerError> {
    consumer::run_consumer(cfg, deps, cancel).await
}
