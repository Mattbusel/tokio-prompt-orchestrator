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
