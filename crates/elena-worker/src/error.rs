//! Errors raised by the worker.

use thiserror::Error;

/// Worker-level failures.
#[derive(Debug, Error)]
pub enum WorkerError {
    /// NATS connection / stream / consumer failure.
    #[error("nats error: {0}")]
    Nats(String),

    /// Persistence-layer failure that the worker can't paper over.
    #[error(transparent)]
    Store(#[from] elena_types::StoreError),

    /// JSON deserialization of a `WorkRequest` failed.
    #[error("malformed work request: {0}")]
    BadWork(String),

    /// Catch-all for unexpected internal failures.
    #[error("internal: {0}")]
    Internal(String),
}
