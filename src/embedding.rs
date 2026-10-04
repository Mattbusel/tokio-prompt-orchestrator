//! Text embeddings for semantic deduplication.
//!
//! An [`Embedder`] turns a prompt into a vector so that two prompts asking
//! the same thing in different words land close together. The
//! [`Deduplicator`](crate::enhanced::Deduplicator) uses one (see
//! [`Deduplicator::with_embedder`](crate::enhanced::Deduplicator::with_embedder))
//! to answer a paraphrased question from the cache instead of paying for a
//! second model call.
//!
//! Implementations live in [`integrations`](crate::integrations):
//!
//! | Feature | Type | Runs |
//! |---|---|---|
//! | `fastembed` | `FastEmbedder` | locally, on CPU, no API key (BGE, MiniLM and other ONNX models) |
//! | `async-openai` | `OpenAiEmbedder` | the OpenAI embeddings API, or any compatible server |
//! | `genai` | `GenaiEmbedder` | any provider genai supports embeddings for |
//!
//! Or implement the trait for your own model.

use async_trait::async_trait;

use crate::OrchestratorError;

/// Turns text into a vector whose direction captures its meaning.
///
/// Vectors from one embedder must all have the same length. They do not need
/// to be normalised: similarity is measured with [`cosine_similarity`].
#[async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// Embed one text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, OrchestratorError>;
}

/// Cosine similarity of two vectors, in `[-1.0, 1.0]`.
///
/// Returns `0.0` when the lengths differ or either vector is all zeros, so a
/// malformed embedding never counts as a match.
///
/// ```
/// use tokio_prompt_orchestrator::embedding::cosine_similarity;
///
/// assert!((cosine_similarity(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
/// assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
/// assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
/// ```
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn similarity_is_bounded_and_symmetric(
            a in proptest::collection::vec(-1e3f32..1e3, 1..64),
            seed in proptest::collection::vec(-1e3f32..1e3, 64),
        ) {
            let b: Vec<f32> = seed.into_iter().take(a.len()).collect();
            let ab = cosine_similarity(&a, &b);
            let ba = cosine_similarity(&b, &a);
            prop_assert!((-1.0001..=1.0001).contains(&ab), "out of range: {ab}");
            prop_assert!((ab - ba).abs() < 1e-5);
        }

        #[test]
        fn a_vector_matches_itself(a in proptest::collection::vec(-1e3f32..1e3, 1..64)) {
            prop_assume!(a.iter().any(|x| *x != 0.0));
            prop_assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-4);
        }

        #[test]
        fn scaling_does_not_change_similarity(
            a in proptest::collection::vec(0.1f32..1e3, 1..64),
            k in 0.01f32..100.0,
        ) {
            let scaled: Vec<f32> = a.iter().map(|x| x * k).collect();
            prop_assert!((cosine_similarity(&a, &scaled) - 1.0).abs() < 1e-4);
        }
    }
}
