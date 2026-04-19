//! Per-workspace episodic memory.
//!
//! Phase 4. `record_episode` distills a completed thread into a compact
//! task summary + action list, embeds the summary, and stores it via
//! [`elena_store::EpisodeStore`]. `recall` returns the nearest-neighbor
//! episodes within a workspace so the loop can inject relevant prior
//! context into a new thread's system prompt.
//!
//! The [`Episode`](elena_store::Episode) type itself lives in `elena-store`
//! so the SQL roundtrip stays in one place.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod memory;
pub mod summary;

pub use memory::EpisodicMemory;
pub use summary::{MAX_SUMMARY_LEN, Summary, extract_summary};

// Convenient re-exports so callers don't need a direct `elena-store` dep.
pub use elena_store::Episode;
pub use elena_types::Outcome;
