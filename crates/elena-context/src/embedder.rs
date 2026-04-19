//! The [`Embedder`] trait + two built-in impls ([`NullEmbedder`] and
//! [`FakeEmbedder`] for tests).

use async_trait::async_trait;

use crate::error::EmbedError;

/// Expected embedding dimension for e5-small-v2. Callers can ignore this
/// when using [`NullEmbedder`] (which returns an empty vec).
pub const EMBED_DIM: usize = 384;

/// Embeds text into a fixed-dimension vector.
///
/// Every real impl returns a non-empty `Vec<f32>` of [`Embedder::dimension`]
/// entries. [`NullEmbedder`] is the exception — it advertises `dimension() == 0`
/// and returns an empty vec, which [`crate::ContextManager`] interprets as
/// "retrieval unavailable — fall back to recency".
#[async_trait]
pub trait Embedder: Send + Sync + 'static {
    /// Embed `text`. Empty input is allowed; concrete impls decide whether to
    /// return an error or an empty vector.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;

    /// The fixed output dimension. 0 means "retrieval disabled".
    fn dimension(&self) -> usize;
}

/// No-op embedder. Always returns an empty vector.
///
/// Installed when the operator hasn't configured a model path, so the agentic
/// loop still runs — it just drops to a recency window in place of retrieval.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullEmbedder;

#[async_trait]
impl Embedder for NullEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(Vec::new())
    }

    fn dimension(&self) -> usize {
        0
    }
}

/// Deterministic test embedder. Maps a text to a 384-dim vector by hashing
/// the input into a small set of hot coordinates — good enough for
/// similarity-ranking assertions without the 130MB real model.
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeEmbedder;

#[async_trait]
impl Embedder for FakeEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(hash_vector(text))
    }

    fn dimension(&self) -> usize {
        EMBED_DIM
    }
}

fn hash_vector(text: &str) -> Vec<f32> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let seed = hasher.finish();
    // Spread 8 hot entries across the vector via the seed. The `i` loop is
    // bounded at 8, so `as u16` on it never truncates; the seed's
    // modulo-reduction to `EMBED_DIM` is the whole point.
    let mut out = vec![0.0_f32; EMBED_DIM];
    for i in 0..8_u16 {
        let scaled = seed.wrapping_mul(u64::from(i) + 1);
        // Modulo by a small `usize` bound (`EMBED_DIM` = 384) fits safely.
        let idx = usize::try_from(scaled % (EMBED_DIM as u64)).unwrap_or(0);
        out[idx] += 1.0 / f32::from(i + 1);
    }
    // Normalize so cosine distance behaves predictably.
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut out {
            *x /= norm;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_embedder_returns_empty() {
        let e = NullEmbedder;
        assert_eq!(e.dimension(), 0);
        let v = e.embed("hello").await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn fake_embedder_is_deterministic() {
        let e = FakeEmbedder;
        assert_eq!(e.dimension(), EMBED_DIM);
        let a = e.embed("hello world").await.unwrap();
        let b = e.embed("hello world").await.unwrap();
        assert_eq!(a.len(), EMBED_DIM);
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn fake_embedder_differs_across_inputs() {
        let e = FakeEmbedder;
        let a = e.embed("alpha").await.unwrap();
        let b = e.embed("bravo").await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn fake_embedder_unit_normalized() {
        let e = FakeEmbedder;
        let v = e.embed("normalize").await.unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm = {norm}");
    }
}
