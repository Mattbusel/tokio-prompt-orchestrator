//! Semantic output deduplication cache with TTL and pluggable eviction policies.

use std::collections::HashMap;

/// Key identifying a cached prompt/model/temperature combination.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// FNV-1a hash of the prompt text.
    pub prompt_hash: u64,
    /// Model identifier string.
    pub model: String,
    /// Temperature rounded to tenths and stored as `(temperature * 10) as u8`.
    pub temperature_bucket: u8,
}

/// A single cached LLM response entry.
#[derive(Debug, Clone)]
pub struct CachedOutput {
    /// The response text.
    pub response: String,
    /// Number of tokens consumed by the request.
    pub tokens_used: u64,
    /// Unix epoch milliseconds when this entry was created.
    pub created_at_ms: u64,
    /// Time-to-live in milliseconds.
    pub ttl_ms: u64,
    /// How many times this entry has been read.
    pub access_count: u64,
}

impl CachedOutput {
    /// Returns true if this entry has expired relative to `now_ms`.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.created_at_ms + self.ttl_ms
    }
}

/// Policy used to select which entry to evict when the cache is full.
#[derive(Debug, Clone)]
pub enum EvictionPolicy {
    /// Evict the entry with the smallest `access_count` (approximated by insertion order for
    /// ties) — true LRU would require timestamps, but access_count serves as a proxy here.
    LRU,
    /// Evict the entry with the lowest `access_count`.
    LFU,
    /// Evict the entry whose TTL expires soonest.
    TTLFirst,
    /// Evict a pseudo-random entry using a seeded LCG.
    Random(u64),
}

/// Statistics snapshot returned by [`OutputCache::stats`].
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Current number of entries.
    pub size: usize,
    /// Maximum number of entries before eviction.
    pub capacity: usize,
    /// Total cache hits.
    pub hits: u64,
    /// Total cache misses.
    pub misses: u64,
    /// Ratio of hits to total lookups.
    pub hit_rate: f64,
    /// `created_at_ms` of the oldest (smallest timestamp) live entry.
    pub oldest_entry_ms: Option<u64>,
}

