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

use moka::ops::compute::Op;
use moka::sync::Cache;
use moka::Expiry;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};
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

/// Most prompt embeddings kept for semantic matching. Each lookup compares
/// against all of them, so this bounds both memory (about 15 MB for
/// 384-dimension models) and the time a lookup takes.
pub const DEFAULT_SEMANTIC_MAX_ENTRIES: u64 = 10_000;

/// A cache hit found by meaning rather than exact text.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticMatch {
    /// Dedup key of the earlier prompt whose answer was reused.
    pub matched_key: String,
    /// Cosine similarity between the two prompts' embeddings.
    pub similarity: f32,
}

/// A prompt in the semantic index: its embedding and, when known, its text
/// (for [`same_specifics`]).
struct IndexedPrompt {
    embedding: Vec<f32>,
    text: Option<String>,
}

/// Embedder plus the threshold it is used with.
#[derive(Clone)]
struct SemanticConfig {
    embedder: Arc<dyn crate::embedding::Embedder>,
    threshold: f32,
}

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
    /// Prompt embeddings by dedup key, for semantic matching. Bounded and
    /// expiring with the answer cache, so it cannot grow without limit.
    embeddings: Cache<String, Arc<IndexedPrompt>>,
    /// Minimum cosine similarity score to treat a new prompt as a duplicate.
    similarity_threshold: f32,
    /// Set by [`Deduplicator::with_embedder`].
    semantic: Option<SemanticConfig>,
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
            embeddings: Cache::builder()
                .max_capacity(DEFAULT_SEMANTIC_MAX_ENTRIES)
                .time_to_live(cache_duration.max(Duration::from_millis(1)))
                .build(),
            similarity_threshold: 1.0, // disabled by default: exact match only
            semantic: None,
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

    /// Like [`check_and_register`](Self::check_and_register), but a new
    /// prompt whose `embedding` is close enough (see
    /// [`with_semantic`](Self::with_semantic)) to an earlier prompt that has
    /// already been answered gets that answer as
    /// [`DeduplicationResult::Cached`].
    ///
    /// Only completed answers are reused; a similar prompt that is still in
    /// flight does not count. Falls back to exact-key lookup when `embedding`
    /// is `None` or the threshold is `1.0`.
    pub async fn check_and_register_with_embedding(
        &self,
        key: &str,
        embedding: Option<Vec<f32>>,
    ) -> DeduplicationResult {
        let threshold = self.similarity_threshold;
        match embedding {
            Some(embedding) if threshold < 1.0 => {
                self.register_semantic(key, embedding, threshold).await.0
            }
            _ => self.check_and_register(key).await,
        }
    }

    /// Turn on semantic deduplication with `embedder`: a prompt that means the
    /// same as one answered within the cache window is answered from the
    /// cache, even when the wording differs.
    ///
    /// `threshold` is the minimum cosine similarity between the two prompts'
    /// embeddings, and a match must also pass [`same_specifics`] (same
    /// numbers, shared words in the same order), because embeddings alone
    /// confuse questions like "convert 10 miles to km" and "convert 10 km to
    /// miles".
    ///
    /// Measured with BGE-small-en-v1.5 (`FastEmbedder::try_default`) in
    /// `tests/semantic_dedup_tests.rs`:
    ///
    /// | Pair | Similarity | Reused at 0.93 |
    /// |---|---|---|
    /// | "What is the capital of France?" / "Which city is the capital of France?" | 0.959 | yes |
    /// | "What time zone is Tokyo in?" / "Which time zone does Tokyo use?" | 0.963 | yes |
    /// | "How many ounces are in a pound?" / "How many oz in one lb?" | 0.931 | yes |
    /// | "How do I reverse a list in Python?" / "What's the way to reverse a Python list?" | 0.985 | no (word order) |
    /// | "Convert 10 miles to kilometers" / "Convert 10 kilometers to miles" | 0.992 | no (guard) |
    /// | "Is 17 a prime number?" / "Is 21 a prime number?" | 0.845 | no |
    /// | "What is the capital of France?" / "What is the capital of Germany?" | 0.795 | no |
    ///
    /// Other models need their own threshold: measure a few of your own
    /// pairs the same way before relying on it.
    ///
    /// Use [`check_and_register_semantic`](Self::check_and_register_semantic)
    /// to dedup with it; [`check_and_register`](Self::check_and_register)
    /// stays exact-match only.
    #[must_use]
    pub fn with_embedder(
        mut self,
        embedder: Arc<dyn crate::embedding::Embedder>,
        threshold: f32,
    ) -> Self {
        self.semantic = Some(SemanticConfig {
            embedder,
            threshold: threshold.clamp(-1.0, 1.0),
        });
        self
    }

    /// `true` when [`with_embedder`](Self::with_embedder) was used.
    pub fn is_semantic(&self) -> bool {
        self.semantic.is_some()
    }

    /// Exact dedup first, then (with [`with_embedder`](Self::with_embedder))
    /// a search for an earlier, already answered prompt with the same
    /// meaning.
    ///
    /// Returns the usual [`DeduplicationResult`] plus, for a semantic hit,
    /// which prompt matched and how closely. A semantic hit is also cached
    /// under `key`, so repeating this exact prompt later is an exact hit.
    ///
    /// If the embedder fails, the error is logged and the request is treated
    /// as new: a broken embedder costs a model call, never an answer.
    pub async fn check_and_register_semantic(
        &self,
        key: &str,
        text: &str,
    ) -> (DeduplicationResult, Option<SemanticMatch>) {
        let Some(semantic) = self.semantic.clone() else {
            return (self.check_and_register(key).await, None);
        };
        // An exact hit, or an identical call in flight, needs no embedding.
        let exact = self.check_and_register(key).await;
        let DeduplicationResult::New(token) = exact else {
            return (exact, None);
        };
        let embedding = match semantic.embedder.embed(text).await {
            Ok(embedding) => embedding,
            Err(e) => {
                warn!(key = key, error = %e, "embedding failed; treating the request as new");
                return (DeduplicationResult::New(token), None);
            }
        };
        let found = self.best_completed_match(key, &embedding, Some(text), semantic.threshold);
        self.embeddings.insert(
            key.to_string(),
            Arc::new(IndexedPrompt { embedding, text: Some(text.to_string()) }),
        );
        match found {
            Some((matched, result)) => {
                crate::metrics::inc_dedup_hit();
                info!(
                    key = key,
                    matched_key = matched.matched_key.as_str(),
                    similarity = matched.similarity,
                    "semantic duplicate answered from cache"
                );
                self.complete(token, result.clone()).await;
                (DeduplicationResult::Cached(result), Some(matched))
            }
            None => (DeduplicationResult::New(token), None),
        }
    }

    /// Exact check, then a similarity search, for a caller-supplied embedding.
    async fn register_semantic(
        &self,
        key: &str,
        embedding: Vec<f32>,
        threshold: f32,
    ) -> (DeduplicationResult, Option<SemanticMatch>) {
        let exact = self.check_and_register(key).await;
        let DeduplicationResult::New(token) = exact else {
            return (exact, None);
        };
        let found = self.best_completed_match(key, &embedding, None, threshold);
        self.embeddings.insert(key.to_string(), Arc::new(IndexedPrompt { embedding, text: None }));
        match found {
            Some((matched, result)) => {
                crate::metrics::inc_dedup_hit();
                self.complete(token, result.clone()).await;
                (DeduplicationResult::Cached(result), Some(matched))
            }
            None => (DeduplicationResult::New(token), None),
        }
    }

    /// The most similar earlier prompt at or above `threshold` whose answer
    /// is still cached, with that answer.
    fn best_completed_match(
        &self,
        key: &str,
        embedding: &[f32],
        text: Option<&str>,
        threshold: f32,
    ) -> Option<(SemanticMatch, String)> {
        let mut best: Option<(SemanticMatch, String)> = None;
        for (other_key, other) in self.embeddings.iter() {
            if other_key.as_str() == key {
                continue;
            }
            let similarity = cosine_similarity(embedding, &other.embedding);
            if similarity < threshold
                || best.as_ref().is_some_and(|(b, _)| b.similarity >= similarity)
            {
                continue;
            }
            // Embeddings barely see numbers and word order ("10 miles to km"
            // vs "10 km to miles" score 0.99 with BGE-small), so a close
            // vector is not enough: the specifics must agree too.
            if let (Some(a), Some(b)) = (text, other.text.as_deref()) {
                if !same_specifics(a, b) {
                    continue;
                }
            }
            if let Some(RequestState::Completed { result }) = self.requests.get(other_key.as_str())
            {
                best = Some((
                    SemanticMatch {
                        matched_key: other_key.to_string(),
                        similarity,
                    },
                    result.to_string(),
                ));
            }
        }
        best
    }
}

