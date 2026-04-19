//! Errors raised by embedding + retrieval + summarization.

use elena_types::{ElenaError, LlmApiError, StoreError};
use thiserror::Error;

/// Embedding-time failures — model I/O, tokenizer errors, shape mismatches.
#[derive(Debug, Clone, Error, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum EmbedError {
    /// Failed to load the ONNX model or tokenizer from disk.
    #[error("failed to load embedding model: {message}")]
    Load {
        /// Underlying cause.
        message: String,
    },
    /// The tokenizer or model rejected the input.
    #[error("embedding input rejected: {message}")]
    Input {
        /// Underlying cause.
        message: String,
    },
    /// Inference ran but produced an unexpected output shape.
    #[error("embedding output shape mismatch: {message}")]
    Shape {
        /// Underlying cause.
        message: String,
    },
}

/// Context-building failures — a mix of store / LLM / embedder errors that
/// may surface while `build_context` runs.
#[derive(Debug, Clone, Error, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ContextError {
    /// Persistence failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// LLM summarizer failure.
    #[error(transparent)]
    LlmApi(#[from] LlmApiError),
    /// Embedder failure.
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

impl From<ContextError> for ElenaError {
    fn from(err: ContextError) -> Self {
        match err {
            ContextError::Store(e) => Self::Store(e),
            ContextError::LlmApi(e) => Self::LlmApi(e),
            ContextError::Embed(e) => Self::LlmApi(LlmApiError::Unknown { message: e.to_string() }),
        }
    }
}
