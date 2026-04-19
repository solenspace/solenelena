//! [`OnnxEmbedder`] — sentence embeddings via `ort` + `tokenizers`.
//!
//! Loads an `e5-small-v2`-compatible BERT-style model (or any model that
//! takes `input_ids` + `attention_mask` and returns `last_hidden_state` of
//! shape `[batch, seq, 384]`). Applies mask-weighted mean pooling + L2
//! normalization on the output so cosine similarity in Postgres matches
//! the model's intended similarity metric.
//!
//! This type requires the ONNX Runtime shared library on the host (via the
//! `ort` crate's `load-dynamic` feature). Tests that exercise the full
//! pipeline are behind `#[ignore]` because they need a model file; unit
//! tests here cover the load-failure paths only.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ndarray::{Array, Array2};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::Value;
use tokenizers::Tokenizer;

use crate::embedder::{EMBED_DIM, Embedder};
use crate::error::EmbedError;

/// ONNX-backed embedder. Thread-safe (cheap clone, shared session inside an
/// [`Arc`]). `ort`'s `Session::run` takes `&mut self`, so we serialize
/// access through a [`Mutex`] — `spawn_blocking` handles the CPU cost.
#[derive(Clone)]
pub struct OnnxEmbedder {
    inner: Arc<Inner>,
}

struct Inner {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    max_seq_len: usize,
    dim: usize,
    model_path: PathBuf,
    tokenizer_path: PathBuf,
}

impl OnnxEmbedder {
    /// Load a model + tokenizer from disk.
    ///
    /// Defaults: `max_seq_len = 512`, `dim = 384` (e5-small-v2). Use
    /// [`Self::with_max_seq_len`] / [`Self::with_dim`] to override for other
    /// models.
    pub fn load(
        model_path: impl AsRef<Path>,
        tokenizer_path: impl AsRef<Path>,
    ) -> Result<Self, EmbedError> {
        let model_path = model_path.as_ref().to_path_buf();
        let tokenizer_path = tokenizer_path.as_ref().to_path_buf();

        let session = Session::builder()
            .map_err(|e| EmbedError::Load { message: e.to_string() })?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| EmbedError::Load { message: e.to_string() })?
            .with_intra_threads(1)
            .map_err(|e| EmbedError::Load { message: e.to_string() })?
            .commit_from_file(&model_path)
            .map_err(|e| EmbedError::Load { message: e.to_string() })?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| EmbedError::Load { message: e.to_string() })?;

        Ok(Self {
            inner: Arc::new(Inner {
                session: Mutex::new(session),
                tokenizer,
                max_seq_len: 512,
                dim: EMBED_DIM,
                model_path,
                tokenizer_path,
            }),
        })
    }

    /// Override the sequence cap used when tokenizing.
    #[must_use]
    pub fn with_max_seq_len(mut self, max_seq_len: usize) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.max_seq_len = max_seq_len;
        }
        self
    }

    /// Override the advertised output dimension.
    #[must_use]
    pub fn with_dim(mut self, dim: usize) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.dim = dim;
        }
        self
    }

    fn run_embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let encoding = self
            .inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| EmbedError::Input { message: e.to_string() })?;

        let ids_u32 = encoding.get_ids();
        let mask_u32 = encoding.get_attention_mask();
        let len = ids_u32.len().min(self.inner.max_seq_len);
        let ids: Vec<i64> = ids_u32.iter().take(len).map(|&x| i64::from(x)).collect();
        let mask: Vec<i64> = mask_u32.iter().take(len).map(|&x| i64::from(x)).collect();

        let ids_arr: Array2<i64> = Array::from_shape_vec((1, len), ids)
            .map_err(|e| EmbedError::Shape { message: e.to_string() })?;
        let mask_arr: Array2<i64> = Array::from_shape_vec((1, len), mask.clone())
            .map_err(|e| EmbedError::Shape { message: e.to_string() })?;

        let ids_value =
            Value::from_array(ids_arr).map_err(|e| EmbedError::Input { message: e.to_string() })?;
        let mask_value = Value::from_array(mask_arr)
            .map_err(|e| EmbedError::Input { message: e.to_string() })?;

        let mut session = self
            .inner
            .session
            .lock()
            .map_err(|e| EmbedError::Input { message: format!("session lock: {e}") })?;
        let outputs = session
            .run(ort::inputs!["input_ids" => ids_value, "attention_mask" => mask_value])
            .map_err(|e| EmbedError::Input { message: e.to_string() })?;

        // Copy the tensor out before dropping `outputs` — the view borrows
        // from the value, so we own the shape + data after this block.
        let mut copied: Option<(Vec<i64>, Vec<f32>)> = None;
        for (_name, value) in &outputs {
            if let Ok((shape_ref, data_ref)) = value.try_extract_tensor::<f32>() {
                copied = Some((shape_ref.to_vec(), data_ref.to_vec()));
                break;
            }
        }
        drop(outputs);
        drop(session);
        let (shape_owned, data_owned) = copied.ok_or_else(|| EmbedError::Shape {
            message: "no f32 tensor in model output".to_owned(),
        })?;

        if shape_owned.len() != 3 || shape_owned[0] != 1 {
            return Err(EmbedError::Shape {
                message: format!("expected [1, seq, dim], got {shape_owned:?}"),
            });
        }
        let seq = usize::try_from(shape_owned[1]).unwrap_or(0);
        let dim = usize::try_from(shape_owned[2]).unwrap_or(0);
        if dim != self.inner.dim {
            return Err(EmbedError::Shape {
                message: format!("expected dim={}, got {}", self.inner.dim, dim),
            });
        }
        if seq != len {
            return Err(EmbedError::Shape {
                message: format!("seq mismatch (tokens={len}, output={seq})"),
            });
        }

        Ok(mean_pool(&data_owned, &mask, seq, dim))
    }

    /// The model path this embedder was loaded from (for diagnostics).
    #[must_use]
    pub fn model_path(&self) -> &Path {
        &self.inner.model_path
    }

    /// The tokenizer path this embedder was loaded from (for diagnostics).
    #[must_use]
    pub fn tokenizer_path(&self) -> &Path {
        &self.inner.tokenizer_path
    }
}