/// Words that carry no specifics; ignored by [`same_specifics`].
const GUARD_STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "at", "to", "for", "from", "by", "with", "and", "or",
    "is", "are", "was", "were", "be", "been", "do", "does", "did", "can", "could", "would",
    "should", "will", "what", "which", "who", "whom", "whose", "when", "where", "why", "how",
    "i", "me", "my", "we", "our", "you", "your", "it", "its", "this", "that", "these", "those",
    "please", "tell", "give", "show", "way", "there", "s", "whats", "what's",
];

fn guard_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '.')
        .map(|t| t.trim_matches('.').to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// `true` when two prompts agree on their specifics, the things embeddings
/// are known to blur: every number is the same, in the same order, and the
/// content words they share appear in the same order.
///
/// So "Convert 10 miles to kilometers" and "Convert 10 kilometers to miles"
/// disagree (shared words in a different order), as do "Is 17 prime?" and
/// "Is 71 prime?" (different numbers), while "What is the capital of
/// France?" and "Which city is the capital of France?" agree. It errs on the
/// side of "different": a wrong "different" costs one model call, a wrong
/// "same" returns someone else's answer.
pub fn same_specifics(a: &str, b: &str) -> bool {
    let (ta, tb) = (guard_tokens(a), guard_tokens(b));
    let is_number = |t: &String| t.chars().any(|c| c.is_ascii_digit());
    let numbers_a: Vec<&String> = ta.iter().filter(|t| is_number(t)).collect();
    let numbers_b: Vec<&String> = tb.iter().filter(|t| is_number(t)).collect();
    if numbers_a != numbers_b {
        return false;
    }
    let content = |tokens: &[String]| -> Vec<String> {
        tokens
            .iter()
            .filter(|t| !is_number(t) && !GUARD_STOPWORDS.contains(&t.as_str()))
            .cloned()
            .collect()
    };
    let (ca, cb) = (content(&ta), content(&tb));
    let shared_a: Vec<&String> = ca.iter().filter(|t| cb.contains(t)).collect();
    let shared_b: Vec<&String> = cb.iter().filter(|t| ca.contains(t)).collect();
    // Repeated words make the order ambiguous; compare first occurrences.
    let first = |v: Vec<&String>| -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for t in v {
            if !seen.contains(t) {
                seen.push(t.clone());
            }
        }
        seen
    };
    first(shared_a) == first(shared_b)
}