/// FNV-1a 64-bit hash of `data`.
pub fn fnv1a_hash(data: &[u8]) -> u64 {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Build a [`CacheKey`] from a prompt string, model name, and temperature.
pub fn cache_key(prompt: &str, model: &str, temperature: f32) -> CacheKey {
    let prompt_hash = fnv1a_hash(prompt.as_bytes());
    let temperature_bucket = (temperature * 10.0).round() as u8;
    CacheKey {
        prompt_hash,
        model: model.to_string(),
        temperature_bucket,
    }
}

/// In-memory output deduplication cache.
pub struct OutputCache {
    /// Live entries.
    pub entries: HashMap<CacheKey, CachedOutput>,
    /// Maximum number of entries.
    pub capacity: usize,
    /// Eviction policy applied when `entries.len() == capacity`.
    pub policy: EvictionPolicy,
    /// Lifetime hit counter.
    pub hits: u64,
    /// Lifetime miss counter.
    pub misses: u64,
    /// Internal LCG state used by `EvictionPolicy::Random`.
    pub lcg_state: u64,
}

impl OutputCache {
    /// Create a new cache with the given capacity and eviction policy.
    pub fn new(capacity: usize, policy: EvictionPolicy) -> Self {
        let lcg_state = match &policy {
            EvictionPolicy::Random(seed) => *seed,
            _ => 6_364_136_223_846_793_005,
        };
        Self {
            entries: HashMap::new(),
            capacity,
            policy,
            hits: 0,
            misses: 0,
            lcg_state,
        }
    }

    /// Advance the internal LCG and return a pseudo-random `u64`.
    fn lcg_next(&mut self) -> u64 {
        self.lcg_state = self
            .lcg_state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.lcg_state
    }

    /// Look up `key`.  Returns `None` if the entry is absent or expired.
    /// On a hit the entry's `access_count` is incremented and the hit stat updated.
    pub fn get(&mut self, key: &CacheKey, now_ms: u64) -> Option<&str> {
        if let Some(entry) = self.entries.get_mut(key) {
            if entry.is_expired(now_ms) {
                self.misses += 1;
                return None;
            }
            entry.access_count += 1;
            self.hits += 1;
            // SAFETY: re-borrow as shared after the mutable borrow above ends.
            return Some(self.entries.get(key).unwrap().response.as_str());
        }
        self.misses += 1;
        None
    }

    /// Insert a new entry, evicting one existing entry first if at capacity.
    pub fn insert(
        &mut self,
        key: CacheKey,
        response: String,
        tokens: u64,
        ttl_ms: u64,
        now_ms: u64,
    ) {
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.evict_one(now_ms);
        }
        self.entries.insert(
            key,
            CachedOutput {
                response,
                tokens_used: tokens,
                created_at_ms: now_ms,
                ttl_ms,
                access_count: 0,
            },
        );
    }

    /// Evict one entry: expired entries take priority; otherwise apply `policy`.
    pub fn evict_one(&mut self, now_ms: u64) {
        // First try to evict an expired entry.
        let expired_key = self
            .entries
            .iter()
            .find(|(_, v)| v.is_expired(now_ms))
            .map(|(k, _)| k.clone());
        if let Some(k) = expired_key {
            self.entries.remove(&k);
            return;
        }

        if self.entries.is_empty() {
            return;
        }

        let victim = match &self.policy {
            EvictionPolicy::LRU => {
                // Proxy: evict entry with smallest access_count (least recently used approx).
                self.entries
                    .iter()
                    .min_by_key(|(_, v)| v.access_count)
                    .map(|(k, _)| k.clone())
            }
            EvictionPolicy::LFU => {
                self.entries
                    .iter()
                    .min_by_key(|(_, v)| v.access_count)
                    .map(|(k, _)| k.clone())
            }
            EvictionPolicy::TTLFirst => {
                // Evict the entry whose absolute expiry timestamp is smallest.
                self.entries
                    .iter()
                    .min_by_key(|(_, v)| v.created_at_ms + v.ttl_ms)
                    .map(|(k, _)| k.clone())
            }
            EvictionPolicy::Random(_) => {
                let rnd = self.lcg_next();
                let idx = (rnd as usize) % self.entries.len();
                self.entries.keys().nth(idx).cloned()
            }
        };

        if let Some(k) = victim {
            self.entries.remove(&k);
        }
    }

    /// Remove all expired entries and return how many were removed.
    pub fn prune_expired(&mut self, now_ms: u64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, v| !v.is_expired(now_ms));
        before - self.entries.len()
    }

    /// Ratio of hits to total lookups, or `0.0` if no lookups have been made.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Return a statistics snapshot for the current moment `now_ms`.
    pub fn stats(&self, now_ms: u64) -> CacheStats {
        let hit_rate = self.hit_rate();
        let oldest_entry_ms = self
            .entries
            .values()
            .filter(|v| !v.is_expired(now_ms))
            .map(|v| v.created_at_ms)
            .min();
        CacheStats {
            size: self.entries.len(),
            capacity: self.capacity,
            hits: self.hits,
            misses: self.misses,
            hit_rate,
            oldest_entry_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(prompt: &str) -> CacheKey {
        cache_key(prompt, "gpt-4", 0.7)
    }

    #[test]
    fn test_cache_miss() {
        let mut cache = OutputCache::new(10, EvictionPolicy::LRU);
        let k = key("hello");
        assert!(cache.get(&k, 1000).is_none());
        assert_eq!(cache.misses, 1);
        assert_eq!(cache.hits, 0);
    }

    #[test]
    fn test_cache_hit() {
        let mut cache = OutputCache::new(10, EvictionPolicy::LRU);
        let k = key("hello");
        cache.insert(k.clone(), "world".to_string(), 10, 60_000, 1000);
        let result = cache.get(&k, 2000);
        assert_eq!(result, Some("world"));
        assert_eq!(cache.hits, 1);
        assert_eq!(cache.misses, 0);
    }

    #[test]
    fn test_ttl_expiry() {
        let mut cache = OutputCache::new(10, EvictionPolicy::LRU);
        let k = key("expire me");
        // TTL of 500 ms, inserted at t=0
        cache.insert(k.clone(), "resp".to_string(), 5, 500, 0);
        // Not yet expired at t=499
        assert!(cache.get(&k, 499).is_some());
        // Expired at t=500
        assert!(cache.get(&k, 500).is_none());
        // Pruning removes expired entries
        cache.insert(k.clone(), "resp".to_string(), 5, 500, 0);
        let pruned = cache.prune_expired(600);
        assert_eq!(pruned, 1);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn test_lru_evicts_least_accessed() {
        let mut cache = OutputCache::new(2, EvictionPolicy::LRU);
        let k1 = cache_key("p1", "m", 0.0);
        let k2 = cache_key("p2", "m", 0.0);
        cache.insert(k1.clone(), "r1".to_string(), 1, 60_000, 0);
        cache.insert(k2.clone(), "r2".to_string(), 1, 60_000, 0);
        // Access k2 to give it higher access_count
        cache.get(&k2, 1);
        // Insert k3 — should evict k1 (access_count=0)
        let k3 = cache_key("p3", "m", 0.0);
        cache.insert(k3.clone(), "r3".to_string(), 1, 60_000, 1);
        assert!(!cache.entries.contains_key(&k1));
        assert!(cache.entries.contains_key(&k2));
        assert!(cache.entries.contains_key(&k3));
    }

    #[test]
    fn test_lfu_evicts_least_used() {
        let mut cache = OutputCache::new(2, EvictionPolicy::LFU);
        let k1 = cache_key("p1", "m", 0.0);
        let k2 = cache_key("p2", "m", 0.0);
        cache.insert(k1.clone(), "r1".to_string(), 1, 60_000, 0);
        cache.insert(k2.clone(), "r2".to_string(), 1, 60_000, 0);
        // Access k1 multiple times
        cache.get(&k1, 1);
        cache.get(&k1, 2);
        // Insert k3 — should evict k2 (access_count=0)
        let k3 = cache_key("p3", "m", 0.0);
        cache.insert(k3.clone(), "r3".to_string(), 1, 60_000, 3);
        assert!(cache.entries.contains_key(&k1));
        assert!(!cache.entries.contains_key(&k2));
        assert!(cache.entries.contains_key(&k3));
    }

    #[test]
    fn test_hit_rate_calculation() {
        let mut cache = OutputCache::new(10, EvictionPolicy::LRU);
        let k = key("q");
        cache.insert(k.clone(), "a".to_string(), 1, 60_000, 0);
        cache.get(&k, 1); // hit
        cache.get(&key("other"), 1); // miss
        // 1 hit / 2 total = 0.5
        let rate = cache.hit_rate();
        assert!((rate - 0.5).abs() < 1e-9);
        let stats = cache.stats(1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate - 0.5).abs() < 1e-9);
    }
}
