//! Context-building configuration.
//!
//! Controls the embedding model location, retrieval parameters, and when to
//! fall back to on-demand LLM summarization for very long threads.

use std::path::PathBuf;

use serde::Deserialize;

/// Context-management settings.
///
/// Both model paths are optional: when either is `None`, [`elena-context`]
/// installs a null embedder and `build_context` degrades to a plain recency
/// window of `recency_window` messages.
#[derive(Debug, Clone, Deserialize)]
pub struct ContextConfig {
    /// Filesystem path to the ONNX embedding model (e.g., `e5-small-v2.onnx`).
    #[serde(default)]
    pub embedding_model_path: Option<PathBuf>,

    /// Filesystem path to the matching `tokenizer.json` file.
    #[serde(default)]
    pub tokenizer_path: Option<PathBuf>,

    /// Max messages to include when retrieval is disabled (`NullEmbedder`).
    #[serde(default = "default_recency_window")]
    pub recency_window: u32,

    /// Top-k neighbors returned by vector search per `build_context` call.
    #[serde(default = "default_retrieval_top_k")]
    pub retrieval_top_k: u32,

    /// Threshold above which `build_context` may invoke LLM summarization
    /// to compress old turns. Sessions shorter than this never summarize.
    #[serde(default = "default_summarize_turn_threshold")]
    pub summarize_turn_threshold: u32,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            embedding_model_path: None,
            tokenizer_path: None,
            recency_window: default_recency_window(),
            retrieval_top_k: default_retrieval_top_k(),
            summarize_turn_threshold: default_summarize_turn_threshold(),
        }
    }
}

const fn default_recency_window() -> u32 {
    100
}

const fn default_retrieval_top_k() -> u32 {
    20
}

const fn default_summarize_turn_threshold() -> u32 {
    200
}
