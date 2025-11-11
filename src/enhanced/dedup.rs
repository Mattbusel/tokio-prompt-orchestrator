//! Request Deduplication
//!
//! Prevents duplicate requests from being processed multiple times.
//! Useful for cost savings when users accidentally submit the same request.
//!
//! ## Usage
//!
//! ```rust
//! use tokio_prompt_orchestrator::enhanced::Deduplicator;
//!
//! let dedup = Deduplicator::new(Duration::from_secs(300)); // 5 minute window
//!
//! // Check if request is duplicate
//! match dedup.check_and_register("prompt_hash").await {
//!     DeduplicationResult::New(token) => {
//!         // Process new request
//!         let result = process_request().await;
//!         dedup.complete(token, result).await;
//!     }
//!     DeduplicationResult::InProgress => {
//!         // Wait for in-progress request
//!         let result = dedup.wait_for_result("prompt_hash").await;
//!     }
//!     DeduplicationResult::Cached(result) => {
//!         // Return cached result
//!         return result;
//!     }
//! }
//! ```

use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

/// Deduplication result
#[derive(Debug, Clone)]
pub enum DeduplicationResult {
    /// New request - should be processed
    New(DeduplicationToken),
    /// Request already in progress - wait for it
    InProgress,
    /// Request recently completed - return cached result
    Cached(String),
}

/// Token for tracking request completion
#[derive(Debug, Clone)]
pub struct DeduplicationToken {
    pub id: String,
    key: String,
}

/// Deduplicator state
#[derive(Clone)]
enum RequestState {
    InProgress {
        started_at: SystemTime,
        waiter_tx: broadcast::Sender<String>,
    },
    Completed {
        result: String,
        completed_at: SystemTime,
    },
}

/// Request deduplicator
pub struct Deduplicator {
    requests: Arc<DashMap<String, RequestState>>,
    cache_duration: Duration,
}

impl Deduplicator {
    /// Create new deduplicator
    ///
    /// `cache_duration` - How long to cache completed requests
    pub fn new(cache_duration: Duration) -> Self {
        let dedup = Self {
            requests: Arc::new(DashMap::new()),
            cache_duration,
        };

        // Start cleanup task
        let requests = dedup.requests.clone();
        let cache_duration = dedup.cache_duration;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                cleanup_expired(&requests, cache_duration);
            }
        });

        dedup
    }

    /// Check if request is duplicate and register if new
    pub async fn check_and_register(&self, key: &str) -> DeduplicationResult {
        // Check if request exists
        if let Some(state) = self.requests.get(key) {
            return match state.value() {
                RequestState::InProgress { .. } => {
                    info!(key = key, "duplicate request detected (in progress)");
                    DeduplicationResult::InProgress
                }
                RequestState::Completed { result, completed_at } => {
                    if completed_at.elapsed().unwrap_or_default() < self.cache_duration {
                        info!(key = key, "duplicate request detected (cached)");
                        DeduplicationResult::Cached(result.clone())
                    } else {
                        // Expired, treat as new
                        drop(state);
                        self.register_new(key).await
                    }
                }
            };
        }

        self.register_new(key).await
    }

    async fn register_new(&self, key: &str) -> DeduplicationResult {
        let (tx, _) = broadcast::channel(16);
        
        self.requests.insert(
            key.to_string(),
            RequestState::InProgress {
                started_at: SystemTime::now(),
                waiter_tx: tx,
            },
        );

        let token = DeduplicationToken {
            id: Uuid::new_v4().to_string(),
            key: key.to_string(),
        };

        debug!(key = key, token_id = %token.id, "new request registered");
        DeduplicationResult::New(token)
    }

    /// Wait for in-progress request to complete
    pub async fn wait_for_result(&self, key: &str) -> Option<String> {
        let rx = {
            let state = self.requests.get(key)?;
            match state.value() {
                RequestState::InProgress { waiter_tx, .. } => {
                    waiter_tx.subscribe()
                }
                RequestState::Completed { result, .. } => {
                    return Some(result.clone());
                }
            }
        };

        // Wait for broadcast
        match rx.recv().await {
            Ok(result) => Some(result),
            Err(_) => None,
        }
    }

    /// Mark request as completed
    pub async fn complete(&self, token: DeduplicationToken, result: String) {
        if let Some(mut entry) = self.requests.get_mut(&token.key) {
            // Broadcast to waiters
            if let RequestState::InProgress { waiter_tx, .. } = entry.value() {
                let _ = waiter_tx.send(result.clone());
            }

            // Update to completed
            *entry = RequestState::Completed {
                result,
                completed_at: SystemTime::now(),
            };

            info!(key = token.key, token_id = %token.id, "request completed");
        }
    }

    /// Mark request as failed (remove from tracking)
    pub async fn fail(&self, token: DeduplicationToken) {
        self.requests.remove(&token.key);
        debug!(key = token.key, token_id = %token.id, "request failed, removed from dedup");
    }

    /// Get deduplication statistics
    pub fn stats(&self) -> DeduplicationStats {
        let mut stats = DeduplicationStats {
            total: self.requests.len(),
            in_progress: 0,
            cached: 0,
        };

        for entry in self.requests.iter() {
            match entry.value() {
                RequestState::InProgress { .. } => stats.in_progress += 1,
                RequestState::Completed { .. } => stats.cached += 1,
            }
        }

        stats
    }

    /// Clear all cached results
    pub fn clear(&self) {
        self.requests.clear();
        debug!("deduplication cache cleared");
    }
}

