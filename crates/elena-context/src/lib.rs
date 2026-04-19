//! Embedding-based context retrieval, packing, and long-session summarization.
//!
//! Phase 4 of Elena. Replaces the naïve recency window used in Phase 3 with:
//!
//! - A pluggable [`Embedder`] trait (real `OnnxEmbedder`, a `NullEmbedder`
//!   for when no model is configured, and a `FakeEmbedder` for tests).
//! - A character-based [`TokenCounter`] approximation for packing context
//!   under a budget without round-tripping to the provider.
//! - `ContextManager::build_context` — embed the query, rank stored messages
//!   by cosine similarity, pack the most-relevant window under a token
//!   budget, and fall back to summarization for >200-turn threads.

#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod context_manager;
pub mod embedder;
pub mod error;
pub mod onnx;
pub mod packer;
pub mod summarize;
pub mod tokens;

pub use context_manager::{ContextManager, ContextManagerOptions};
pub use embedder::{EMBED_DIM, Embedder, FakeEmbedder, NullEmbedder};
pub use error::{ContextError, EmbedError};
pub use onnx::OnnxEmbedder;
pub use packer::pack;
pub use summarize::{Summarizer, summary_message};
pub use tokens::TokenCounter;