impl std::fmt::Debug for OnnxEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnnxEmbedder")
            .field("model_path", &self.inner.model_path)
            .field("tokenizer_path", &self.inner.tokenizer_path)
            .field("max_seq_len", &self.inner.max_seq_len)
            .field("dim", &self.inner.dim)
            .finish()
    }
}

#[async_trait]
impl Embedder for OnnxEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let this = self.clone();
        let input = text.to_owned();
        tokio::task::spawn_blocking(move || this.run_embed(&input))
            .await
            .map_err(|e| EmbedError::Input { message: format!("worker join: {e}") })?
    }

    fn dimension(&self) -> usize {
        self.inner.dim
    }
}

/// Mask-weighted mean pooling of a `[seq, dim]` tensor.
fn mean_pool(data: &[f32], mask: &[i64], seq: usize, dim: usize) -> Vec<f32> {
    let mut out = vec![0.0_f32; dim];
    let mut active = 0.0_f32;
    for i in 0..seq {
        if mask.get(i).copied().unwrap_or(0) == 0 {
            continue;
        }
        active += 1.0;
        let offset = i * dim;
        for (o, v) in out.iter_mut().zip(&data[offset..offset + dim]) {
            *o += *v;
        }
    }
    if active > 0.0 {
        for v in &mut out {
            *v /= active;
        }
    }
    // L2 normalize for cosine comparability.
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for v in &mut out {
            *v /= norm;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires `libonnxruntime.dylib` on the host (operator-provided) since
    /// the `ort` crate uses dynamic loading. Skipped by default so CI without
    /// ONNX Runtime installed still passes.
    #[test]
    #[ignore = "requires libonnxruntime.dylib (operator-provided)"]
    fn load_errors_on_missing_model_file() {
        let err = OnnxEmbedder::load(
            "/definitely/does/not/exist/model.onnx",
            "/definitely/does/not/exist/tokenizer.json",
        )
        .unwrap_err();
        assert!(matches!(err, EmbedError::Load { .. }), "got {err:?}");
    }

    #[test]
    fn mean_pool_averages_active_positions() {
        // seq=2, dim=3; second position is masked out, first is active.
        let data = vec![1.0, 2.0, 2.0, 9.0, 9.0, 9.0];
        let mask = vec![1, 0];
        let pooled = mean_pool(&data, &mask, 2, 3);
        // After averaging (1 active row) we get [1, 2, 2]; then L2-normalize.
        let expected_norm: f32 = (1.0_f32 + 4.0 + 4.0).sqrt();
        let expected = vec![1.0 / expected_norm, 2.0 / expected_norm, 2.0 / expected_norm];
        for (a, b) in pooled.iter().zip(&expected) {
            assert!((a - b).abs() < 1e-6, "pooled={pooled:?}, expected={expected:?}");
        }
    }

    #[test]
    fn mean_pool_empty_mask_returns_zero_vector() {
        let data = vec![1.0, 2.0, 3.0];
        let mask = vec![0];
        let pooled = mean_pool(&data, &mask, 1, 3);
        assert_eq!(pooled, vec![0.0, 0.0, 0.0]);
    }
}
