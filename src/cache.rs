//! # Prompt Cache
//!
//! Content-addressed, in-process LRU cache for LLM inference responses.
//!
//! Cache keys are the SHA-256 hash of `(model_id + prompt_text)`.  Entries
//! carry a TTL; expired entries are evicted lazily on [`PromptCache::get`] and
//! eagerly via [`PromptCache::evict_expired`].  When the cache reaches
//! [`CacheConfig::max_entries`] the least-recently-used entry is evicted to
//! make room (LRU via `VecDeque` order tracking + `HashMap` for O(1) lookup).
//!
//! The cache is fully thread-safe: the public handle is `Arc<Mutex<CacheInner>>`.
//!
//! ## Example
//!
//! ```
//! use tokio_prompt_orchestrator::cache::{CacheConfig, PromptCache};
//! use std::time::Duration;
//!
//! let cfg = CacheConfig {
//!     max_entries: 100,
//!     default_ttl: Duration::from_secs(300),
//!     max_prompt_len: 8192,
//! };
//! let cache = PromptCache::new(cfg);
//!
//! // Cache a response.
//! cache.insert("gpt-4o", "Hello, world!", vec!["Hi there!".to_string()], None);
//!
//! // Retrieve it.
//! let result = cache.get("gpt-4o", "Hello, world!");
//! assert!(result.is_some());
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

// ── Key helpers ───────────────────────────────────────────────────────────────

/// Compute the SHA-256 cache key for `(model_id, prompt)`.
fn compute_key(model_id: &str, prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(model_id.as_bytes());
    hasher.update(b"\x00"); // separator
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for [`PromptCache`].
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of entries retained in the cache.
    ///
    /// When this limit is reached, the least-recently-used entry is evicted
    /// before a new one is inserted.
    pub max_entries: usize,
    /// TTL applied to entries that do not supply their own TTL on insertion.
    pub default_ttl: Duration,
    /// Prompts longer than this (in bytes) are never cached.
    ///
    /// This prevents the cache from holding very large strings in memory.
    /// Set to `usize::MAX` to disable the limit.
    pub max_prompt_len: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1_024,
            default_ttl: Duration::from_secs(300),
            max_prompt_len: 16_384,
        }
    }
}

// ── Entry ─────────────────────────────────────────────────────────────────────

/// A single cached inference response.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The cached response tokens/chunks.
    pub response: Vec<String>,
    /// Instant the entry was first inserted.
    pub created_at: Instant,
    /// Number of times this entry has been returned on a cache hit.
    pub hits: u64,
    /// How long this entry lives before it is considered expired.
    pub ttl: Duration,
}

impl CacheEntry {
    /// Return `true` when the entry has lived past its TTL.
    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= self.ttl
    }
}

// ── Stats ─────────────────────────────────────────────────────────────────────

/// Aggregate statistics for a [`PromptCache`] instance.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Current number of live (non-expired) entries in the cache.
    pub entries: usize,
    /// Cumulative number of cache hits since the cache was created.
    pub total_hits: u64,
    /// Cumulative number of cache misses since the cache was created.
    pub total_misses: u64,
    /// Hit rate: `total_hits / (total_hits + total_misses)`, or `0.0` when no
    /// requests have been made.
    pub hit_rate: f64,
    /// Number of entries evicted (LRU or TTL expiry) since the cache was
    /// created.
    pub evictions: u64,
}

// ── Inner state ───────────────────────────────────────────────────────────────

struct CacheInner {
    /// Map from SHA-256 key string → entry.
    map: HashMap<String, CacheEntry>,
    /// LRU order: front = least recently used, back = most recently used.
    order: VecDeque<String>,
    config: CacheConfig,
    total_hits: u64,
    total_misses: u64,
    evictions: u64,
}

