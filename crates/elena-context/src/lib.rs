//! Embedding-based context retrieval, packing, and long-session summarization.
//!
//! Replaces the naïve recency window in `elena-core::context_builder` with:
//!
//! - A pluggable [`Embedder`] trait (real `OnnxEmbedder`, a `NullEmbedder`
//!   for when no model is configured, and a `FakeEmbedder` for tests).
//! - A character-based [`TokenCounter`] approximation for packing context
//!   under a budget without round-tripping to the provider.
//! - `ContextManager::build_context` — embed the query, rank stored messages
//!   by cosine similarity, pack the most-relevant window under a token
//!   budget, and fall back to summarization for >200-turn threads.

// B1.6 — TenantTier + BudgetLimits::DEFAULT_FREE/PRO + default_budget_for_tier
// are #[deprecated] during the JWT-claim transition window. Remove this
// crate-level allow once the deprecated items are deleted.
#![allow(deprecated)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod context_manager;
pub mod embedder;
pub mod error;
pub mod memory;
pub mod onnx;
pub mod packer;
pub mod summarize;
pub mod summary;
pub mod tokens;

pub use context_manager::{ContextManager, ContextManagerOptions};
pub use embedder::{EMBED_DIM, Embedder, FakeEmbedder, NullEmbedder};
pub use error::{ContextError, EmbedError};
// Q14 — episodic memory inlined into elena-context. The two were
// already tightly coupled via deps.memory.recall + deps.context.embedder;
// merging saves a crate boundary and avoids a needless dep edge.
pub use memory::EpisodicMemory;
pub use onnx::OnnxEmbedder;
pub use packer::pack;
pub use summarize::{Summarizer, summary_message};
pub use summary::{MAX_SUMMARY_LEN, Summary, extract_summary};
pub use tokens::TokenCounter;

// Convenient re-exports so callers don't need a direct elena-store /
// elena-types dep just for episodic-memory types.
pub use elena_store::Episode;
pub use elena_types::Outcome;
