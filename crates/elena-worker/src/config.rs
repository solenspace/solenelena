//! [`WorkerConfig`] — operator knobs for the worker binary.

use serde::Deserialize;

/// Worker-side runtime config.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkerConfig {
    /// NATS cluster URL, e.g. `nats://127.0.0.1:4222`.
    pub nats_url: String,

    /// Stable identifier for this worker process; used as the Redis claim
    /// owner string. Should be unique per pod (hostname / pod name + PID).
    pub worker_id: String,

    /// Max concurrent agentic loops this worker will run.
    /// Default 500 (matches the doc's r-pod sizing).
    #[serde(default = "default_max_concurrent_loops")]
    pub max_concurrent_loops: usize,

    /// Optional override for the `JetStream` consumer's durable name.
    /// Leaving as `None` reuses [`DEFAULT_DURABLE_NAME`].
    #[serde(default)]
    pub durable_name: Option<String>,

    /// Optional override for the `JetStream` stream name.
    /// Leaving as `None` reuses [`elena_types::subjects::WORK_STREAM`].
    #[serde(default)]
    pub stream_name: Option<String>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            nats_url: "nats://127.0.0.1:4222".to_owned(),
            worker_id: format!("worker-{}", uuid::Uuid::new_v4()),
            max_concurrent_loops: default_max_concurrent_loops(),
            durable_name: None,
            stream_name: None,
        }
    }
}

const fn default_max_concurrent_loops() -> usize {
    500
}

/// Default durable consumer name shared across worker pods.
pub const DEFAULT_DURABLE_NAME: &str = "elena-workers";
