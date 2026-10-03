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
use moka::ops::compute::Op;
use moka::sync::Cache;
use moka::Expiry;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, info};
use uuid::Uuid;

/// Outcome of a [`Deduplicator::check_and_register`] call.
///
/// Three-way classification allows callers to decide whether to do the work
/// themselves, wait for a concurrent worker, or immediately reuse a cached
/// result.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
///
/// # #[tokio::main]
/// # async fn main() {
/// let dedup = Deduplicator::new(Duration::from_secs(60));
/// match dedup.check_and_register("my-key").await {
///     DeduplicationResult::New(token) => {
///         let result = "computed".to_string();
///         dedup.complete(token, result).await;
///     }
///     DeduplicationResult::InProgress => {
///         // another task is already working; wait for it
///         let _ = dedup.wait_for_result("my-key").await;
///     }
///     DeduplicationResult::Cached(result) => {
///         println!("reused: {result}");
///     }
/// }
/// # }
/// ```
#[derive(Debug, Clone)]
pub enum DeduplicationResult {
    /// New request — should be processed by the caller.
    ///
    /// The caller must eventually call [`Deduplicator::complete`] or
    /// [`Deduplicator::fail`] with the returned [`DeduplicationToken`] so that
    /// any tasks blocked in [`Deduplicator::wait_for_result`] are unblocked.
    New(DeduplicationToken),
    /// An identical request is already being processed by another task.
    ///
    /// The caller should call [`Deduplicator::wait_for_result`] to block until
    /// that task completes and then reuse its result.
    InProgress,
    /// The request was recently completed and the result is still within the
    /// cache window.  The cached response string is returned directly.
    Cached(String),
}

/// Ownership token issued when a new request is registered with the
/// [`Deduplicator`].
///
/// The holder of a `DeduplicationToken` is the *authoritative worker* for
/// that request key.  It must call either [`Deduplicator::complete`] or
/// [`Deduplicator::fail`] to resolve the pending state.
///
/// # Drop behaviour
///
/// If a token is dropped without calling `complete` or `fail`, the
/// `InProgress` entry is automatically removed from the deduplicator so that
/// subsequent callers are not permanently blocked.  A `WARN`-level log line
/// is emitted in this case.
///
/// # Cloning
///
/// Tokens are `Clone` because they are cheaply cloneable (`Arc`-backed), but
/// only the **first** clone to call `complete` or `fail` takes effect;
/// subsequent calls on other clones are no-ops.
#[derive(Debug, Clone)]
pub struct DeduplicationToken {
    /// Unique identifier for this deduplication token.
    ///
    /// Useful for structured log correlation.
    pub id: String,
    key: String,
    completed: Arc<AtomicBool>,
    /// The same channel waiters subscribed to. Held by the token so that
    /// waiters are always woken, even if the cache evicted the entry.
    waiter_tx: broadcast::Sender<String>,
    requests: Cache<String, RequestState>,
}

/// # Behavior on Drop
///
/// When a `DeduplicationToken` is dropped without calling `complete()`, the
/// following sequence occurs:
///
/// 1. **Cancellation signal**: The `Drop` impl sends a sentinel cancellation
///    string (`"\x00CANCELLED"`) over the broadcast channel before removing the
///    entry.  Any tasks already blocked in `wait_for_result()` receive this
///    value via `rx.recv()` and return `Some("\x00CANCELLED")` rather than
///    `None`.  Callers of `wait_for_result` that inspect the returned string
///    can detect cancellation by checking for this sentinel.
///
/// 2. **Entry removal**: After the cancellation broadcast the `InProgress`
///    entry is removed from the shared cache.  Any tasks that subscribe
///    *after* the removal will find no entry and `wait_for_result` will
///    return `None`.
///
/// 3. **Re-registrability**: Because the entry is removed, the *next* caller
///    to invoke `check_and_register` for the same key will receive a fresh
///    `New` token and can retry processing.
///
/// **Waiters are NOT left hanging indefinitely.**  They either receive the
/// cancellation sentinel or `None` (if they race with the removal), both of
/// which are finite outcomes that unblock the awaiting task promptly.
///
/// The sentinel value `"\x00CANCELLED"` uses a NUL prefix which cannot appear
/// in normal LLM output, making it safe to use as a reserved signal.
pub const DEDUP_CANCELLED_SENTINEL: &str = "\x00CANCELLED";

