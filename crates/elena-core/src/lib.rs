//! Agentic loop state machine for Elena.
//!
//! Phase 3 surface: [`LoopState`] + [`LoopPhase`] + [`LoopDeps`] and the
//! driver entry points that advance one loop run from `Received` to
//! `Completed` or `Failed`. Every phase transition commits durable state
//! (messages to Postgres, checkpoint to Redis) before yielding, so any
//! worker can resume a thread after a crash.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod checkpoint;
pub mod context_builder;
pub mod deps;
pub mod dispatch_decision;
pub mod loop_driver;
pub mod request_builder;
pub mod state;
pub mod step;
pub mod stream_consumer;

pub use checkpoint::{drop_loop_state, load_loop_state, save_loop_state};
pub use context_builder::fetch_recent_messages;
pub use deps::LoopDeps;
pub use dispatch_decision::{is_decision_fork, requires_approval};
pub use loop_driver::run_loop;
pub use request_builder::build_llm_request;
pub use state::{LoopPhase, LoopState, RecoveryState};
pub use step::{StepOutcome, step};
pub use stream_consumer::{ConsumedStream, consume_stream};
