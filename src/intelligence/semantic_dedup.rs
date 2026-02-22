#![allow(
    missing_docs,
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::redundant_closure,
    clippy::derivable_impls,
    clippy::unwrap_or_default,
    dead_code,
    private_interfaces
)]
#![cfg(feature = "intelligence")]

//! # Stage: Semantic Deduplicator
//! Embedding-based deduplication using deterministic pseudo-embeddings.
//! Falls back to exact-key matching when similarity index is full.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};
use thiserror::Error;
use tracing::{debug, info};

#[derive(Debug, Error)]
pub enum SemanticDedupError {
    #[error("embedding service unavailable")]
    EmbeddingUnavailable,
    #[error("semantic index is full (max {0} entries)")]
    IndexFull(usize),
    #[error("index lock poisoned")]
    LockPoisoned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticDedupConfig {
    pub similarity_threshold: f64,
    pub max_index_size: usize,
    pub embedding_dim: usize,
    pub exact_fallback: bool,
}
impl Default for SemanticDedupConfig {
    fn default() -> Self {
        Self {
            similarity_threshold: 0.92,
            max_index_size: 10_000,
            embedding_dim: 64,
            exact_fallback: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarityMatch {
    pub key: String,
    pub similarity: f64,
    pub cached_response: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingEntry {
    pub key: String,
    pub embedding: Vec<f64>,
    pub response: Option<String>,
    pub hit_count: u64,
}

pub struct SemanticDedup {
    index: Arc<RwLock<Vec<EmbeddingEntry>>>,
    config: SemanticDedupConfig,
    exact_hits: Arc<AtomicU64>,
    semantic_hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl SemanticDedup {
    pub fn new(config: SemanticDedupConfig) -> Self {
        Self {
            index: Arc::new(RwLock::new(Vec::new())),
            config,
            exact_hits: Arc::new(AtomicU64::new(0)),
            semantic_hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Deterministic pseudo-embedding: hash each word, project via sin/cos of hash values.
    pub fn embed(text: &str, dim: usize) -> Vec<f64> {
        if dim == 0 {
            return Vec::new();
        }
        let mut acc = vec![0.0f64; dim];
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.is_empty() {
            return acc;
        }
        for word in &words {
            let mut h = DefaultHasher::new();
            word.hash(&mut h);
            let hv = h.finish();
            for i in 0..dim {
                let angle = (hv.wrapping_add(i as u64) as f64) * std::f64::consts::PI / dim as f64;
                acc[i] += if i % 2 == 0 { angle.sin() } else { angle.cos() };
            }
        }
        // Normalize to unit vector
        let norm: f64 = acc.iter().map(|v| v * v).sum::<f64>().sqrt();
        if norm > 1e-9 {
            acc.iter_mut().for_each(|v| *v /= norm);
        }
        acc
    }

    pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let mag_a: f64 = a.iter().map(|v| v * v).sum::<f64>().sqrt();
        let mag_b: f64 = b.iter().map(|v| v * v).sum::<f64>().sqrt();
        if mag_a < 1e-9 || mag_b < 1e-9 {
            return 0.0;
        }
        (dot / (mag_a * mag_b)).clamp(-1.0, 1.0)
    }

    pub fn lookup(&self, prompt: &str) -> Result<Option<SimilarityMatch>, SemanticDedupError> {
        let embedding = Self::embed(prompt, self.config.embedding_dim);
        let guard = self
            .index
            .read()
            .map_err(|_| SemanticDedupError::LockPoisoned)?;
        if guard.is_empty() {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        let mut best_sim = -1.0f64;
        let mut best_entry: Option<&EmbeddingEntry> = None;
        for entry in guard.iter() {
            let sim = Self::cosine_similarity(&embedding, &entry.embedding);
            if sim > best_sim {
                best_sim = sim;
                best_entry = Some(entry);
            }
        }
        if best_sim >= self.config.similarity_threshold {
            if let Some(entry) = best_entry {
                if entry.key == Self::make_key(prompt) {
                    self.exact_hits.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.semantic_hits.fetch_add(1, Ordering::Relaxed);
                }
                debug!(
                    key = entry.key.as_str(),
                    similarity = best_sim,
                    "semantic match found"
                );
                return Ok(Some(SimilarityMatch {
                    key: entry.key.clone(),
                    similarity: best_sim,
                    cached_response: entry.response.clone(),
                }));
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        Ok(None)
    }

    fn make_key(text: &str) -> String {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        format!("{:016x}", h.finish())[..16].to_string()
    }

    pub fn insert(
        &self,
        prompt: &str,
        response: Option<String>,
    ) -> Result<String, SemanticDedupError> {
        let embedding = Self::embed(prompt, self.config.embedding_dim);
        let key = Self::make_key(prompt);
        let mut guard = self
            .index
            .write()
            .map_err(|_| SemanticDedupError::LockPoisoned)?;
        // Update if key already exists
        if let Some(entry) = guard.iter_mut().find(|e| e.key == key) {
            entry.hit_count += 1;
            if response.is_some() {
                entry.response = response;
            }
            return Ok(key);
        }
        // Evict LRU if full and fallback enabled
        if guard.len() >= self.config.max_index_size {
            if self.config.exact_fallback {
                if let Some(lru_idx) = guard
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.hit_count)
                    .map(|(i, _)| i)
                {
                    guard.remove(lru_idx);
                }
            } else {
                return Err(SemanticDedupError::IndexFull(self.config.max_index_size));
            }
        }
        guard.push(EmbeddingEntry {
            key: key.clone(),
            embedding,
            response,
            hit_count: 0,
        });
        info!(
            key = key.as_str(),
            index_len = guard.len(),
            "prompt inserted into semantic index"
        );
        Ok(key)
    }

    pub fn hit_rate(&self) -> f64 {
        let hits =
            self.exact_hits.load(Ordering::Relaxed) + self.semantic_hits.load(Ordering::Relaxed);
        let total = hits + self.misses.load(Ordering::Relaxed);
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.exact_hits.load(Ordering::Relaxed),
            self.semantic_hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // 1. Config defaults
    // ---------------------------------------------------------------

    #[test]
    fn test_config_default_similarity_threshold() {
        let cfg = SemanticDedupConfig::default();
        assert!(
            (cfg.similarity_threshold - 0.92).abs() < f64::EPSILON,
            "expected threshold 0.92, got {}",
            cfg.similarity_threshold
        );
    }

    #[test]
    fn test_config_default_max_index_size() {
        let cfg = SemanticDedupConfig::default();
        assert_eq!(cfg.max_index_size, 10_000);
    }

    #[test]
    fn test_config_default_embedding_dim() {
        let cfg = SemanticDedupConfig::default();
        assert_eq!(cfg.embedding_dim, 64);
    }

    #[test]
    fn test_config_default_exact_fallback() {
        let cfg = SemanticDedupConfig::default();
        assert!(cfg.exact_fallback);
    }

    // ---------------------------------------------------------------
    // 2. embed() tests
    // ---------------------------------------------------------------

    #[test]
    fn test_embed_deterministic_same_text_same_result() {
        let a = SemanticDedup::embed("hello world", 64);
        let b = SemanticDedup::embed("hello world", 64);
        assert_eq!(a, b, "embed must be deterministic for identical input");
    }

    #[test]
    fn test_embed_dimension_matches_requested() {
        for dim in [1, 16, 64, 128, 256] {
            let emb = SemanticDedup::embed("test text", dim);
            assert_eq!(
                emb.len(),
                dim,
                "expected dimension {}, got {}",
                dim,
                emb.len()
            );
        }
    }

    #[test]
    fn test_embed_zero_dim_returns_empty() {
        let emb = SemanticDedup::embed("anything", 0);
        assert!(emb.is_empty(), "zero dim should return empty vec");
    }

    #[test]
    fn test_embed_empty_text_returns_zero_vector() {
        let emb = SemanticDedup::embed("", 64);
        assert_eq!(emb.len(), 64);
        for val in &emb {
            assert!(
                val.abs() < f64::EPSILON,
                "empty text should produce zero vector, found {}",
                val
            );
        }
    }

    #[test]
    fn test_embed_whitespace_only_returns_zero_vector() {
        let emb = SemanticDedup::embed("   \t\n  ", 32);
        assert_eq!(emb.len(), 32);
        for val in &emb {
            assert!(val.abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_embed_different_texts_produce_different_embeddings() {
        let a = SemanticDedup::embed("the quick brown fox", 64);
        let b = SemanticDedup::embed("jumped over the lazy dog", 64);
        assert_ne!(a, b, "different texts should produce different embeddings");
    }

    #[test]
    fn test_embed_normalized_to_unit_vector() {
        let emb = SemanticDedup::embed("normalize me please", 64);
        let magnitude: f64 = emb.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!(
            (magnitude - 1.0).abs() < 1e-6,
            "expected unit vector magnitude ~1.0, got {}",
            magnitude
        );
    }

    #[test]
    fn test_embed_single_word_normalized() {
        let emb = SemanticDedup::embed("singleton", 128);
        let magnitude: f64 = emb.iter().map(|v| v * v).sum::<f64>().sqrt();
        assert!(
            (magnitude - 1.0).abs() < 1e-6,
            "single word embedding should be unit vector, got magnitude {}",
            magnitude
        );
    }

    // ---------------------------------------------------------------
    // 3. cosine_similarity() tests
    // ---------------------------------------------------------------

    #[test]
    fn test_cosine_similarity_identical_vectors_returns_one() {
        let v = vec![1.0, 2.0, 3.0, 4.0];
        let sim = SemanticDedup::cosine_similarity(&v, &v);
        assert!(
            (sim - 1.0).abs() < 1e-9,
            "identical vectors should have similarity 1.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_orthogonal_vectors_near_zero() {
        // Orthogonal in 2D
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            sim.abs() < 1e-9,
            "orthogonal vectors should have similarity ~0.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_empty_vectors_returns_zero() {
        let a: Vec<f64> = vec![];
        let b: Vec<f64> = vec![];
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            sim.abs() < f64::EPSILON,
            "empty vectors should return 0.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_mismatched_lengths_returns_zero() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            sim.abs() < f64::EPSILON,
            "mismatched lengths should return 0.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_negated_vector_returns_negative_one() {
        let a = vec![1.0, 2.0, 3.0];
        let b: Vec<f64> = a.iter().map(|v| -v).collect();
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            (sim - (-1.0)).abs() < 1e-9,
            "negated vector should have similarity -1.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_zero_magnitude_returns_zero() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            sim.abs() < f64::EPSILON,
            "zero-magnitude vector should return 0.0, got {}",
            sim
        );
    }

    #[test]
    fn test_cosine_similarity_scaled_vectors_returns_one() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![2.0, 4.0, 6.0];
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            (sim - 1.0).abs() < 1e-9,
            "parallel vectors should have similarity 1.0, got {}",
            sim
        );
    }

    // ---------------------------------------------------------------
    // 4. insert() + lookup() tests
    // ---------------------------------------------------------------

    #[test]
    fn test_lookup_empty_index_returns_none() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        let result = dedup.lookup("any prompt").unwrap();
        assert!(result.is_none(), "lookup on empty index should return None");
    }

    #[test]
    fn test_insert_then_lookup_same_text_finds_match() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        let key = dedup.insert("hello world", Some("response-1".into())).unwrap();
        let result = dedup.lookup("hello world").unwrap();
        assert!(result.is_some(), "lookup should find exact match");
        let m = result.unwrap();
        assert_eq!(m.key, key);
        assert!(
            (m.similarity - 1.0).abs() < 1e-9,
            "exact same text should have similarity 1.0"
        );
    }

    #[test]
    fn test_insert_returns_cached_response_on_lookup() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup
            .insert("cache me", Some("cached-response".into()))
            .unwrap();
        let m = dedup.lookup("cache me").unwrap().unwrap();
        assert_eq!(
            m.cached_response,
            Some("cached-response".into()),
            "lookup should return the cached response"
        );
    }

    #[test]
    fn test_insert_same_key_twice_increments_hit_count() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        let key1 = dedup.insert("duplicate prompt", None).unwrap();
        let key2 = dedup.insert("duplicate prompt", None).unwrap();
        assert_eq!(key1, key2, "same prompt should produce same key");

        // Verify hit_count incremented by reading the index directly
        let guard = dedup.index.read().unwrap();
        let entry = guard.iter().find(|e| e.key == key1).unwrap();
        assert_eq!(
            entry.hit_count, 1,
            "second insert should bump hit_count to 1"
        );
    }

    #[test]
    fn test_insert_same_key_updates_response() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup.insert("prompt", Some("first".into())).unwrap();
        dedup.insert("prompt", Some("second".into())).unwrap();

        let guard = dedup.index.read().unwrap();
        let entry = guard.iter().find(|e| e.response.is_some()).unwrap();
        assert_eq!(entry.response, Some("second".into()));
    }

    #[test]
    fn test_index_full_without_fallback_returns_error() {
        let cfg = SemanticDedupConfig {
            max_index_size: 2,
            exact_fallback: false,
            ..SemanticDedupConfig::default()
        };
        let dedup = SemanticDedup::new(cfg);
        dedup.insert("prompt-a", None).unwrap();
        dedup.insert("prompt-b", None).unwrap();

        let result = dedup.insert("prompt-c", None);
        assert!(result.is_err(), "should return error when index is full");
        match result.unwrap_err() {
            SemanticDedupError::IndexFull(cap) => assert_eq!(cap, 2),
            other => panic!("expected IndexFull, got {:?}", other),
        }
    }

    #[test]
    fn test_index_full_with_fallback_evicts_lru_entry() {
        let cfg = SemanticDedupConfig {
            max_index_size: 2,
            exact_fallback: true,
            ..SemanticDedupConfig::default()
        };
        let dedup = SemanticDedup::new(cfg);
        // Insert two entries; first has hit_count 0
        dedup.insert("prompt-a", None).unwrap();
        dedup.insert("prompt-b", None).unwrap();

        // Bump prompt-b hit_count by re-inserting
        dedup.insert("prompt-b", None).unwrap();

        // Insert a third — should evict prompt-a (lower hit_count)
        let key_c = dedup.insert("prompt-c", None).unwrap();

        let guard = dedup.index.read().unwrap();
        assert_eq!(guard.len(), 2, "index should still be at max capacity");
        let keys: Vec<&str> = guard.iter().map(|e| e.key.as_str()).collect();
        assert!(
            keys.contains(&key_c.as_str()),
            "newly inserted key should be present"
        );
        // prompt-a's key should be gone (it had 0 hits vs prompt-b's 1)
        let key_a = SemanticDedup::make_key("prompt-a");
        assert!(
            !keys.contains(&key_a.as_str()),
            "LRU entry (prompt-a) should have been evicted"
        );
    }

    #[test]
    fn test_lookup_miss_increments_miss_counter() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup.insert("stored prompt", None).unwrap();
        // Lookup something completely different with low threshold match
        let cfg = SemanticDedupConfig {
            similarity_threshold: 0.9999,
            ..SemanticDedupConfig::default()
        };
        let dedup2 = SemanticDedup::new(cfg);
        dedup2.insert("stored prompt", None).unwrap();
        let _ = dedup2.lookup("totally unrelated xyzzy").unwrap();
        let (_, _, misses) = dedup2.stats();
        assert!(misses >= 1, "miss counter should be at least 1");
    }

    #[test]
    fn test_insert_none_response_then_lookup() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup.insert("no response prompt", None).unwrap();
        let m = dedup.lookup("no response prompt").unwrap().unwrap();
        assert_eq!(
            m.cached_response, None,
            "should return None for response when none was stored"
        );
    }

    // ---------------------------------------------------------------
    // 5. hit_rate() and stats() tests
    // ---------------------------------------------------------------

    #[test]
    fn test_hit_rate_initial_is_zero() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        assert!(
            dedup.hit_rate().abs() < f64::EPSILON,
            "initial hit rate should be 0.0"
        );
    }

    #[test]
    fn test_stats_initial_all_zeros() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        let (exact, semantic, misses) = dedup.stats();
        assert_eq!(exact, 0);
        assert_eq!(semantic, 0);
        assert_eq!(misses, 0);
    }

    #[test]
    fn test_stats_after_miss() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        // Lookup on empty index produces a miss
        let _ = dedup.lookup("nothing here").unwrap();
        let (exact, semantic, misses) = dedup.stats();
        assert_eq!(exact, 0);
        assert_eq!(semantic, 0);
        assert_eq!(misses, 1);
    }