/// Returns `true` if the dedup result string represents a cancellation signal.
///
/// Callers that receive a result from [`Deduplicator::wait_for_result`] should
/// use this function instead of comparing to the raw sentinel directly, so
/// that internal implementation details remain hidden.
pub fn is_cancelled_result(result: &str) -> bool {
    result == DEDUP_CANCELLED_SENTINEL
}

impl Drop for DeduplicationToken {
    fn drop(&mut self) {
        // Only act if this is the last clone and complete() was never called.
        if Arc::strong_count(&self.completed) == 1 && !self.completed.load(Ordering::Acquire) {
            // Wake tasks already blocked in wait_for_result() at once.
            // Send errors only mean nobody is waiting.
            let _ = self.waiter_tx.send(DEDUP_CANCELLED_SENTINEL.to_string());
            remove_if_owned_by(&self.requests, &self.key, &self.id);
            tracing::warn!(
                key = %self.key,
                "DeduplicationToken dropped without complete(): cancellation sent and in-progress entry removed"
            );
        }
    }
}

/// Deduplicator state
#[derive(Debug, Clone)]
enum RequestState {
    InProgress {
        /// Id of the [`DeduplicationToken`] that owns this entry.
        owner: Arc<str>,
        waiter_tx: broadcast::Sender<String>,
    },
    Completed {
        result: Arc<str>,
    },
}

/// Remove `key` only while it is still the in-progress entry of token `owner`,
/// so a stale token can never clear a newer registration or a cached result.
fn remove_if_owned_by(requests: &Cache<String, RequestState>, key: &str, owner: &str) {
    requests
        .entry_by_ref(key)
        .and_compute_with(|current| match current.map(|e| e.into_value()) {
            Some(RequestState::InProgress { owner: o, .. }) if &*o == owner => Op::Remove,
            _ => Op::Nop,
        });
}

/// Per-entry lifetimes: a completed result lives for the dedup window; an
/// in-progress entry lives long enough for a slow model call to finish.
struct DedupExpiry {
    cache_duration: Duration,
    in_progress_ttl: Duration,
}

impl Expiry<String, RequestState> for DedupExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &RequestState,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(self.ttl_for(value))
    }

    fn expire_after_update(
        &self,
        _key: &String,
        value: &RequestState,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(self.ttl_for(value))
    }
}

impl DedupExpiry {
    fn ttl_for(&self, value: &RequestState) -> Duration {
        match value {
            RequestState::InProgress { .. } => self.in_progress_ttl,
            RequestState::Completed { .. } => self.cache_duration,
        }
    }
}

/// Default bound on tracked keys (in-progress plus cached) per [`Deduplicator`].
pub const DEFAULT_DEDUP_MAX_ENTRIES: u64 = 100_000;

/// An in-progress entry is dropped after 10x the cache window, but never
/// sooner than this, so a zero-length window still shares in-flight calls.
const MIN_IN_PROGRESS_TTL: Duration = Duration::from_secs(600);

