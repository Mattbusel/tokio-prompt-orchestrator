//! Caching Layer
//!
//! Provides response caching with Redis backend (optional) or in-memory fallback.
//!
//! ## Usage
//!
//! ```no_run
//! use tokio_prompt_orchestrator::enhanced::CacheLayer;
//! # #[tokio::main]
//! # async fn main() {
//! let cache = CacheLayer::new_memory(1000); // 1000 entries
//!
//! // Check cache
//! if let Some(result) = cache.get("prompt_hash").await {
//!     println!("{result}"); // use the cached result
//! }
//!
//! // ... do inference ...
//! let result = "inference result".to_string();
//!
//! // Store result
//! cache.set("prompt_hash", result, 3600).await; // TTL: 1 hour
//! # }
//! ```

use dashmap::DashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

/// Cache entry with expiration
#[derive(Clone)]
struct CacheEntry {
    value: String,
    expires_at: SystemTime,
}

/// Cache layer supporting memory and Redis backends
#[derive(Clone)]
pub struct CacheLayer {
    backend: CacheBackend,
}

#[derive(Clone)]
enum CacheBackend {
    Memory(Arc<MemoryCache>),
    #[cfg(feature = "caching")]
    Redis(Arc<RedisCache>),
}

struct MemoryCache {
    store: DashMap<String, CacheEntry>,
    max_entries: usize,
}

#[cfg(feature = "caching")]
struct RedisCache {
    client: redis::Client,
}

impl CacheLayer {
    /// Create in-memory cache with max entries
    pub fn new_memory(max_entries: usize) -> Self {
        Self {
            backend: CacheBackend::Memory(Arc::new(MemoryCache {
                store: DashMap::new(),
                max_entries,
            })),
        }
    }

    /// Create Redis-backed cache
    #[cfg(feature = "caching")]
    pub async fn new_redis(redis_url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(redis_url)?;

        // Test connection
        let mut conn = client.get_multiplexed_async_connection().await?;
        redis::cmd("PING").query_async::<_, ()>(&mut conn).await?;

        Ok(Self {
            backend: CacheBackend::Redis(Arc::new(RedisCache { client })),
        })
    }

    /// Get cached value if exists and not expired
    pub async fn get(&self, key: &str) -> Option<String> {
        match &self.backend {
            CacheBackend::Memory(cache) => {
                if let Some(entry) = cache.store.get(key) {
                    if entry.expires_at > SystemTime::now() {
                        debug!(key = key, "cache hit (memory)");
                        return Some(entry.value.clone());
                    }
                    // Expired
                    drop(entry);
                    cache.store.remove(key);
                    debug!(key = key, "cache expired");
                }
                debug!(key = key, "cache miss (memory)");
                None
            }
            #[cfg(feature = "caching")]
            CacheBackend::Redis(cache) => match cache.get_redis(key).await {
                Ok(Some(value)) => {
                    debug!(key = key, "cache hit (redis)");
                    Some(value)
                }
                Ok(None) => {
                    debug!(key = key, "cache miss (redis)");
                    None
                }
                Err(e) => {
                    warn!(key = key, error = ?e, "redis get error");
                    None
                }
            },
        }
    }

    /// Set cached value with TTL in seconds
    pub async fn set(&self, key: impl Into<String>, value: impl Into<String>, ttl_secs: u64) {
        let key = key.into();
        let value = value.into();

        match &self.backend {
            CacheBackend::Memory(cache) => {
                // Evict if at capacity
                if cache.store.len() >= cache.max_entries {
                    // Simple FIFO eviction (remove first entry)
                    if let Some(first_key) = cache.store.iter().next().map(|e| e.key().clone()) {
                        cache.store.remove(&first_key);
                    }
                }

                cache.store.insert(
                    key.clone(),
                    CacheEntry {
                        value,
                        expires_at: SystemTime::now() + Duration::from_secs(ttl_secs),
                    },
                );
                debug!(key = key, ttl_secs = ttl_secs, "cached (memory)");
            }
            #[cfg(feature = "caching")]
            CacheBackend::Redis(cache) => {
                if let Err(e) = cache.set_redis(&key, &value, ttl_secs).await {
                    warn!(key = key, error = ?e, "redis set error");
                } else {
                    debug!(key = key, ttl_secs = ttl_secs, "cached (redis)");
                }
            }
        }
    }

