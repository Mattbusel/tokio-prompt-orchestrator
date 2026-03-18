//! Request Deduplication
//!
//! Prevents duplicate requests from being processed multiple times.
//! Useful for cost savings when users accidentally submit the same request.
//!
//! ## Usage
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
//! # async fn process_request() -> String { String::new() }
//! # #[tokio::main]
//! # async fn main() {
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
//!         let _result = dedup.wait_for_result("prompt_hash").await;
//!     }
//!     DeduplicationResult::Cached(result) => {
//!         // Use cached result
//!         println!("{result}");
//!     }
//! }
//! # }
//! ```

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Unique identifier for this deduplication token.
    pub id: String,
    key: String,
    completed: Arc<std::sync::atomic::AtomicBool>,
    requests: Arc<DashMap<String, RequestState>>,
}

impl Drop for DeduplicationToken {
    fn drop(&mut self) {
        // Only act if this is the last clone and complete() was never called
        if Arc::strong_count(&self.completed) == 1
            && !self.completed.load(std::sync::atomic::Ordering::Acquire)
        {
            // Remove the in-progress entry so waiters don't hang.
            // We can't notify them with a result, but at least they unblock
            // on their next check_and_register() call.
            self.requests.remove(&self.key);
            tracing::warn!(
                key = %self.key,
                "DeduplicationToken dropped without complete() — in-progress entry removed"
            );
        }
    }
}

/// Deduplicator state
#[derive(Debug, Clone)]
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
#[derive(Clone)]
pub struct Deduplicator {
    requests: Arc<DashMap<String, RequestState>>,
    cache_duration: Duration,
    /// Signals the background cleanup task to stop when set to `true`.
    shutdown: Arc<AtomicBool>,
}

impl Deduplicator {
    /// Create new deduplicator
    ///
    /// `cache_duration` - How long to cache completed requests
    pub fn new(cache_duration: Duration) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let dedup = Self {
            requests: Arc::new(DashMap::new()),
            cache_duration,
            shutdown: shutdown.clone(),
        };

