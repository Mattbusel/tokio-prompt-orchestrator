//! # Semantic Cache
//!
//! LRU-evicting similarity cache for prompt/response pairs.
//!
//! Prompts are embedded into a 64-dimensional TF-IDF-style vector using FNV-1a
//! token hashing, then L2-normalised.  Cache lookup returns a stored response
//! if the cosine similarity between the query embedding and any stored embedding
//! is at or above the configured threshold.

use std::collections::HashMap;
use std::time::Instant;

// ── Embedding ─────────────────────────────────────────────────────────────────

const DIM: usize = 64;

/// FNV-1a 32-bit hash of a byte slice.
fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for byte in s.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// Produce a deterministic TF-IDF-style 64-dim embedding for `prompt`.
///
/// Each whitespace-separated token is hashed with FNV-1a and its count is
/// accumulated into the dimension `hash % DIM`.  The resulting vector is then
/// L2-normalised.
pub fn embed_prompt(prompt: &str) -> Vec<f32> {
    let mut vec = vec![0.0_f32; DIM];

    for token in prompt.split_whitespace() {
        let h = fnv1a(token) as usize % DIM;
        vec[h] += 1.0;
    }

    // L2 normalise
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vec {
            *v /= norm;
        }
    }

    vec
}

/// Cosine similarity between two equal-length vectors.
///
/// Returns 0.0 if either vector is zero-length.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

// ── CacheEntry ────────────────────────────────────────────────────────────────

/// A single entry stored in the [`SemanticCache`].
pub struct CacheEntry {
    /// The original prompt text.
    pub prompt: String,
    /// The stored response.
    pub response: String,
    /// Embedding vector for the prompt.
    pub embedding: Vec<f32>,
    /// Number of times this entry has been returned as a cache hit.
    pub hit_count: u64,
    /// Wall-clock time this entry was inserted.
    pub timestamp: Instant,
}

// ── CacheStats ────────────────────────────────────────────────────────────────

/// Aggregate statistics for a [`SemanticCache`].
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Number of successful cache lookups.
    pub hits: u64,
    /// Number of failed cache lookups.
    pub misses: u64,
    /// Number of entries evicted due to capacity limits.
    pub evictions: u64,
    /// Current number of entries in the cache.
    pub size: usize,
}

// ── SemanticCache ─────────────────────────────────────────────────────────────

/// LRU-evicting semantic similarity cache.
///
/// Internally maintains an ordered list of keys (front = most recently used,
/// back = least recently used) and a `HashMap` of entries.  On lookup the
/// matching key is moved to the front; on insert an LRU eviction is performed
/// when the cache is full.
pub struct SemanticCache {
    capacity: usize,
    threshold: f64,
    /// Insertion-order list: front = MRU, back = LRU.
    order: Vec<String>,
    /// Key → entry storage.
    entries: HashMap<String, CacheEntry>,
    /// Aggregate statistics.
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl SemanticCache {
    /// Create a new `SemanticCache` with the given `capacity` and cosine
    /// similarity `threshold` in `[0.0, 1.0]`.
    pub fn new(capacity: usize, similarity_threshold: f64) -> Self {
        Self {
            capacity,
            threshold: similarity_threshold,
            order: Vec::with_capacity(capacity),
            entries: HashMap::with_capacity(capacity),
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// Look up `prompt` in the cache.
    ///
    /// Returns `Some(&str)` with the stored response if any entry's embedding
    /// has cosine similarity ≥ `threshold` with `prompt`'s embedding.
    /// The best match is returned and promoted to MRU position.
    pub fn lookup(&mut self, prompt: &str) -> Option<&str> {
        let query_emb = embed_prompt(prompt);

        // Find the best matching key.
        let mut best_key: Option<String> = None;
        let mut best_sim = -1.0_f32;

        for (key, entry) in &self.entries {
            let sim = cosine_similarity(&query_emb, &entry.embedding);
            if sim > best_sim {
                best_sim = sim;
                best_key = Some(key.clone());
            }
        }

        if best_sim >= self.threshold as f32 {
            let key = best_key?;
            // Move to front (MRU).
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
                self.order.insert(0, key.clone());
            }
            let entry = self.entries.get_mut(&key)?;
            entry.hit_count += 1;
            self.hits += 1;
            // Return a reference to the response.
            Some(self.entries[&key].response.as_str())
        } else {
            self.misses += 1;
            None
        }
    }

    /// Insert a prompt/response pair into the cache.
    ///
    /// If the cache is full the LRU entry is evicted first.
    /// If `prompt` already exists (exact key match) it is updated in place.
    pub fn insert(&mut self, prompt: &str, response: String) {
        let key = prompt.to_string();

        if self.entries.contains_key(&key) {
            // Update existing entry and promote to MRU.
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.response = response;
                entry.timestamp = Instant::now();
            }
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
                self.order.insert(0, key);
            }
            return;
        }

        if self.entries.len() >= self.capacity {
            self.evict_lru();
        }

        let embedding = embed_prompt(prompt);
        let entry = CacheEntry {
            prompt: key.clone(),
            response,
            embedding,
            hit_count: 0,
            timestamp: Instant::now(),
        };
        self.entries.insert(key.clone(), entry);
        self.order.insert(0, key);
    }