/// In-process request deduplicator that coalesces identical concurrent
/// requests and caches recently completed results.
///
/// # How it works
///
/// 1. The caller derives a stable cache key (see [`dedup_key`]) from the
///    prompt and session.
/// 2. [`Deduplicator::check_and_register`] atomically checks the shared
///    state and returns one of three outcomes:
///    - [`DeduplicationResult::New`]: the caller is the first to see this
///      key; it receives a [`DeduplicationToken`] and must process the request.
///    - [`DeduplicationResult::InProgress`]: another task is already working;
///      the caller should call [`Deduplicator::wait_for_result`] to block.
///    - [`DeduplicationResult::Cached`]: a prior result is still within the
///      `cache_duration` TTL; the caller can return it immediately.
/// 3. On success the worker calls [`Deduplicator::complete`]; on failure it
///    calls [`Deduplicator::fail`], which removes the entry.
///
/// # Storage
///
/// Entries live in a [`moka`] concurrent cache with a per-entry expiry and a
/// size bound ([`DEFAULT_DEDUP_MAX_ENTRIES`] unless set with
/// [`Deduplicator::with_max_entries`]). Expired entries are never returned
/// and are evicted by the cache itself, so there is no background sweeper
/// task, and memory stays bounded however many distinct prompts arrive
/// within one window.
///
/// # Thread safety
///
/// `Deduplicator` is `Clone + Send + Sync`.  All clones share the same cache.
///
/// # Examples
///
/// ```no_run
/// use std::time::Duration;
/// use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
///
/// # #[tokio::main]
/// # async fn main() {
/// let dedup = Deduplicator::new(Duration::from_secs(300));
/// let key = "dedup:g:abc123";
///
/// match dedup.check_and_register(key).await {
///     DeduplicationResult::New(token) => {
///         let result = "hello".to_string();
///         dedup.complete(token, result).await;
///     }
///     DeduplicationResult::InProgress => {
///         let _ = dedup.wait_for_result(key).await;
///     }
///     DeduplicationResult::Cached(result) => println!("{result}"),
/// }
/// # }
/// ```
#[derive(Clone)]
pub struct Deduplicator {
    requests: Cache<String, RequestState>,
    cache_duration: Duration,
    /// Optional embedding store for semantic (cosine-similarity) deduplication.
    embeddings: Arc<DashMap<String, Vec<f32>>>,
    /// Minimum cosine similarity score to treat a new prompt as a duplicate.
    similarity_threshold: f32,
}

impl Deduplicator {
    /// Create a new `Deduplicator` with the given cache TTL and room for
    /// [`DEFAULT_DEDUP_MAX_ENTRIES`] keys.
    ///
    /// # Arguments
    ///
    /// * `cache_duration`: how long a completed result remains cached before
    ///   being treated as a fresh request.  Common choices: 5 minutes for
    ///   interactive use, 1 hour for batch/idempotent workloads. `0` shares
    ///   only calls that are still in flight.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::Deduplicator;
    ///
    /// let dedup = Deduplicator::new(Duration::from_secs(300));
    /// ```
    pub fn new(cache_duration: Duration) -> Self {
        Self::with_max_entries(cache_duration, DEFAULT_DEDUP_MAX_ENTRIES)
    }

    /// Like [`new`](Self::new), but holding at most `max_entries` keys. When
    /// full, the cache evicts the entries least likely to be reused.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::Deduplicator;
    ///
    /// let dedup = Deduplicator::with_max_entries(Duration::from_secs(60), 10_000);
    /// ```
    pub fn with_max_entries(cache_duration: Duration, max_entries: u64) -> Self {
        let in_progress_ttl = cache_duration.saturating_mul(10).max(MIN_IN_PROGRESS_TTL);
        let requests = Cache::builder()
            .max_capacity(max_entries)
            .expire_after(DedupExpiry {
                cache_duration,
                in_progress_ttl,
            })
            .build();
        Self {
            requests,
            cache_duration,
            embeddings: Arc::new(DashMap::new()),
            similarity_threshold: 1.0, // disabled by default: exact match only
        }
    }

    /// The window a completed result stays reusable for.
    pub fn cache_duration(&self) -> Duration {
        self.cache_duration
    }

    /// Kept for API compatibility. The cache needs no background task, so
    /// there is nothing to stop.
    pub fn signal_shutdown(&self) {}

    /// Kept for API compatibility. The cache needs no background task, so
    /// this returns at once.
    pub async fn shutdown(&self) {}