pub use crate::embedding::cosine_similarity;

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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // ── specifics guard ─────────────────────────────────────────────────

    #[test]
    fn test_guard_rejects_swapped_conversion() {
        // BGE-small scores this pair 0.99: only the guard tells them apart.
        assert!(!same_specifics(
            "Convert 10 miles to kilometers",
            "Convert 10 kilometers to miles"
        ));
    }

    #[test]
    fn test_guard_rejects_different_numbers() {
        assert!(!same_specifics("Is 17 a prime number?", "Is 71 a prime number?"));
        assert!(!same_specifics("Is 17 a prime number?", "Is 17.5 a prime number?"));
        assert!(!same_specifics("Top 5 tips", "Top 10 tips"));
    }

    #[test]
    fn test_guard_accepts_paraphrases() {
        for (a, b) in [
            ("What is the capital of France?", "Which city is the capital of France?"),
            ("What time zone is Tokyo in?", "Which time zone does Tokyo use?"),
            ("How many ounces are in a pound?", "How many oz in one lb?"),
            (
                "Summarize this ticket: login fails after password reset",
                "Give me a summary of this ticket: login fails after password reset",
            ),
        ] {
            assert!(same_specifics(a, b), "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn test_guard_is_conservative_on_reordered_words() {
        // Same meaning, different order: refused, which costs one model call.
        assert!(!same_specifics(
            "How do I reverse a list in Python?",
            "What's the way to reverse a Python list?"
        ));
    }

    #[tokio::test]
    async fn test_semantic_guard_blocks_a_close_vector_with_different_specifics() {
        #[derive(Debug)]
        struct SameVector;
        #[async_trait::async_trait]
        impl crate::embedding::Embedder for SameVector {
            async fn embed(&self, _: &str) -> Result<Vec<f32>, crate::OrchestratorError> {
                Ok(vec![1.0, 0.0])
            }
        }
        let dedup = Deduplicator::new(Duration::from_secs(60)).with_embedder(Arc::new(SameVector), 0.9);
        let a = "Convert 10 miles to kilometers";
        if let DeduplicationResult::New(t) = dedup.check_and_register_semantic("a", a).await.0 {
            dedup.complete(t, "16.09 km".into()).await;
        }
        let b = "Convert 10 kilometers to miles";
        let (result, _) = dedup.check_and_register_semantic("b", b).await;
        assert!(matches!(result, DeduplicationResult::New(_)), "must not reuse the miles answer");
    }

    // ── semantic dedup ──────────────────────────────────────────────────

    /// Deterministic embedder: known texts map to fixed vectors, anything
    /// else to an orthogonal one. Counts calls; "boom" fails.
    #[derive(Debug, Default)]
    struct TableEmbedder {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::embedding::Embedder for TableEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::OrchestratorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(match text {
                "What is the capital of France?" => vec![1.0, 0.0, 0.0],
                "Which city is the capital of France?" => vec![0.98, 0.199, 0.0],
                "How do I bake bread?" => vec![0.0, 0.0, 1.0],
                "boom" => return Err(crate::OrchestratorError::Other("embedder down".into())),
                _ => vec![0.0, 1.0, 0.0],
            })
        }
    }

    fn key(text: &str) -> String {
        format!("test:{text}")
    }

    fn semantic(threshold: f32) -> (Deduplicator, Arc<TableEmbedder>) {
        let embedder = Arc::new(TableEmbedder::default());
        let dedup = Deduplicator::new(Duration::from_secs(60))
            .with_embedder(Arc::clone(&embedder) as Arc<dyn crate::embedding::Embedder>, threshold);
        (dedup, embedder)
    }

    async fn answer(dedup: &Deduplicator, text: &str, reply: &str) {
        match dedup.check_and_register_semantic(&key(text), text).await.0 {
            DeduplicationResult::New(token) => dedup.complete(token, reply.to_string()).await,
            other => panic!("expected a new request for {text:?}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_semantic_paraphrase_gets_the_real_cached_answer() {
        let (dedup, _) = semantic(0.95);
        answer(&dedup, "What is the capital of France?", "Paris.").await;

        let q = "Which city is the capital of France?";
        let (result, matched) = dedup.check_and_register_semantic(&key(q), q).await;
        match result {
            DeduplicationResult::Cached(text) => assert_eq!(text, "Paris."),
            other => panic!("expected a semantic hit, got {other:?}"),
        }
        let matched = matched.expect("semantic match details");
        assert_eq!(matched.matched_key, key("What is the capital of France?"));
        assert!(matched.similarity > 0.97 && matched.similarity < 1.0, "{}", matched.similarity);
    }

    #[tokio::test]
    async fn test_semantic_unrelated_prompt_is_new() {
        let (dedup, _) = semantic(0.95);
        answer(&dedup, "What is the capital of France?", "Paris.").await;
        let q = "How do I bake bread?";
        let (result, matched) = dedup.check_and_register_semantic(&key(q), q).await;
        assert!(matches!(result, DeduplicationResult::New(_)));
        assert!(matched.is_none());
    }

    #[tokio::test]
    async fn test_semantic_threshold_is_respected() {
        // The paraphrase scores about 0.98: a 0.99 threshold must not match.
        let (dedup, _) = semantic(0.99);
        answer(&dedup, "What is the capital of France?", "Paris.").await;
        let q = "Which city is the capital of France?";
        let (result, _) = dedup.check_and_register_semantic(&key(q), q).await;
        assert!(matches!(result, DeduplicationResult::New(_)));
    }

    #[tokio::test]
    async fn test_semantic_ignores_answers_still_in_flight() {
        let (dedup, _) = semantic(0.95);
        let first = "What is the capital of France?";
        let (pending, _) = dedup.check_and_register_semantic(&key(first), first).await;
        assert!(matches!(pending, DeduplicationResult::New(_)));
        // No answer yet, so a paraphrase cannot reuse one.
        let q = "Which city is the capital of France?";
        let (result, _) = dedup.check_and_register_semantic(&key(q), q).await;
        assert!(matches!(result, DeduplicationResult::New(_)));
        drop(pending);
    }

    #[tokio::test]
    async fn test_semantic_hit_is_cached_under_the_new_key() {
        let (dedup, embedder) = semantic(0.95);
        answer(&dedup, "What is the capital of France?", "Paris.").await;
        let q = "Which city is the capital of France?";
        let _ = dedup.check_and_register_semantic(&key(q), q).await;
        let calls = embedder.calls.load(Ordering::SeqCst);
        // Asking the paraphrase again is now an exact hit: no embedding call.
        let (again, matched) = dedup.check_and_register_semantic(&key(q), q).await;
        assert!(matches!(again, DeduplicationResult::Cached(ref t) if t == "Paris."));
        assert!(matched.is_none(), "an exact hit reports no semantic match");
        assert_eq!(embedder.calls.load(Ordering::SeqCst), calls);
    }

    #[tokio::test]
    async fn test_semantic_embedder_failure_costs_a_call_not_an_answer() {
        let (dedup, _) = semantic(0.95);
        let (result, matched) = dedup.check_and_register_semantic(&key("boom"), "boom").await;
        assert!(matches!(result, DeduplicationResult::New(_)));
        assert!(matched.is_none());
    }

    #[tokio::test]
    async fn test_without_embedder_semantic_check_is_exact_only() {
        let dedup = Deduplicator::new(Duration::from_secs(60));
        assert!(!dedup.is_semantic());
        answer(&dedup, "What is the capital of France?", "Paris.").await;
        let q = "Which city is the capital of France?";
        let (result, _) = dedup.check_and_register_semantic(&key(q), q).await;
        assert!(matches!(result, DeduplicationResult::New(_)));
    }

    #[tokio::test]
    async fn test_caller_supplied_embedding_returns_the_cached_answer() {
        // Regression: a semantic hit used to return Cached(""), an empty answer.
        let dedup = Deduplicator::new(Duration::from_secs(60)).with_semantic(0.95);
        match dedup
            .check_and_register_with_embedding("k1", Some(vec![1.0, 0.0]))
            .await
        {
            DeduplicationResult::New(token) => dedup.complete(token, "Paris.".to_string()).await,
            other => panic!("expected new, got {other:?}"),
        }
        match dedup
            .check_and_register_with_embedding("k2", Some(vec![0.99, 0.05]))
            .await
        {
            DeduplicationResult::Cached(text) => assert_eq!(text, "Paris."),
            other => panic!("expected the cached answer, got {other:?}"),
        }
    }

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