    /// Delete cached value
    pub async fn delete(&self, key: &str) {
        match &self.backend {
            CacheBackend::Memory(cache) => {
                cache.store.remove(key);
                debug!(key = key, "deleted from cache (memory)");
            }
            #[cfg(feature = "caching")]
            CacheBackend::Redis(cache) => {
                if let Err(e) = cache.delete_redis(key).await {
                    warn!(key = key, error = ?e, "redis delete error");
                } else {
                    debug!(key = key, "deleted from cache (redis)");
                }
            }
        }
    }

    /// Clear all cached values
    pub async fn clear(&self) {
        match &self.backend {
            CacheBackend::Memory(cache) => {
                cache.store.clear();
                debug!("cleared memory cache");
            }
            #[cfg(feature = "caching")]
            CacheBackend::Redis(cache) => {
                if let Err(e) = cache.clear_redis().await {
                    warn!(error = ?e, "redis clear error");
                } else {
                    debug!("cleared redis cache");
                }
            }
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        match &self.backend {
            CacheBackend::Memory(cache) => CacheStats {
                entries: cache.store.len(),
                backend: "memory".to_string(),
            },
            #[cfg(feature = "caching")]
            CacheBackend::Redis(_) => CacheStats {
                entries: 0, // Would need separate dbsize call
                backend: "redis".to_string(),
            },
        }
    }
}

#[cfg(feature = "caching")]
impl RedisCache {
    async fn get_redis(&self, key: &str) -> Result<Option<String>, redis::RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        redis::cmd("GET").arg(key).query_async(&mut conn).await
    }

    async fn set_redis(
        &self,
        key: &str,
        value: &str,
        ttl_secs: u64,
    ) -> Result<(), redis::RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        redis::cmd("SETEX")
            .arg(key)
            .arg(ttl_secs)
            .arg(value)
            .query_async(&mut conn)
            .await
    }

    async fn delete_redis(&self, key: &str) -> Result<(), redis::RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        redis::cmd("DEL").arg(key).query_async(&mut conn).await
    }

    async fn clear_redis(&self) -> Result<(), redis::RedisError> {
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        redis::cmd("FLUSHDB").query_async(&mut conn).await
    }
}

/// Cache statistics
#[derive(Debug)]
pub struct CacheStats {
    /// Number of entries currently held in the cache.
    pub entries: usize,
    /// Name of the storage backend in use (`"memory"` or `"redis"`).
    pub backend: String,
}

/// Generate cache key from prompt
pub fn cache_key(prompt: &str) -> String {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    prompt.hash(&mut hasher);
    format!("prompt:{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_memory_cache() {
        let cache = CacheLayer::new_memory(10);

        // Set and get
        cache.set("key1", "value1", 3600).await;
        assert_eq!(cache.get("key1").await, Some("value1".to_string()));

        // Miss
        assert_eq!(cache.get("key2").await, None);

        // Delete
        cache.delete("key1").await;
        assert_eq!(cache.get("key1").await, None);
    }

    #[tokio::test]
    async fn test_cache_expiration() {
        let cache = CacheLayer::new_memory(10);

        // Set with 1 second TTL
        cache.set("expire", "value", 1).await;
        assert_eq!(cache.get("expire").await, Some("value".to_string()));

        // Wait for expiration
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(cache.get("expire").await, None);
    }

    #[test]
    fn test_cache_key_generation() {
        let key1 = cache_key("hello world");
        let key2 = cache_key("hello world");
        let key3 = cache_key("different");

        assert_eq!(key1, key2); // Same input = same key
        assert_ne!(key1, key3); // Different input = different key
    }
}