    /// Atomically check whether a request is new, in-progress, or cached, and
    /// register it as in-progress if it is new.
    ///
    /// Only one of any number of concurrent callers with the same key
    /// receives [`DeduplicationResult::New`].
    ///
    /// # Arguments
    ///
    /// * `key`: stable cache key; derive one with [`dedup_key`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::{Deduplicator, DeduplicationResult};
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let dedup = Deduplicator::new(Duration::from_secs(60));
    /// if let DeduplicationResult::New(token) = dedup.check_and_register("key").await {
    ///     dedup.complete(token, "result".to_string()).await;
    /// }
    /// # }
    /// ```
    pub async fn check_and_register(&self, key: &str) -> DeduplicationResult {
        let id = Uuid::new_v4().to_string();
        let (tx, _) = broadcast::channel(16);
        let entry = self
            .requests
            .entry_by_ref(key)
            .or_insert_with(|| RequestState::InProgress {
                owner: Arc::from(id.as_str()),
                waiter_tx: tx.clone(),
            });

        if entry.is_fresh() {
            let token = DeduplicationToken {
                id,
                key: key.to_string(),
                completed: Arc::new(AtomicBool::new(false)),
                waiter_tx: tx,
                requests: self.requests.clone(),
            };
            debug!(key = key, token_id = %token.id, "new request registered");
            return DeduplicationResult::New(token);
        }

        crate::metrics::inc_dedup_hit();
        match entry.into_value() {
            RequestState::InProgress { .. } => {
                info!(key = key, "duplicate request detected (in progress)");
                DeduplicationResult::InProgress
            }
            RequestState::Completed { result } => {
                info!(key = key, "duplicate request detected (cached)");
                DeduplicationResult::Cached(result.to_string())
            }
        }
    }

    /// Wait for an in-progress request to complete and return its result.
    ///
    /// If the request has already completed by the time this is called, the
    /// cached result is returned immediately without waiting.
    ///
    /// # Returns
    ///
    /// `Some(result)` when the pending request completes, or `None` if the
    /// key is not tracked (e.g. the worker called [`Deduplicator::fail`]).
    pub async fn wait_for_result(&self, key: &str) -> Option<String> {
        let mut rx = match self.requests.get(key)? {
            RequestState::InProgress { waiter_tx, .. } => waiter_tx.subscribe(),
            RequestState::Completed { result } => return Some(result.to_string()),
        };

        let result = rx.recv().await.ok();
        if result.is_some() {
            crate::metrics::inc_dedup_waiter_unblocked();
        }
        result
    }

    /// Mark a request as successfully completed and cache its result.
    ///
    /// Notifies all tasks currently blocked in [`Deduplicator::wait_for_result`]
    /// for the same key.  The result is retained for `cache_duration` so
    /// subsequent callers receive [`DeduplicationResult::Cached`].
    pub async fn complete(&self, token: DeduplicationToken, result: String) {
        token.completed.store(true, Ordering::Release);
        let _ = token.waiter_tx.send(result.clone());
        if self.cache_duration.is_zero() {
            // Nothing to cache: just release the key.
            remove_if_owned_by(&self.requests, &token.key, &token.id);
        } else {
            let cached = RequestState::Completed {
                result: Arc::from(result),
            };
            self.requests
                .entry_by_ref(&token.key)
                .and_compute_with(|current| match current.map(|e| e.into_value()) {
                    Some(RequestState::InProgress { owner, .. }) if *owner == *token.id => {
                        Op::Put(cached)
                    }
                    // Evicted while in flight: cache the answer anyway.
                    None => Op::Put(cached),
                    _ => Op::Nop,
                });
        }
        info!(key = token.key, token_id = %token.id, "request completed");
    }

    /// Mark a request as failed and remove it from tracking.
    ///
    /// After this call, the next [`Deduplicator::check_and_register`] for the
    /// same key will receive [`DeduplicationResult::New`] so the request can
    /// be retried.  Tasks waiting in [`Deduplicator::wait_for_result`]
    /// receive the cancellation sentinel (see [`is_cancelled_result`]).
    pub async fn fail(&self, token: DeduplicationToken) {
        remove_if_owned_by(&self.requests, &token.key, &token.id);
        debug!(key = token.key, token_id = %token.id, "request failed, removed from dedup");
    }