        // Start cleanup task; checks `shutdown` flag each iteration so it
        // stops promptly when the last Deduplicator handle is dropped.
        let requests = dedup.requests.clone();
        let cache_duration = dedup.cache_duration;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                cleanup_expired(&requests, cache_duration);
            }
        });

        dedup
    }

    /// Check if request is duplicate and register if new.
    ///
    /// Uses DashMap's atomic `entry()` API to eliminate the TOCTOU race that
    /// would otherwise let multiple concurrent callers each see `New` for the
    /// same key.  Only one caller will ever receive `New(token)` for a given
    /// key while that key is `InProgress`.
    pub async fn check_and_register(&self, key: &str) -> DeduplicationResult {
        use dashmap::mapref::entry::Entry;

        // First, handle the already-present cases through a read-side fast path.
        // We still need the atomic entry() below for the insert path.
        if let Some(state) = self.requests.get(key) {
            return match state.value() {
                RequestState::InProgress { .. } => {
                    info!(key = key, "duplicate request detected (in progress)");
                    crate::metrics::inc_dedup_hit();
                    DeduplicationResult::InProgress
                }
                RequestState::Completed {
                    result,
                    completed_at,
                } => {
                    if completed_at.elapsed().unwrap_or_default() < self.cache_duration {
                        info!(key = key, "duplicate request detected (cached)");
                        crate::metrics::inc_dedup_hit();
                        DeduplicationResult::Cached(result.clone())
                    } else {
                        // Expired — fall through to the atomic entry path below.
                        drop(state);
                        // Remove the expired entry so the entry() call below sees it as absent.
                        self.requests.remove(key);
                        // Fall through to atomic insert.
                        return self.atomic_register_new(key);
                    }
                }
            };
        }

        // Key is absent — use entry() for an atomic check-and-insert so that
        // concurrent callers cannot both see "absent" and both get New.
        match self.requests.entry(key.to_string()) {
            Entry::Occupied(occ) => {
                // Another task raced us and inserted first.
                match occ.get() {
                    RequestState::InProgress { .. } => {
                        info!(key = key, "duplicate request detected (in progress, raced)");
                        crate::metrics::inc_dedup_hit();
                        DeduplicationResult::InProgress
                    }
                    RequestState::Completed { result, .. } => {
                        info!(key = key, "duplicate request detected (cached, raced)");
                        crate::metrics::inc_dedup_hit();
                        DeduplicationResult::Cached(result.clone())
                    }
                }
            }
            Entry::Vacant(vac) => {
                let (tx, _) = broadcast::channel(16);
                vac.insert(RequestState::InProgress {
                    started_at: SystemTime::now(),
                    waiter_tx: tx,
                });
                let token = DeduplicationToken {
                    id: Uuid::new_v4().to_string(),
                    key: key.to_string(),
                    completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    requests: Arc::clone(&self.requests),
                };
                debug!(key = key, token_id = %token.id, "new request registered");
                DeduplicationResult::New(token)
            }
        }
    }

    /// Atomically insert a new `InProgress` entry.  Called only when we have
    /// already removed an expired entry and need a fresh registration.
    fn atomic_register_new(&self, key: &str) -> DeduplicationResult {
        use dashmap::mapref::entry::Entry;
        match self.requests.entry(key.to_string()) {
            Entry::Occupied(occ) => match occ.get() {
                RequestState::InProgress { .. } => {
                    crate::metrics::inc_dedup_hit();
                    DeduplicationResult::InProgress
                }
                RequestState::Completed { result, .. } => {
                    crate::metrics::inc_dedup_hit();
                    DeduplicationResult::Cached(result.clone())
                }
            },
            Entry::Vacant(vac) => {
                let (tx, _) = broadcast::channel(16);
                vac.insert(RequestState::InProgress {
                    started_at: SystemTime::now(),
                    waiter_tx: tx,
                });
                let token = DeduplicationToken {
                    id: Uuid::new_v4().to_string(),
                    key: key.to_string(),
                    completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    requests: Arc::clone(&self.requests),
                };
                debug!(key = key, "new request registered (after expiry)");
                DeduplicationResult::New(token)
            }
        }
    }

    /// Wait for in-progress request to complete
    pub async fn wait_for_result(&self, key: &str) -> Option<String> {
        let mut rx = {
            let state = self.requests.get(key)?;
            match state.value() {
                RequestState::InProgress { waiter_tx, .. } => waiter_tx.subscribe(),
                RequestState::Completed { result, .. } => {
                    return Some(result.clone());
                }
            }
        };

        let result = rx.recv().await.ok();
        if result.is_some() {
            crate::metrics::inc_dedup_waiter_unblocked();
        }
        result
    }

    /// Mark request as completed
    pub async fn complete(&self, token: DeduplicationToken, result: String) {
        token.completed.store(true, std::sync::atomic::Ordering::Release);
        if let Some(mut entry) = self.requests.get_mut(&token.key) {
            if let RequestState::InProgress { waiter_tx, .. } = entry.value() {
                let _ = waiter_tx.send(result.clone());
            }

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

impl Drop for Deduplicator {
    fn drop(&mut self) {
        // Signal the background cleanup task to stop on its next wake-up.
        // Only the last owner sets this; clones share the same Arc.
        if Arc::strong_count(&self.shutdown) == 1 {
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }
}

fn cleanup_expired(requests: &DashMap<String, RequestState>, cache_duration: Duration) {
    let _now = SystemTime::now();
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
        debug!(
            removed = removed,
            "cleaned up expired deduplication entries"
        );
    }
}

/// Deduplication statistics
#[derive(Debug)]
pub struct DeduplicationStats {
    /// Total number of tracked requests (in-progress + cached).
    pub total: usize,
    /// Requests that are currently being processed.
    pub in_progress: usize,
    /// Completed requests whose results are still cached.
    pub cached: usize,
}

/// Generate a deduplication key scoped to a session.
///
/// Including `session_id` in the key prevents two different sessions from
/// colliding on the same cached result even when their prompts are identical.
/// Use `None` only for anonymous/global dedup where cross-session sharing is
/// intentional (e.g. read-only reference data queries).
///
/// # Example
///
/// ```
/// use std::collections::HashMap;
/// use tokio_prompt_orchestrator::enhanced::dedup_key;
///
/// let meta = HashMap::new();
/// // Same prompt, different sessions → different keys.
/// let k1 = dedup_key("hello", &meta, Some("session-alice"));
/// let k2 = dedup_key("hello", &meta, Some("session-bob"));
/// assert_ne!(k1, k2);
///
/// // Same prompt, same session → same key (deterministic).
/// let k3 = dedup_key("hello", &meta, Some("session-alice"));
/// assert_eq!(k1, k3);
///
/// // No session → global key (backward-compatible).
/// let k4 = dedup_key("hello", &meta, None);
/// assert_ne!(k1, k4);
/// ```
/// FNV-1a hash (64-bit). Deterministic across process restarts — unlike
/// `DefaultHasher` which uses a randomised seed since Rust 1.36.
fn fnv1a(bytes: &[u8]) -> u64 {
    const PRIME: u64 = 1_099_511_628_211;
    const BASIS: u64 = 14_695_981_039_346_656_037;
    bytes.iter().fold(BASIS, |acc, &b| acc.wrapping_mul(PRIME) ^ b as u64)
}

pub fn dedup_key(
    prompt: &str,
    metadata: &std::collections::HashMap<String, String>,
    session_id: Option<&str>,
) -> String {
    // Build a canonical byte sequence: optional session prefix + prompt + sorted metadata.
    let mut buf = String::new();

    if let Some(sid) = session_id {
        buf.push_str(sid);
        buf.push('\x00');
    }

    buf.push_str(prompt);

    // Include relevant metadata in key (sorted for determinism).
    let mut meta_keys: Vec<_> = metadata.keys().collect();
    meta_keys.sort();
    for key in meta_keys {
        if let Some(value) = metadata.get(key) {
            buf.push('\x00');
            buf.push_str(key);
            buf.push('=');
            buf.push_str(value);
        }
    }

    let hash = fnv1a(buf.as_bytes());
    match session_id {
        Some(_) => format!("dedup:s:{hash:x}"),
        None => format!("dedup:g:{hash:x}"),
    }
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
            DeduplicationResult::InProgress => {} // Expected
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
        let wait_task = tokio::spawn(async move { dedup_clone.wait_for_result("test-key").await });

        // Complete request
        tokio::time::sleep(Duration::from_millis(100)).await;
        dedup.complete(token, "result".to_string()).await;

        // Check waiter got result
        let result = wait_task.await.unwrap();
        assert_eq!(result, Some("result".to_string()));
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired_entries() {
        let dedup = Deduplicator::new(Duration::from_millis(50)); // very short TTL

        // Register and complete a request
        let result = dedup.check_and_register("test-key").await;
        if let DeduplicationResult::New(token) = result {
            dedup.complete(token, "done".to_string()).await;
        }

        // Verify it's cached
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::Cached(_) => {} // expected
            other => panic!("expected Cached, got {:?}", other),
        }

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now it should be treated as new
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(_) => {} // expected
            other => panic!("expected New after expiry, got {:?}", other),
        }
    }

    #[test]
    fn test_dedup_key_generation() {
        let empty = HashMap::new();

        // Deterministic: same inputs → same key.
        let key1 = dedup_key("hello", &empty, None);
        let key2 = dedup_key("hello", &empty, None);
        assert_eq!(key1, key2);

        // Different prompt → different key.
        let key3 = dedup_key("world", &empty, None);
        assert_ne!(key1, key3);

        // With metadata → different from without.
        let mut meta = HashMap::new();
        meta.insert("user".to_string(), "alice".to_string());
        let key4 = dedup_key("hello", &meta, None);
        assert_ne!(key1, key4);

        // Global keys carry the "g:" prefix.
        assert!(key1.starts_with("dedup:g:"), "key={key1}");
    }

    #[test]
    fn test_dedup_key_session_isolation() {
        let empty = HashMap::new();

        // Same prompt, different sessions → different keys.
        let k_alice = dedup_key("hello", &empty, Some("session-alice"));
        let k_bob = dedup_key("hello", &empty, Some("session-bob"));
        assert_ne!(
            k_alice, k_bob,
            "different sessions must not share a dedup key"
        );

        // Same prompt, same session → same key (deterministic).
        let k_alice2 = dedup_key("hello", &empty, Some("session-alice"));
        assert_eq!(k_alice, k_alice2);

        // Session key != global key for the same prompt.
        let k_global = dedup_key("hello", &empty, None);
        assert_ne!(k_alice, k_global);

        // Session keys carry the "s:" prefix.
        assert!(k_alice.starts_with("dedup:s:"), "key={k_alice}");
        assert!(k_global.starts_with("dedup:g:"), "key={k_global}");
    }

    #[test]
    fn test_dedup_key_session_with_metadata() {
        let mut meta = HashMap::new();
        meta.insert("model".to_string(), "gpt-4".to_string());

        // Session + metadata combination is unique.
        let k1 = dedup_key("prompt", &meta, Some("sess-1"));
        let k2 = dedup_key("prompt", &meta, Some("sess-2"));
        let k3 = dedup_key("prompt", &meta, None);
        assert_ne!(k1, k2);
        assert_ne!(k1, k3);
        assert_ne!(k2, k3);
    }
}