impl CacheInner {
    fn new(config: CacheConfig) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            config,
            total_hits: 0,
            total_misses: 0,
            evictions: 0,
        }
    }

    /// Move `key` to the back of the LRU queue (most recently used).
    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_owned());
    }

    /// Evict LRU entries until `map.len() < max_entries`.
    fn evict_lru_to_fit(&mut self) {
        while self.map.len() >= self.config.max_entries {
            if let Some(lru_key) = self.order.pop_front() {
                if self.map.remove(&lru_key).is_some() {
                    self.evictions += 1;
                }
            } else {
                break;
            }
        }
    }

    fn get(&mut self, key: &str) -> Option<Vec<String>> {
        // Check for expiry first.
        if let Some(entry) = self.map.get(key) {
            if entry.is_expired() {
                self.map.remove(key);
                if let Some(pos) = self.order.iter().position(|k| k == key) {
                    self.order.remove(pos);
                }
                self.evictions += 1;
                self.total_misses += 1;
                return None;
            }
        }
        if let Some(entry) = self.map.get_mut(key) {
            entry.hits += 1;
            self.total_hits += 1;
            let response = entry.response.clone();
            self.touch(key);
            Some(response)
        } else {
            self.total_misses += 1;
            None
        }
    }

    fn insert(&mut self, key: String, response: Vec<String>, ttl: Duration) {
        if self.map.contains_key(&key) {
            // Update in place, reset TTL.
            if let Some(entry) = self.map.get_mut(&key) {
                entry.response = response;
                entry.created_at = Instant::now();
                entry.ttl = ttl;
            }
            self.touch(&key);
            return;
        }
        self.evict_lru_to_fit();
        self.order.push_back(key.clone());
        self.map.insert(
            key,
            CacheEntry {
                response,
                created_at: Instant::now(),
                hits: 0,
                ttl,
            },
        );
    }

    fn evict_expired(&mut self) -> usize {
        let expired: Vec<String> = self
            .map
            .iter()
            .filter(|(_, e)| e.is_expired())
            .map(|(k, _)| k.clone())
            .collect();
        let count = expired.len();
        for key in &expired {
            self.map.remove(key);
            if let Some(pos) = self.order.iter().position(|k| k == key) {
                self.order.remove(pos);
            }
            self.evictions += 1;
        }
        count
    }

    fn stats(&self) -> CacheStats {
        let total = self.total_hits + self.total_misses;
        CacheStats {
            entries: self.map.len(),
            total_hits: self.total_hits,
            total_misses: self.total_misses,
            hit_rate: if total == 0 {
                0.0
            } else {
                self.total_hits as f64 / total as f64
            },
            evictions: self.evictions,
        }
    }

    fn flush(&mut self) {
        let count = self.map.len();
        self.map.clear();
        self.order.clear();
        self.evictions += count as u64;
    }
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Thread-safe, content-addressed LRU prompt cache.
///
/// Clone-cheap: all clones share the same underlying data.
#[derive(Clone)]
pub struct PromptCache {
    inner: Arc<Mutex<CacheInner>>,
}