    /// Remove the least recently used entry from the cache.
    pub fn evict_lru(&mut self) {
        if let Some(lru_key) = self.order.pop() {
            self.entries.remove(&lru_key);
            self.evictions += 1;
        }
    }

    /// Return a statistics snapshot.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            size: self.entries.len(),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match_lookup() {
        let mut cache = SemanticCache::new(10, 0.99);
        cache.insert("hello world", "response A".to_string());
        let result = cache.lookup("hello world");
        assert_eq!(result, Some("response A"));
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 0);
    }

    #[test]
    fn test_similar_prompt_retrieval() {
        let mut cache = SemanticCache::new(10, 0.5);
        cache.insert("the quick brown fox", "fox response".to_string());
        // Overlapping tokens → similar embedding.
        let result = cache.lookup("quick brown fox");
        assert!(result.is_some(), "Expected similar prompt to hit cache");
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn test_dissimilar_prompt_is_miss() {
        let mut cache = SemanticCache::new(10, 0.99);
        cache.insert("hello world", "response A".to_string());
        let result = cache.lookup("completely different zzzz");
        assert!(result.is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn test_lru_eviction() {
        let mut cache = SemanticCache::new(2, 1.0);
        cache.insert("prompt one", "resp1".to_string());
        cache.insert("prompt two", "resp2".to_string());
        // Access "prompt one" → becomes MRU; "prompt two" is now LRU.
        let _ = cache.lookup("prompt one");
        // Insert a third entry → "prompt two" (LRU) should be evicted.
        cache.insert("prompt three", "resp3".to_string());
        assert_eq!(cache.stats().evictions, 1);
        assert_eq!(cache.stats().size, 2);
        // "prompt two" was evicted.
        assert!(cache.lookup("prompt two").is_none());
        // "prompt one" still present.
        assert!(cache.lookup("prompt one").is_some());
    }

    #[test]
    fn test_evict_lru_explicit() {
        let mut cache = SemanticCache::new(3, 1.0);
        cache.insert("a a a", "ra".to_string());
        cache.insert("b b b", "rb".to_string());
        assert_eq!(cache.stats().size, 2);
        cache.evict_lru();
        // LRU is "a a a" (inserted first, never accessed since).
        assert_eq!(cache.stats().size, 1);
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_known_value() {
        // a = (3,4,0), b = (4,3,0); dot=24, |a|=5, |b|=5 → sim=24/25=0.96
        let a = vec![3.0_f32, 4.0, 0.0];
        let b = vec![4.0_f32, 3.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.96).abs() < 1e-5, "sim={sim}");
    }

    #[test]
    fn test_embed_prompt_is_normalised() {
        let emb = embed_prompt("hello world foo bar");
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "norm={norm}");
    }

    #[test]
    fn test_stats_size_tracks_inserts() {
        let mut cache = SemanticCache::new(5, 1.0);
        cache.insert("p1", "r1".to_string());
        cache.insert("p2", "r2".to_string());
        assert_eq!(cache.stats().size, 2);
    }

    #[test]
    fn test_update_existing_entry() {
        let mut cache = SemanticCache::new(5, 1.0);
        cache.insert("hello world", "v1".to_string());
        cache.insert("hello world", "v2".to_string());
        // Should still be 1 entry.
        assert_eq!(cache.stats().size, 1);
        let r = cache.lookup("hello world").unwrap();
        assert_eq!(r, "v2");
    }
}