    #[test]
    fn test_stats_after_exact_hit() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup.insert("exact query", None).unwrap();
        let _ = dedup.lookup("exact query").unwrap();
        let (exact, _semantic, _misses) = dedup.stats();
        assert_eq!(exact, 1, "exact hit should be counted");
    }

    #[test]
    fn test_hit_rate_after_mixed_lookups() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup.insert("known prompt", None).unwrap();

        // One hit
        let _ = dedup.lookup("known prompt").unwrap();
        // One miss (empty index won't match this)
        let cfg = SemanticDedupConfig {
            similarity_threshold: 1.0,
            ..SemanticDedupConfig::default()
        };
        let dedup2 = SemanticDedup::new(cfg);
        dedup2.insert("known prompt", None).unwrap();
        let _ = dedup2.lookup("known prompt").unwrap(); // exact match, sim=1.0, passes even threshold=1.0
        let _ = dedup2.lookup("unknown gibberish xyzzy").unwrap(); // miss

        let (exact, _semantic, misses) = dedup2.stats();
        let total = exact + misses;
        assert!(total > 0);
        let rate = dedup2.hit_rate();
        assert!(
            rate > 0.0 && rate < 1.0,
            "hit rate should be between 0 and 1, got {}",
            rate
        );
    }

    // ---------------------------------------------------------------
    // 6. make_key deterministic
    // ---------------------------------------------------------------

    #[test]
    fn test_make_key_deterministic() {
        let k1 = SemanticDedup::make_key("some prompt text");
        let k2 = SemanticDedup::make_key("some prompt text");
        assert_eq!(k1, k2, "make_key must be deterministic");
    }

    #[test]
    fn test_make_key_different_texts_differ() {
        let k1 = SemanticDedup::make_key("alpha");
        let k2 = SemanticDedup::make_key("beta");
        assert_ne!(k1, k2, "different texts should produce different keys");
    }

    #[test]
    fn test_make_key_length_is_16() {
        let k = SemanticDedup::make_key("any string");
        assert_eq!(k.len(), 16, "key should be 16 hex characters");
    }

    #[test]
    fn test_make_key_is_hex() {
        let k = SemanticDedup::make_key("hex check");
        assert!(
            k.chars().all(|c| c.is_ascii_hexdigit()),
            "key should only contain hex digits, got '{}'",
            k
        );
    }

    // ---------------------------------------------------------------
    // 7. Error variant display messages
    // ---------------------------------------------------------------

    #[test]
    fn test_error_embedding_unavailable_display() {
        let err = SemanticDedupError::EmbeddingUnavailable;
        assert_eq!(format!("{}", err), "embedding service unavailable");
    }

    #[test]
    fn test_error_index_full_display() {
        let err = SemanticDedupError::IndexFull(5000);
        assert_eq!(
            format!("{}", err),
            "semantic index is full (max 5000 entries)"
        );
    }

    #[test]
    fn test_error_lock_poisoned_display() {
        let err = SemanticDedupError::LockPoisoned;
        assert_eq!(format!("{}", err), "index lock poisoned");
    }

    // ---------------------------------------------------------------
    // 8. Edge cases and additional coverage
    // ---------------------------------------------------------------

    #[test]
    fn test_new_creates_empty_index() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        let guard = dedup.index.read().unwrap();
        assert!(guard.is_empty());
    }

    #[test]
    fn test_multiple_inserts_grow_index() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        dedup.insert("a", None).unwrap();
        dedup.insert("b", None).unwrap();
        dedup.insert("c", None).unwrap();
        let guard = dedup.index.read().unwrap();
        assert_eq!(guard.len(), 3);
    }

    #[test]
    fn test_embed_dim_one() {
        let emb = SemanticDedup::embed("test", 1);
        assert_eq!(emb.len(), 1);
        // Single-dimension unit vector magnitude should be 1.0
        assert!((emb[0].abs() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_single_element() {
        let a = vec![3.0];
        let b = vec![7.0];
        let sim = SemanticDedup::cosine_similarity(&a, &b);
        assert!(
            (sim - 1.0).abs() < 1e-9,
            "single positive elements should have similarity 1.0"
        );
    }

    #[test]
    fn test_insert_returns_consistent_key() {
        let dedup = SemanticDedup::new(SemanticDedupConfig::default());
        let k1 = dedup.insert("consistent key test", None).unwrap();
        let k2 = dedup.insert("consistent key test", None).unwrap();
        assert_eq!(k1, k2, "insert should return the same key for same prompt");
    }

    #[test]
    fn test_similarity_match_struct_fields() {
        let m = SimilarityMatch {
            key: "abc".into(),
            similarity: 0.95,
            cached_response: Some("resp".into()),
        };
        assert_eq!(m.key, "abc");
        assert!((m.similarity - 0.95).abs() < f64::EPSILON);
        assert_eq!(m.cached_response, Some("resp".into()));
    }

    #[test]
    fn test_embedding_entry_struct_fields() {
        let e = EmbeddingEntry {
            key: "k".into(),
            embedding: vec![0.5, 0.5],
            response: Some("r".into()),
            hit_count: 42,
        };
        assert_eq!(e.key, "k");
        assert_eq!(e.embedding.len(), 2);
        assert_eq!(e.response, Some("r".into()));
        assert_eq!(e.hit_count, 42);
    }

    #[test]
    fn test_config_custom_values() {
        let cfg = SemanticDedupConfig {
            similarity_threshold: 0.5,
            max_index_size: 100,
            embedding_dim: 32,
            exact_fallback: false,
        };
        assert!((cfg.similarity_threshold - 0.5).abs() < f64::EPSILON);
        assert_eq!(cfg.max_index_size, 100);
        assert_eq!(cfg.embedding_dim, 32);
        assert!(!cfg.exact_fallback);
    }

    #[test]
    fn test_lookup_returns_highest_similarity_match() {
        let cfg = SemanticDedupConfig {
            similarity_threshold: 0.0, // accept any similarity
            ..SemanticDedupConfig::default()
        };
        let dedup = SemanticDedup::new(cfg);
        dedup.insert("hello world", Some("hw".into())).unwrap();
        dedup.insert("goodbye world", Some("gw".into())).unwrap();

        // Lookup with exact text should find it with similarity 1.0
        let m = dedup.lookup("hello world").unwrap().unwrap();
        assert!(
            (m.similarity - 1.0).abs() < 1e-9,
            "exact text lookup should return similarity 1.0"
        );
        assert_eq!(m.cached_response, Some("hw".into()));
    }

    #[test]
    fn test_eviction_preserves_high_hit_count_entries() {
        let cfg = SemanticDedupConfig {
            max_index_size: 3,
            exact_fallback: true,
            ..SemanticDedupConfig::default()
        };
        let dedup = SemanticDedup::new(cfg);

        // Insert three entries
        dedup.insert("alpha", None).unwrap();
        dedup.insert("beta", None).unwrap();
        dedup.insert("gamma", None).unwrap();

        // Bump alpha and gamma hit counts
        dedup.insert("alpha", None).unwrap(); // hit_count = 1
        dedup.insert("alpha", None).unwrap(); // hit_count = 2
        dedup.insert("gamma", None).unwrap(); // hit_count = 1

        // Insert a 4th entry — should evict beta (hit_count = 0)
        dedup.insert("delta", None).unwrap();

        let guard = dedup.index.read().unwrap();
        assert_eq!(guard.len(), 3);
        let keys: Vec<&str> = guard.iter().map(|e| e.key.as_str()).collect();
        let beta_key = SemanticDedup::make_key("beta");
        assert!(
            !keys.contains(&beta_key.as_str()),
            "beta (lowest hit_count) should have been evicted"
        );
    }
}