    /// Return a snapshot of current deduplication statistics.
    ///
    /// The counts are computed by iterating the cache in O(n). Use sparingly
    /// on hot paths; prefer Prometheus counters for high-frequency monitoring.
    pub fn stats(&self) -> DeduplicationStats {
        let mut stats = DeduplicationStats {
            total: 0,
            in_progress: 0,
            cached: 0,
        };
        for (_, state) in self.requests.iter() {
            stats.total += 1;
            match state {
                RequestState::InProgress { .. } => stats.in_progress += 1,
                RequestState::Completed { .. } => stats.cached += 1,
            }
        }
        stats
    }

    /// Clear all cached results
    pub fn clear(&self) {
        self.requests.invalidate_all();
        debug!("deduplication cache cleared");
    }

    /// Enable semantic (embedding-based) deduplication.
    ///
    /// When enabled, [`check_and_register_with_embedding`](Self::check_and_register_with_embedding)
    /// compares new embeddings against all stored embeddings using cosine similarity.
    /// Any stored embedding with similarity >= `threshold` is treated as a cache hit.
    ///
    /// # Arguments
    ///
    /// * `threshold`: cosine similarity score in `[0.0, 1.0]`.  `1.0` requires
    ///   exact vector match (default); `0.95` catches near-paraphrases.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::enhanced::Deduplicator;
    ///
    /// let dedup = Deduplicator::new(Duration::from_secs(300))
    ///     .with_semantic(0.95);
    /// ```
    pub fn with_semantic(mut self, threshold: f32) -> Self {
        self.similarity_threshold = threshold;
        self
    }

    /// Like [`check_and_register`](Self::check_and_register) but also performs
    /// a semantic similarity scan against previously registered embeddings.
    ///
    /// If `embedding` is `Some` and semantic deduplication is enabled (threshold < 1.0),
    /// all stored embeddings are scanned.  The first match whose cosine similarity
    /// meets the threshold is returned as [`DeduplicationResult::Cached`] with an
    /// empty string (the caller should use `wait_for_result` with the matched key
    /// to obtain the actual cached value).
    ///
    /// Falls back to exact-key lookup when `embedding` is `None` or the threshold
    /// equals `1.0`.
    pub async fn check_and_register_with_embedding(
        &self,
        key: &str,
        embedding: Option<Vec<f32>>,
    ) -> DeduplicationResult {
        // Semantic scan first (only when an embedding is provided and threshold < 1.0)
        if let Some(ref emb) = embedding {
            if self.similarity_threshold < 1.0 {
                for entry in self.embeddings.iter() {
                    let sim = cosine_similarity(emb, entry.value());
                    if sim >= self.similarity_threshold {
                        debug!(
                            key = key,
                            matched_key = entry.key().as_str(),
                            similarity = sim,
                            "semantic duplicate detected"
                        );
                        crate::metrics::inc_dedup_hit();
                        return DeduplicationResult::Cached(String::new());
                    }
                }
                // No semantic match: store embedding for future lookups.
                self.embeddings.insert(key.to_string(), emb.clone());
            }
        }

        self.check_and_register(key).await
    }
}