impl PromptCache {
    /// Create a new cache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CacheInner::new(config))),
        }
    }

    /// Look up a cached response for `(model_id, prompt)`.
    ///
    /// Returns `None` on a cache miss or if the entry has expired.
    /// Updates the LRU order and hit counter on a hit.
    ///
    /// Prompts longer than `max_prompt_len` are always treated as misses.
    pub fn get(&self, model_id: &str, prompt: &str) -> Option<Vec<String>> {
        let max_len = {
            let guard = self.inner.lock().ok()?;
            guard.config.max_prompt_len
        };
        if prompt.len() > max_len {
            return None;
        }
        let key = compute_key(model_id, prompt);
        self.inner.lock().ok()?.get(&key)
    }

    /// Insert a response for `(model_id, prompt)`.
    ///
    /// - `ttl`: when `None`, the cache's `default_ttl` is used.
    /// - Prompts longer than `max_prompt_len` are silently dropped.
    /// - If the cache is full the LRU entry is evicted first.
    pub fn insert(
        &self,
        model_id: &str,
        prompt: &str,
        response: Vec<String>,
        ttl: Option<Duration>,
    ) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if prompt.len() > guard.config.max_prompt_len {
            return;
        }
        let ttl = ttl.unwrap_or(guard.config.default_ttl);
        let key = compute_key(model_id, prompt);
        guard.insert(key, response, ttl);
    }

    /// Evict all entries whose TTL has elapsed.
    ///
    /// Returns the number of entries removed.
    pub fn evict_expired(&self) -> usize {
        self.inner
            .lock()
            .map(|mut g| g.evict_expired())
            .unwrap_or(0)
    }

    /// Return a snapshot of current cache statistics.
    pub fn stats(&self) -> CacheStats {
        self.inner
            .lock()
            .map(|g| g.stats())
            .unwrap_or_default()
    }

    /// Remove all entries from the cache.
    pub fn flush(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.flush();
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_cache(max: usize) -> PromptCache {
        PromptCache::new(CacheConfig {
            max_entries: max,
            default_ttl: Duration::from_secs(60),
            max_prompt_len: 1024,
        })
    }

    #[test]
    fn test_basic_insert_and_get() {
        let cache = make_cache(10);
        cache.insert("m1", "hello", vec!["world".to_string()], None);
        let result = cache.get("m1", "hello");
        assert_eq!(result, Some(vec!["world".to_string()]));
    }

    #[test]
    fn test_miss_returns_none() {
        let cache = make_cache(10);
        assert!(cache.get("m1", "missing").is_none());
    }

    #[test]
    fn test_different_models_different_keys() {
        let cache = make_cache(10);
        cache.insert("m1", "p", vec!["r1".to_string()], None);
        cache.insert("m2", "p", vec!["r2".to_string()], None);
        assert_eq!(cache.get("m1", "p"), Some(vec!["r1".to_string()]));
        assert_eq!(cache.get("m2", "p"), Some(vec!["r2".to_string()]));
    }

    #[test]
    fn test_hit_counter_increments() {
        let cache = make_cache(10);
        cache.insert("m", "p", vec!["r".to_string()], None);
        cache.get("m", "p");
        cache.get("m", "p");
        let stats = cache.stats();
        assert_eq!(stats.total_hits, 2);
        assert_eq!(stats.total_misses, 0);
    }

    #[test]
    fn test_miss_counter_increments() {
        let cache = make_cache(10);
        cache.get("m", "not_there");
        let stats = cache.stats();
        assert_eq!(stats.total_misses, 1);
        assert_eq!(stats.total_hits, 0);
    }

    #[test]
    fn test_hit_rate_calculation() {
        let cache = make_cache(10);
        cache.insert("m", "p", vec![], None);
        cache.get("m", "p"); // hit
        cache.get("m", "miss"); // miss
        let stats = cache.stats();
        assert!((stats.hit_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_lru_eviction_when_full() {
        let cache = make_cache(3);
        cache.insert("m", "a", vec!["a".to_string()], None);
        cache.insert("m", "b", vec!["b".to_string()], None);
        cache.insert("m", "c", vec!["c".to_string()], None);
        // Touch "a" so it becomes most recently used.
        cache.get("m", "a");
        // Insert "d" — "b" should be evicted (LRU).
        cache.insert("m", "d", vec!["d".to_string()], None);
        assert!(cache.get("m", "b").is_none()); // evicted
        assert!(cache.get("m", "a").is_some());
        assert!(cache.get("m", "c").is_some());
        assert!(cache.get("m", "d").is_some());
    }

    #[test]
    fn test_eviction_counter() {
        let cache = make_cache(2);
        cache.insert("m", "a", vec![], None);
        cache.insert("m", "b", vec![], None);
        cache.insert("m", "c", vec![], None); // evicts "a"
        let stats = cache.stats();
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn test_ttl_expiry_on_get() {
        let cache = PromptCache::new(CacheConfig {
            max_entries: 10,
            default_ttl: Duration::from_millis(1),
            max_prompt_len: 1024,
        });
        cache.insert("m", "p", vec!["r".to_string()], None);
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("m", "p").is_none());
    }

    #[test]
    fn test_evict_expired_removes_stale() {
        let cache = PromptCache::new(CacheConfig {
            max_entries: 10,
            default_ttl: Duration::from_millis(1),
            max_prompt_len: 1024,
        });
        cache.insert("m", "a", vec![], None);
        cache.insert("m", "b", vec![], None);
        std::thread::sleep(Duration::from_millis(5));
        let removed = cache.evict_expired();
        assert_eq!(removed, 2);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn test_prompt_too_long_not_cached() {
        let cache = PromptCache::new(CacheConfig {
            max_entries: 10,
            default_ttl: Duration::from_secs(60),
            max_prompt_len: 5,
        });
        cache.insert("m", "toolongprompt", vec!["r".to_string()], None);
        assert!(cache.get("m", "toolongprompt").is_none());
    }

    #[test]
    fn test_flush_clears_all() {
        let cache = make_cache(10);
        cache.insert("m", "a", vec![], None);
        cache.insert("m", "b", vec![], None);
        cache.flush();
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn test_flush_increments_evictions() {
        let cache = make_cache(10);
        cache.insert("m", "a", vec![], None);
        cache.insert("m", "b", vec![], None);
        cache.flush();
        assert_eq!(cache.stats().evictions, 2);
    }

    #[test]
    fn test_update_existing_entry() {
        let cache = make_cache(10);
        cache.insert("m", "p", vec!["old".to_string()], None);
        cache.insert("m", "p", vec!["new".to_string()], None);
        assert_eq!(cache.get("m", "p"), Some(vec!["new".to_string()]));
        // Should not grow beyond 1 entry.
        assert_eq!(cache.stats().entries, 1);
    }

    #[test]
    fn test_custom_ttl_overrides_default() {
        let cache = PromptCache::new(CacheConfig {
            max_entries: 10,
            default_ttl: Duration::from_secs(3600), // long default
            max_prompt_len: 1024,
        });
        // Short custom TTL.
        cache.insert("m", "p", vec!["r".to_string()], Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("m", "p").is_none()); // expired via custom TTL
    }

    #[test]
    fn test_clone_shares_state() {
        let cache = make_cache(10);
        let clone = cache.clone();
        cache.insert("m", "p", vec!["r".to_string()], None);
        assert_eq!(clone.get("m", "p"), Some(vec!["r".to_string()]));
    }

    #[test]
    fn test_stats_default_zero() {
        let cache = make_cache(10);
        let stats = cache.stats();
        assert_eq!(stats.entries, 0);
        assert_eq!(stats.total_hits, 0);
        assert_eq!(stats.total_misses, 0);
        assert_eq!(stats.evictions, 0);
        assert_eq!(stats.hit_rate, 0.0);
    }

    #[test]
    fn test_compute_key_deterministic() {
        let k1 = compute_key("model", "prompt");
        let k2 = compute_key("model", "prompt");
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_compute_key_differs_by_model() {
        let k1 = compute_key("model-a", "prompt");
        let k2 = compute_key("model-b", "prompt");
        assert_ne!(k1, k2);
    }
}