fn cleanup_expired(requests: &DashMap<String, RequestState>, cache_duration: Duration) {
    let now = SystemTime::now();
    let mut removed = 0;

    requests.retain(|_, state| {
        match state {
            RequestState::InProgress { started_at, .. } => {
                // Remove stale in-progress requests (10x cache duration)
                if started_at.elapsed().unwrap_or_default() > cache_duration * 10 {
                    removed += 1;
                    return false;
                }
            }
            RequestState::Completed { completed_at, .. } => {
                // Remove expired cached results
                if completed_at.elapsed().unwrap_or_default() > cache_duration {
                    removed += 1;
                    return false;
                }
            }
        }
        true
    });

    if removed > 0 {
        debug!(removed = removed, "cleaned up expired deduplication entries");
    }
}

/// Deduplication statistics
#[derive(Debug)]
pub struct DeduplicationStats {
    pub total: usize,
    pub in_progress: usize,
    pub cached: usize,
}

/// Generate deduplication key from prompt and metadata
pub fn dedup_key(prompt: &str, metadata: &std::collections::HashMap<String, String>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    prompt.hash(&mut hasher);
    
    // Include relevant metadata in key
    let mut meta_keys: Vec<_> = metadata.keys().collect();
    meta_keys.sort();
    for key in meta_keys {
        if let Some(value) = metadata.get(key) {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }
    }

    format!("dedup:{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_new_request() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(token) => {
                assert_eq!(token.key, "test-key");
            }
            _ => panic!("Expected new request"),
        }
    }

    #[tokio::test]
    async fn test_duplicate_detection() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        
        // First request
        let token = match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(t) => t,
            _ => panic!("Expected new request"),
        };

        // Second request (while first is in progress)
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::InProgress => {}, // Expected
            _ => panic!("Expected in-progress"),
        }

        // Complete first request
        dedup.complete(token, "result".to_string()).await;

        // Third request (should get cached result)
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::Cached(result) => {
                assert_eq!(result, "result");
            }
            _ => panic!("Expected cached result"),
        }
    }

    #[tokio::test]
    async fn test_wait_for_result() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        
        // Register request
        let token = match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(t) => t,
            _ => panic!("Expected new request"),
        };

        // Spawn task to wait
        let dedup_clone = dedup.clone();
        let wait_task = tokio::spawn(async move {
            dedup_clone.wait_for_result("test-key").await
        });

        // Complete request
        tokio::time::sleep(Duration::from_millis(100)).await;
        dedup.complete(token, "result".to_string()).await;

        // Check waiter got result
        let result = wait_task.await.unwrap();
        assert_eq!(result, Some("result".to_string()));
    }

    #[test]
    fn test_dedup_key_generation() {
        let key1 = dedup_key("hello", &HashMap::new());
        let key2 = dedup_key("hello", &HashMap::new());
        let key3 = dedup_key("world", &HashMap::new());

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);

        // With metadata
        let mut meta = HashMap::new();
        meta.insert("user".to_string(), "alice".to_string());
        let key4 = dedup_key("hello", &meta);
        
        assert_ne!(key1, key4); // Different due to metadata
    }
}