/// Compute the cosine similarity between two dense vectors.
///
/// Returns a value in `[-1.0, 1.0]`.  Returns `0.0` if either vector has zero norm
/// so that zero-length embeddings never falsely match.
///
/// # Panics
///
/// Does not panic.  Mismatched lengths are handled by iterating the shorter vector.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// A point-in-time snapshot of [`Deduplicator`] state.
///
/// Obtain via [`Deduplicator::stats`].
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

    // SHA-256, truncated to 128 bits: deterministic across restarts and
    // processes, and wide enough that two different prompts never share a
    // cached answer by accident (a 64-bit hash makes that a real risk at scale).
    let digest = Sha256::digest(buf.as_bytes());
    let hash = hex::encode(&digest[..16]);
    match session_id {
        Some(_) => format!("dedup:s:{hash}"),
        None => format!("dedup:g:{hash}"),
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
            _ => unreachable!("Expected new request"),
        }
    }

    #[tokio::test]
    async fn test_duplicate_detection() {
        let dedup = Deduplicator::new(Duration::from_secs(60));

        // First request
        let token = match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(t) => t,
            _ => unreachable!("Expected new request"),
        };

        // Second request (while first is in progress)
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::InProgress => {} // Expected
            _ => unreachable!("Expected in-progress"),
        }

        // Complete first request
        dedup.complete(token, "result".to_string()).await;

        // Third request (should get cached result)
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::Cached(result) => {
                assert_eq!(result, "result");
            }
            _ => unreachable!("Expected cached result"),
        }
    }

    #[tokio::test]
    async fn test_wait_for_result() {
        let dedup = Deduplicator::new(Duration::from_secs(60));

        // Register request
        let token = match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(t) => t,
            _ => unreachable!("Expected new request"),
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
            other => unreachable!("expected Cached, got {:?}", other),
        }

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now it should be treated as new
        match dedup.check_and_register("test-key").await {
            DeduplicationResult::New(_) => {} // expected
            other => unreachable!("expected New after expiry, got {:?}", other),
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

    #[tokio::test]
    async fn test_shutdown_does_not_hang() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        // Register and complete a request so there is some state.
        let token = match dedup.check_and_register("key").await {
            DeduplicationResult::New(t) => t,
            _ => unreachable!("expected New"),
        };
        dedup.complete(token, "result".into()).await;
        // shutdown() must return promptly (background task wakes every 60 s,
        // but the shutdown flag makes it exit on the *next* wake-up; since the
        // task is sleeping we just verify the flag is set and the handle is taken).
        tokio::time::timeout(std::time::Duration::from_secs(5), dedup.shutdown())
            .await
            .expect("shutdown() must complete within 5 s");
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

    #[test]
    fn test_dedup_key_is_128_bit_hex() {
        let key = dedup_key("hello", &HashMap::new(), None);
        let hash = key.trim_start_matches("dedup:g:");
        assert_eq!(hash.len(), 32, "key={key}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_memory_is_bounded() {
        let dedup = Deduplicator::with_max_entries(Duration::from_secs(300), 100);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        rt.block_on(async {
            for i in 0..5_000 {
                if let DeduplicationResult::New(t) = dedup.check_and_register(&format!("k{i}")).await {
                    dedup.complete(t, "x".into()).await;
                }
            }
        });
        dedup.requests.run_pending_tasks();
        assert!(
            dedup.requests.entry_count() <= 100,
            "entries={}",
            dedup.requests.entry_count()
        );
    }

    #[tokio::test]
    async fn test_zero_window_still_shares_in_flight_calls() {
        let dedup = Deduplicator::new(Duration::ZERO);
        let token = match dedup.check_and_register("k").await {
            DeduplicationResult::New(t) => t,
            other => unreachable!("expected New, got {other:?}"),
        };
        assert!(matches!(
            dedup.check_and_register("k").await,
            DeduplicationResult::InProgress
        ));
        let waiter = {
            let d = dedup.clone();
            tokio::spawn(async move { d.wait_for_result("k").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        dedup.complete(token, "answer".into()).await;
        assert_eq!(waiter.await.ok().flatten().as_deref(), Some("answer"));
        // Nothing is cached with a zero window.
        assert!(matches!(
            dedup.check_and_register("k").await,
            DeduplicationResult::New(_)
        ));
    }

    #[tokio::test]
    async fn test_dropped_token_wakes_waiters_and_frees_key() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        let token = match dedup.check_and_register("k").await {
            DeduplicationResult::New(t) => t,
            other => unreachable!("expected New, got {other:?}"),
        };
        let waiter = {
            let d = dedup.clone();
            tokio::spawn(async move { d.wait_for_result("k").await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(token);
        let got = waiter.await.ok().flatten();
        assert!(got.as_deref().map_or(true, is_cancelled_result), "got {got:?}");
        assert!(matches!(
            dedup.check_and_register("k").await,
            DeduplicationResult::New(_)
        ));
    }
}
