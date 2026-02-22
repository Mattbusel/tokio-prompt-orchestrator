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
#![cfg(feature = "self-tune")]

//! # Snapshot Store (Task 1.6)
//!
//! ## Responsibility
//! Git-like versioning for pipeline configuration. Stores timestamped snapshots
//! of all tunable parameters, supports diffing between versions, rollback to
//! any previous snapshot, and finding the best configuration within a time window.
//!
//! ## Guarantees
//! - Thread-safe: all operations safe under concurrent access
//! - Bounded: snapshot history capped at configurable maximum
//! - Deterministic: same inputs always produce same diffs
//! - Non-blocking: all operations are O(n) in history size
//!
//! ## NOT Responsible For
//! - Applying configuration changes (that's the controller's job)
//! - Persistent storage (in-memory only; Redis tier is separate)
//! - Distributed consensus on config versions (see evolution/)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the current Unix timestamp in seconds, or 0 if the clock is unavailable.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO)
        .as_secs()
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by the snapshot store.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// An internal lock was poisoned by a panicking thread.
    #[error("snapshot store lock poisoned")]
    LockPoisoned,

    /// The requested snapshot version does not exist.
    #[error("snapshot version {0} not found")]
    SnapshotNotFound(u64),

    /// The store contains no snapshots.
    #[error("snapshot history is empty")]
    EmptyHistory,

    /// The requested version is beyond the latest known version.
    #[error("invalid version: requested {requested}, latest is {latest}")]
    InvalidVersion {
        /// The version number that was requested.
        requested: u64,
        /// The highest version number currently stored.
        latest: u64,
    },

    /// A Redis operation failed (only available with the `distributed` feature).
    #[error("redis error: {0}")]
    Redis(String),
}

// ---------------------------------------------------------------------------
// SnapshotSource
// ---------------------------------------------------------------------------

/// What triggered the creation of a configuration snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotSource {
    /// Created by a human operator or API call.
    Manual,
    /// Created by PID controller update.
    PidAdjustment,
    /// Created when an experiment winner was promoted.
    ExperimentPromotion,
    /// Created by a rollback operation.
    Rollback {
        /// The version that was active before the rollback.
        from_version: u64,
    },
    /// Created by a periodic automatic snapshot.
    Scheduled,
}

// ---------------------------------------------------------------------------
// ConfigSnapshot
// ---------------------------------------------------------------------------

/// An immutable record of pipeline configuration at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// Monotonically increasing version number.
    pub version: u64,
    /// The full set of tunable parameters at this point in time.
    pub parameters: HashMap<String, f64>,
    /// What triggered this snapshot.
    pub source: SnapshotSource,
    /// Metric values recorded at the time of this snapshot.
    pub metric_scores: HashMap<String, f64>,
    /// Unix timestamp in seconds when this snapshot was created.
    pub created_at_secs: u64,
    /// Human-readable description of why this snapshot was taken.
    pub description: String,
}

// ---------------------------------------------------------------------------
// ConfigDiff
// ---------------------------------------------------------------------------

/// A diff between two configuration snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiff {
    /// Version number of the "before" snapshot.
    pub from_version: u64,
    /// Version number of the "after" snapshot.
    pub to_version: u64,
    /// Parameter-level changes between the two versions.
    pub changes: Vec<ParamChange>,
    /// Metric-level changes between the two versions.
    pub metric_changes: Vec<MetricChange>,
}

// ---------------------------------------------------------------------------
// ParamChange
// ---------------------------------------------------------------------------

/// A single parameter change between two configuration snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamChange {
    /// The parameter name.
    pub name: String,
    /// The old value, or `None` if the parameter was newly added.
    pub old_value: Option<f64>,
    /// The new value, or `None` if the parameter was removed.
    pub new_value: Option<f64>,
    /// The difference `new - old` (0.0 if either side is `None`).
    pub delta: f64,
}

// ---------------------------------------------------------------------------
// MetricChange
// ---------------------------------------------------------------------------

/// A single metric change between two configuration snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricChange {
    /// The metric name.
    pub name: String,
    /// The old metric value.
    pub old_value: f64,
    /// The new metric value.
    pub new_value: f64,
    /// The difference `new - old`.
    pub delta: f64,
    /// Whether the delta represents an improvement (positive delta).
    pub improved: bool,
}

// ---------------------------------------------------------------------------
// StoreInner
// ---------------------------------------------------------------------------

/// Internal mutable state of the snapshot store.
struct StoreInner {
    /// All stored snapshots, ordered by insertion (oldest first).
    snapshots: Vec<ConfigSnapshot>,
    /// The next version number to assign.
    next_version: u64,
    /// Maximum number of snapshots to retain.
    max_snapshots: usize,
    /// Which metric to optimise for in [`SnapshotStore::best_in_window`].
    best_score_metric: Option<String>,
}

// ---------------------------------------------------------------------------
// SnapshotStore
// ---------------------------------------------------------------------------

/// Thread-safe, bounded store of configuration snapshots with diffing,
/// rollback, and best-in-window selection.
///
/// Cloning a `SnapshotStore` produces a handle that shares the same
/// underlying state (via `Arc<Mutex<_>>`).
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    inner: Arc<Mutex<StoreInner>>,
}

// Manual Debug for StoreInner is not needed since we derive Debug on SnapshotStore
// via a manual impl that only prints the type name.
impl std::fmt::Debug for StoreInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreInner")
            .field("snapshot_count", &self.snapshots.len())
            .field("next_version", &self.next_version)
            .field("max_snapshots", &self.max_snapshots)
            .finish()
    }
}

impl SnapshotStore {
    /// Create a new, empty snapshot store.
    ///
    /// # Arguments
    /// * `max_snapshots` — Maximum number of snapshots to retain. When exceeded,
    ///   the oldest snapshot is evicted.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(max_snapshots: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StoreInner {
                snapshots: Vec::new(),
                next_version: 0,
                max_snapshots,
                best_score_metric: None,
            })),
        }
    }

    /// Create a new snapshot store with a designated score metric for
    /// [`best_in_window`](Self::best_in_window).
    ///
    /// # Arguments
    /// * `max_snapshots` — Maximum number of snapshots to retain.
    /// * `metric` — The metric name to maximise when selecting the best snapshot.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_score_metric(max_snapshots: usize, metric: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StoreInner {
                snapshots: Vec::new(),
                next_version: 0,
                max_snapshots,
                best_score_metric: Some(metric.into()),
            })),
        }
    }

    /// Create and store a new configuration snapshot.
    ///
    /// The version number is auto-incremented. If the store is at capacity,
    /// the oldest snapshot is evicted before the new one is inserted.
    ///
    /// # Arguments
    /// * `parameters` — The full set of tunable parameters.
    /// * `metric_scores` — Metric values at the time of this snapshot.
    /// * `source` — What triggered this snapshot.
    /// * `description` — Human-readable note.
    ///
    /// # Returns
    /// The newly created [`ConfigSnapshot`].
    ///
    /// # Errors
    /// Returns [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn create_snapshot(
        &self,
        parameters: HashMap<String, f64>,
        metric_scores: HashMap<String, f64>,
        source: SnapshotSource,
        description: impl Into<String>,
    ) -> Result<ConfigSnapshot, SnapshotError> {
        let mut inner = self.inner.lock().map_err(|_| SnapshotError::LockPoisoned)?;

        let version = inner.next_version;
        inner.next_version += 1;

        let snapshot = ConfigSnapshot {
            version,
            parameters,
            source,
            metric_scores,
            created_at_secs: unix_now(),
            description: description.into(),
        };

        // Evict oldest if at capacity.
        if inner.snapshots.len() >= inner.max_snapshots && !inner.snapshots.is_empty() {
            inner.snapshots.remove(0);
        }

        inner.snapshots.push(snapshot.clone());
        Ok(snapshot)
    }

    /// Retrieve a snapshot by its version number.
    ///
    /// # Arguments
    /// * `version` — The version number to look up.
    ///
    /// # Returns
    /// A clone of the matching [`ConfigSnapshot`].
    ///
    /// # Errors
    /// - [`SnapshotError::SnapshotNotFound`] if no snapshot with that version exists.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn get(&self, version: u64) -> Result<ConfigSnapshot, SnapshotError> {
        let inner = self.inner.lock().map_err(|_| SnapshotError::LockPoisoned)?;
        inner
            .snapshots
            .iter()
            .find(|s| s.version == version)
            .cloned()
            .ok_or(SnapshotError::SnapshotNotFound(version))
    }

    /// Return the most recent snapshot.
    ///
    /// # Returns
    /// A clone of the newest [`ConfigSnapshot`].
    ///
    /// # Errors
    /// - [`SnapshotError::EmptyHistory`] if no snapshots have been recorded.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn latest(&self) -> Result<ConfigSnapshot, SnapshotError> {
        let inner = self.inner.lock().map_err(|_| SnapshotError::LockPoisoned)?;
        inner
            .snapshots
            .last()
            .cloned()
            .ok_or(SnapshotError::EmptyHistory)
    }

    /// Compute the diff between two snapshot versions.
    ///
    /// # Arguments
    /// * `from` — The "before" version number.
    /// * `to` — The "after" version number.
    ///
    /// # Returns
    /// A [`ConfigDiff`] describing all parameter and metric changes.
    ///
    /// # Errors
    /// - [`SnapshotError::SnapshotNotFound`] if either version does not exist.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn diff(&self, from: u64, to: u64) -> Result<ConfigDiff, SnapshotError> {
        let inner = self.inner.lock().map_err(|_| SnapshotError::LockPoisoned)?;

        let from_snap = inner
            .snapshots
            .iter()
            .find(|s| s.version == from)
            .ok_or(SnapshotError::SnapshotNotFound(from))?;

        let to_snap = inner
            .snapshots
            .iter()
            .find(|s| s.version == to)
            .ok_or(SnapshotError::SnapshotNotFound(to))?;

        let changes = Self::compute_param_changes(&from_snap.parameters, &to_snap.parameters);
        let metric_changes =
            Self::compute_metric_changes(&from_snap.metric_scores, &to_snap.metric_scores);

        Ok(ConfigDiff {
            from_version: from,
            to_version: to,
            changes,
            metric_changes,
        })
    }

    /// Create a new snapshot that restores the parameters from a previous version.
    ///
    /// The new snapshot has source [`SnapshotSource::Rollback`] and a new
    /// auto-incremented version number.
    ///
    /// # Arguments
    /// * `to_version` — The version whose parameters should be restored.
    ///
    /// # Returns
    /// The newly created rollback [`ConfigSnapshot`].
    ///
    /// # Errors
    /// - [`SnapshotError::SnapshotNotFound`] if the target version does not exist.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn rollback(&self, to_version: u64) -> Result<ConfigSnapshot, SnapshotError> {
        let target = self.get(to_version)?;
        let latest = self.latest()?;

        self.create_snapshot(
            target.parameters,
            target.metric_scores,
            SnapshotSource::Rollback {
                from_version: latest.version,
            },
            format!("Rollback to version {}", to_version),
        )
    }

    /// Find the snapshot with the best score metric value within a time window.
    ///
    /// If no `best_score_metric` is configured, the most recent snapshot within
    /// the window is returned.
    ///
    /// # Arguments
    /// * `window_secs` — How far back from now (in seconds) to search.
    ///
    /// # Returns
    /// The best [`ConfigSnapshot`] within the window.
    ///
    /// # Errors
    /// - [`SnapshotError::EmptyHistory`] if no snapshots exist or none fall
    ///   within the window.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn best_in_window(&self, window_secs: u64) -> Result<ConfigSnapshot, SnapshotError> {
        let inner = self.inner.lock().map_err(|_| SnapshotError::LockPoisoned)?;

        let cutoff = unix_now().saturating_sub(window_secs);

        let candidates: Vec<&ConfigSnapshot> = inner
            .snapshots
            .iter()
            .filter(|s| s.created_at_secs >= cutoff)
            .collect();

        if candidates.is_empty() {
            return Err(SnapshotError::EmptyHistory);
        }

        match &inner.best_score_metric {
            Some(metric) => {
                let mut best: Option<&ConfigSnapshot> = None;
                let mut best_val = f64::NEG_INFINITY;

                for candidate in &candidates {
                    if let Some(&val) = candidate.metric_scores.get(metric) {
                        if val > best_val {
                            best_val = val;
                            best = Some(candidate);
                        }
                    }
                }

                // If no candidate has the metric, fall back to most recent.
                best.or_else(|| candidates.last().copied())
                    .cloned()
                    .ok_or(SnapshotError::EmptyHistory)
            }
            None => {
                // No score metric configured; return most recent candidate.
                candidates
                    .last()
                    .cloned()
                    .cloned()
                    .ok_or(SnapshotError::EmptyHistory)
            }
        }
    }

    /// Return the last N snapshots, newest first.
    ///
    /// # Arguments
    /// * `n` — Maximum number of snapshots to return.
    ///
    /// # Returns
    /// A `Vec` of snapshots ordered from newest to oldest, containing at most
    /// `n` entries.
    ///
    /// # Panics
    /// This function never panics.
    pub fn history(&self, n: usize) -> Vec<ConfigSnapshot> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };

        inner.snapshots.iter().rev().take(n).cloned().collect()
    }

    /// Return the number of snapshots currently stored.
    ///
    /// # Panics
    /// This function never panics.
    pub fn version_count(&self) -> usize {
        self.inner.lock().map(|g| g.snapshots.len()).unwrap_or(0)
    }

    /// Return the version number of the most recent snapshot, or `None` if
    /// the store is empty.
    ///
    /// # Panics
    /// This function never panics.
    pub fn latest_version(&self) -> Option<u64> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.snapshots.last().map(|s| s.version))
    }

    /// Find all snapshots created by the given source type.
    ///
    /// For [`SnapshotSource::Rollback`], matching is by variant only — the
    /// inner `from_version` value is ignored.
    ///
    /// # Arguments
    /// * `source` — The source type to filter by.
    ///
    /// # Returns
    /// A `Vec` of matching snapshots (may be empty).
    ///
    /// # Panics
    /// This function never panics.
    pub fn find_by_source(&self, source: &SnapshotSource) -> Vec<ConfigSnapshot> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };

        inner
            .snapshots
            .iter()
            .filter(|s| Self::source_variant_matches(&s.source, source))
            .cloned()
            .collect()
    }

    /// Remove all snapshots from the store.
    ///
    /// # Errors
    /// Returns [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn clear(&self) -> Result<(), SnapshotError> {
        let mut inner = self.inner.lock().map_err(|_| SnapshotError::LockPoisoned)?;
        inner.snapshots.clear();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Compare two source values by discriminant only (ignoring inner data
    /// for `Rollback`).
    fn source_variant_matches(a: &SnapshotSource, b: &SnapshotSource) -> bool {
        matches!(
            (a, b),
            (SnapshotSource::Manual, SnapshotSource::Manual)
                | (SnapshotSource::PidAdjustment, SnapshotSource::PidAdjustment)
                | (
                    SnapshotSource::ExperimentPromotion,
                    SnapshotSource::ExperimentPromotion
                )
                | (
                    SnapshotSource::Rollback { .. },
                    SnapshotSource::Rollback { .. }
                )
                | (SnapshotSource::Scheduled, SnapshotSource::Scheduled)
        )
    }

    /// Compute parameter-level changes between two parameter maps.
    fn compute_param_changes(
        from: &HashMap<String, f64>,
        to: &HashMap<String, f64>,
    ) -> Vec<ParamChange> {
        let mut changes = Vec::new();

        // Parameters in `from`.
        for (name, &old_val) in from {
            match to.get(name) {
                Some(&new_val) => {
                    if (old_val - new_val).abs() > f64::EPSILON {
                        changes.push(ParamChange {
                            name: name.clone(),
                            old_value: Some(old_val),
                            new_value: Some(new_val),
                            delta: new_val - old_val,
                        });
                    }
                }
                None => {
                    changes.push(ParamChange {
                        name: name.clone(),
                        old_value: Some(old_val),
                        new_value: None,
                        delta: 0.0,
                    });
                }
            }
        }

        // Parameters only in `to` (newly added).
        for (name, &new_val) in to {
            if !from.contains_key(name) {
                changes.push(ParamChange {
                    name: name.clone(),
                    old_value: None,
                    new_value: Some(new_val),
                    delta: 0.0,
                });
            }
        }

        // Sort for deterministic output.
        changes.sort_by(|a, b| a.name.cmp(&b.name));
        changes
    }

    /// Compute metric-level changes — only for metrics present in both maps.
    fn compute_metric_changes(
        from: &HashMap<String, f64>,
        to: &HashMap<String, f64>,
    ) -> Vec<MetricChange> {
        let mut changes = Vec::new();

        for (name, &old_val) in from {
            if let Some(&new_val) = to.get(name) {
                let delta = new_val - old_val;
                changes.push(MetricChange {
                    name: name.clone(),
                    old_value: old_val,
                    new_value: new_val,
                    delta,
                    improved: delta > 0.0,
                });
            }
        }

        // Sort for deterministic output.
        changes.sort_by(|a, b| a.name.cmp(&b.name));
        changes
    }
}

// ============================================================================
// RedisSnapshotStore (distributed feature)
// ============================================================================

/// A snapshot store backed by both in-memory storage and Redis persistence.
///
/// # Responsibility
/// Wraps the in-memory [`SnapshotStore`] and mirrors snapshot data to Redis for
/// cross-node visibility and crash recovery. Operates in "degraded mode"
/// (pure in-memory) when Redis is unavailable.
///
/// # Guarantees
/// - **Best-effort persistence**: Redis writes are fire-and-forget; failures
///   are logged but never propagated to callers.
/// - **Fast reads**: the in-memory store is always consulted first.
/// - **Fallback**: if a snapshot is missing from memory (e.g. after restart),
///   the store attempts to fetch it from Redis.
/// - **Zero panics**: no `unwrap()`, `expect()`, or `panic!()` in any code path.
///
/// # NOT Responsible For
/// - Distributed locking or leader election
/// - Cache invalidation across nodes (consumers should use pub/sub)
/// - Redis cluster topology management
#[cfg(feature = "distributed")]
pub struct RedisSnapshotStore {
    /// Fast in-memory store for local reads.
    inner: SnapshotStore,
    /// Redis connection manager, if available. `None` means degraded mode.
    redis: Option<redis::aio::ConnectionManager>,
    /// The Redis URL used for connecting (stored for diagnostics).
    redis_url: String,
}

#[cfg(feature = "distributed")]
impl std::fmt::Debug for RedisSnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisSnapshotStore")
            .field("inner", &self.inner)
            .field("redis_connected", &self.redis.is_some())
            .field("redis_url", &self.redis_url)
            .finish()
    }
}

#[cfg(feature = "distributed")]
impl RedisSnapshotStore {
    /// Redis key prefix for snapshots.
    const KEY_PREFIX: &'static str = "helix:snapshot:";

    /// Create a new Redis-backed snapshot store.
    ///
    /// Attempts to connect to Redis at `redis_url`. If the connection fails,
    /// the store operates in degraded mode (in-memory only) and logs a warning.
    ///
    /// # Arguments
    /// * `max_snapshots` — Maximum number of snapshots to retain in memory.
    /// * `redis_url` — Redis connection string (e.g. `redis://127.0.0.1:6379`).
    ///
    /// # Panics
    /// This function never panics.
    pub async fn new(max_snapshots: usize, redis_url: &str) -> Self {
        let inner = SnapshotStore::new(max_snapshots);
        let redis = Self::try_connect(redis_url).await;
        Self {
            inner,
            redis,
            redis_url: redis_url.to_string(),
        }
    }

    /// Attempt to open a Redis connection manager.
    ///
    /// Returns `None` (and logs a warning) on failure.
    async fn try_connect(redis_url: &str) -> Option<redis::aio::ConnectionManager> {
        let client = match redis::Client::open(redis_url) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    redis_url = redis_url,
                    error = %e,
                    "Failed to create Redis client; operating in degraded mode"
                );
                return None;
            }
        };
        match client.get_connection_manager().await {
            Ok(mgr) => Some(mgr),
            Err(e) => {
                tracing::warn!(
                    redis_url = redis_url,
                    error = %e,
                    "Failed to connect to Redis; operating in degraded mode"
                );
                None
            }
        }
    }

    /// Return `true` if a Redis connection is available.
    pub fn is_redis_connected(&self) -> bool {
        self.redis.is_some()
    }

    /// Build the Redis key for a given snapshot version.
    fn redis_key(version: u64) -> String {
        format!("{}{}", Self::KEY_PREFIX, version)
    }

    /// Best-effort write of a snapshot to Redis. Logs a warning on failure.
    async fn persist_to_redis(&self, snapshot: &ConfigSnapshot) {
        let Some(ref conn) = self.redis else { return };
        let mut conn = conn.clone();

        let json = match serde_json::to_string(snapshot) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize snapshot for Redis");
                return;
            }
        };

        let key = Self::redis_key(snapshot.version);
        let result: Result<(), redis::RedisError> =
            redis::cmd("SET").arg(&key).arg(&json).query_async(&mut conn).await;

        if let Err(e) = result {
            tracing::warn!(
                key = key.as_str(),
                error = %e,
                "Failed to persist snapshot to Redis"
            );
        }
    }

    /// Try to load a snapshot from Redis by version.
    async fn load_from_redis(&self, version: u64) -> Result<Option<ConfigSnapshot>, SnapshotError> {
        let Some(ref conn) = self.redis else {
            return Ok(None);
        };
        let mut conn = conn.clone();

        let key = Self::redis_key(version);
        let result: Result<Option<String>, redis::RedisError> =
            redis::cmd("GET").arg(&key).query_async(&mut conn).await;

        match result {
            Ok(Some(json)) => {
                let snapshot: ConfigSnapshot = serde_json::from_str(&json)
                    .map_err(|e| SnapshotError::Redis(format!("deserialization failed: {e}")))?;
                Ok(Some(snapshot))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(key = key.as_str(), error = %e, "Redis GET failed");
                Ok(None)
            }
        }
    }

    /// Best-effort delete of a snapshot from Redis.
    async fn delete_from_redis(&self, version: u64) {
        let Some(ref conn) = self.redis else { return };
        let mut conn = conn.clone();

        let key = Self::redis_key(version);
        let result: Result<(), redis::RedisError> =
            redis::cmd("DEL").arg(&key).query_async(&mut conn).await;

        if let Err(e) = result {
            tracing::warn!(key = key.as_str(), error = %e, "Failed to delete snapshot from Redis");
        }
    }

    /// Create and store a new configuration snapshot.
    ///
    /// The snapshot is written to the in-memory store first, then persisted to
    /// Redis on a best-effort basis.
    ///
    /// # Arguments
    /// * `parameters` — The full set of tunable parameters.
    /// * `metric_scores` — Metric values at the time of this snapshot.
    /// * `source` — What triggered this snapshot.
    /// * `description` — Human-readable note.
    ///
    /// # Returns
    /// The newly created [`ConfigSnapshot`].
    ///
    /// # Errors
    /// Returns [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn create_snapshot(
        &self,
        parameters: HashMap<String, f64>,
        metric_scores: HashMap<String, f64>,
        source: SnapshotSource,
        description: impl Into<String>,
    ) -> Result<ConfigSnapshot, SnapshotError> {
        let snapshot = self.inner.create_snapshot(parameters, metric_scores, source, description)?;
        self.persist_to_redis(&snapshot).await;
        Ok(snapshot)
    }

    /// Retrieve a snapshot by its version number.
    ///
    /// Checks the in-memory store first. If not found, attempts to load from
    /// Redis as a fallback.
    ///
    /// # Arguments
    /// * `version` — The version number to look up.
    ///
    /// # Returns
    /// A clone of the matching [`ConfigSnapshot`].
    ///
    /// # Errors
    /// - [`SnapshotError::SnapshotNotFound`] if not found in memory or Redis.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    /// - [`SnapshotError::Redis`] if Redis returns corrupt data.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn get(&self, version: u64) -> Result<ConfigSnapshot, SnapshotError> {
        match self.inner.get(version) {
            Ok(snap) => Ok(snap),
            Err(SnapshotError::SnapshotNotFound(_)) => {
                // Try Redis fallback.
                match self.load_from_redis(version).await? {
                    Some(snap) => Ok(snap),
                    None => Err(SnapshotError::SnapshotNotFound(version)),
                }
            }
            Err(other) => Err(other),
        }
    }

    /// Return the most recent snapshot from the in-memory store.
    ///
    /// # Errors
    /// - [`SnapshotError::EmptyHistory`] if no snapshots have been recorded.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn latest(&self) -> Result<ConfigSnapshot, SnapshotError> {
        self.inner.latest()
    }

    /// Compute the diff between two snapshot versions.
    ///
    /// Delegates to the in-memory store.
    ///
    /// # Panics
    /// This function never panics.
    pub fn diff(&self, from: u64, to: u64) -> Result<ConfigDiff, SnapshotError> {
        self.inner.diff(from, to)
    }

    /// Rollback to a previous version.
    ///
    /// Creates a new snapshot in both the in-memory store and Redis.
    ///
    /// # Arguments
    /// * `to_version` — The version whose parameters should be restored.
    ///
    /// # Returns
    /// The newly created rollback [`ConfigSnapshot`].
    ///
    /// # Errors
    /// - [`SnapshotError::SnapshotNotFound`] if the target version does not exist.
    /// - [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub async fn rollback(&self, to_version: u64) -> Result<ConfigSnapshot, SnapshotError> {
        let snapshot = self.inner.rollback(to_version)?;
        self.persist_to_redis(&snapshot).await;
        Ok(snapshot)
    }

    /// Return the last N snapshots, newest first.
    ///
    /// # Panics
    /// This function never panics.
    pub fn history(&self, n: usize) -> Vec<ConfigSnapshot> {
        self.inner.history(n)
    }

    /// Return the number of snapshots currently stored in memory.
    ///
    /// # Panics
    /// This function never panics.
    pub fn version_count(&self) -> usize {
        self.inner.version_count()
    }

    /// Return the version number of the most recent snapshot, or `None`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn latest_version(&self) -> Option<u64> {
        self.inner.latest_version()
    }

    /// Find all snapshots created by the given source type.
    ///
    /// # Panics
    /// This function never panics.
    pub fn find_by_source(&self, source: &SnapshotSource) -> Vec<ConfigSnapshot> {
        self.inner.find_by_source(source)
    }

    /// Remove all snapshots from the in-memory store.
    ///
    /// Note: this does **not** clear Redis. Use with care.
    ///
    /// # Errors
    /// Returns [`SnapshotError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn clear(&self) -> Result<(), SnapshotError> {
        self.inner.clear()
    }

    /// Find the best snapshot in a time window.
    ///
    /// Delegates to the in-memory store.
    ///
    /// # Panics
    /// This function never panics.
    pub fn best_in_window(&self, window_secs: u64) -> Result<ConfigSnapshot, SnapshotError> {
        self.inner.best_in_window(window_secs)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Helper: build a parameter map from slice of pairs.
    fn params(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// Helper: build a metric map from slice of pairs.
    fn metrics(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    // --- Basic store lifecycle ---

    #[test]
    fn test_new_creates_empty_store() {
        let store = SnapshotStore::new(10);
        assert_eq!(store.version_count(), 0);
        assert!(store.latest_version().is_none());
    }

    #[test]
    fn test_create_snapshot_increments_version() {
        let store = SnapshotStore::new(10);
        let s0 = store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        let s1 = store
            .create_snapshot(
                params(&[("a", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();
        assert_eq!(s0.version, 0);
        assert_eq!(s1.version, 1);
    }

    #[test]
    fn test_create_snapshot_returns_snapshot() {
        let store = SnapshotStore::new(10);
        let snap = store
            .create_snapshot(
                params(&[("rate", 0.5)]),
                metrics(&[("throughput", 100.0)]),
                SnapshotSource::PidAdjustment,
                "initial",
            )
            .unwrap();
        assert_eq!(snap.version, 0);
        assert_eq!(snap.parameters["rate"], 0.5);
        assert_eq!(snap.metric_scores["throughput"], 100.0);
        assert_eq!(snap.source, SnapshotSource::PidAdjustment);
        assert_eq!(snap.description, "initial");
    }

    #[test]
    fn test_get_existing_version() {
        let store = SnapshotStore::new(10);
        let snap = store
            .create_snapshot(
                params(&[("x", 42.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "test",
            )
            .unwrap();
        let found = store.get(snap.version).unwrap();
        assert_eq!(found.version, snap.version);
        assert!((found.parameters["x"] - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_get_nonexistent_version() {
        let store = SnapshotStore::new(10);
        let result = store.get(999);
        assert!(matches!(result, Err(SnapshotError::SnapshotNotFound(999))));
    }

    #[test]
    fn test_latest_returns_most_recent() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("v", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "first",
            )
            .unwrap();
        let last = store
            .create_snapshot(
                params(&[("v", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "last",
            )
            .unwrap();
        let latest = store.latest().unwrap();
        assert_eq!(latest.version, last.version);
    }

    #[test]
    fn test_latest_empty_history_error() {
        let store = SnapshotStore::new(10);
        let result = store.latest();
        assert!(matches!(result, Err(SnapshotError::EmptyHistory)));
    }

    // --- Diff ---

    #[test]
    fn test_diff_shows_changes() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("alpha", 1.0), ("beta", 2.0)]),
                metrics(&[("throughput", 100.0)]),
                SnapshotSource::Manual,
                "base",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("alpha", 3.0), ("beta", 2.0)]),
                metrics(&[("throughput", 200.0)]),
                SnapshotSource::PidAdjustment,
                "changed alpha",
            )
            .unwrap();

        let diff = store.diff(0, 1).unwrap();
        assert_eq!(diff.from_version, 0);
        assert_eq!(diff.to_version, 1);
        assert_eq!(diff.changes.len(), 1);
        assert_eq!(diff.changes[0].name, "alpha");
        assert_eq!(diff.changes[0].old_value, Some(1.0));
        assert_eq!(diff.changes[0].new_value, Some(3.0));
        assert!((diff.changes[0].delta - 2.0).abs() < f64::EPSILON);

        assert_eq!(diff.metric_changes.len(), 1);
        assert_eq!(diff.metric_changes[0].name, "throughput");
        assert!(diff.metric_changes[0].improved);
    }

    #[test]
    fn test_diff_shows_additions() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 1.0), ("b", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        let diff = store.diff(0, 1).unwrap();
        let addition = diff.changes.iter().find(|c| c.name == "b").unwrap();
        assert_eq!(addition.old_value, None);
        assert_eq!(addition.new_value, Some(2.0));
        assert!((addition.delta - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_diff_shows_removals() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("a", 1.0), ("b", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        let diff = store.diff(0, 1).unwrap();
        let removal = diff.changes.iter().find(|c| c.name == "b").unwrap();
        assert_eq!(removal.old_value, Some(2.0));
        assert_eq!(removal.new_value, None);
        assert!((removal.delta - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_diff_nonexistent_version() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();

        let result = store.diff(0, 999);
        assert!(matches!(result, Err(SnapshotError::SnapshotNotFound(999))));

        let result = store.diff(999, 0);
        assert!(matches!(result, Err(SnapshotError::SnapshotNotFound(999))));
    }

    // --- Rollback ---

    #[test]
    fn test_rollback_creates_new_snapshot() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("rate", 0.1)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("rate", 0.9)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        let rollback_snap = store.rollback(0).unwrap();
        assert_eq!(rollback_snap.version, 2);
        assert_eq!(store.version_count(), 3);
    }

    #[test]
    fn test_rollback_preserves_parameters() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("rate", 0.1), ("depth", 5.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("rate", 0.9), ("depth", 10.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        let rollback_snap = store.rollback(0).unwrap();
        assert!((rollback_snap.parameters["rate"] - 0.1).abs() < f64::EPSILON);
        assert!((rollback_snap.parameters["depth"] - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rollback_source_is_rollback() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("x", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("x", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        let rollback_snap = store.rollback(0).unwrap();
        assert!(matches!(
            rollback_snap.source,
            SnapshotSource::Rollback { from_version: 1 }
        ));
    }

    // --- best_in_window ---

    #[test]
    fn test_best_in_window_returns_best_score() {
        let store = SnapshotStore::with_score_metric(10, "throughput");
        store
            .create_snapshot(
                params(&[("v", 1.0)]),
                metrics(&[("throughput", 100.0)]),
                SnapshotSource::Manual,
                "low",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("v", 2.0)]),
                metrics(&[("throughput", 300.0)]),
                SnapshotSource::Manual,
                "high",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("v", 3.0)]),
                metrics(&[("throughput", 200.0)]),
                SnapshotSource::Manual,
                "mid",
            )
            .unwrap();

        let best = store.best_in_window(3600).unwrap();
        assert_eq!(best.version, 1); // the one with throughput=300
    }

    #[test]
    fn test_best_in_window_no_metric_returns_latest() {
        let store = SnapshotStore::new(10); // no score metric configured
        store
            .create_snapshot(
                params(&[("v", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "old",
            )
            .unwrap();
        let newest = store
            .create_snapshot(
                params(&[("v", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "new",
            )
            .unwrap();

        let best = store.best_in_window(3600).unwrap();
        assert_eq!(best.version, newest.version);
    }

    #[test]
    fn test_best_in_window_empty_error() {
        let store = SnapshotStore::new(10);
        let result = store.best_in_window(3600);
        assert!(matches!(result, Err(SnapshotError::EmptyHistory)));
    }

    // --- History ---

    #[test]
    fn test_history_newest_first() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("v", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "a",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("v", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "b",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("v", 3.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "c",
            )
            .unwrap();

        let history = store.history(10);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].version, 2);
        assert_eq!(history[1].version, 1);
        assert_eq!(history[2].version, 0);
    }

    #[test]
    fn test_history_limits_count() {
        let store = SnapshotStore::new(10);
        for i in 0..5 {
            store
                .create_snapshot(
                    params(&[("v", i as f64)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    format!("snap {i}"),
                )
                .unwrap();
        }
        let history = store.history(3);
        assert_eq!(history.len(), 3);
    }

    // --- version_count / latest_version ---

    #[test]
    fn test_version_count() {
        let store = SnapshotStore::new(10);
        assert_eq!(store.version_count(), 0);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        assert_eq!(store.version_count(), 1);
        store
            .create_snapshot(
                params(&[("a", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();
        assert_eq!(store.version_count(), 2);
    }

    #[test]
    fn test_latest_version() {
        let store = SnapshotStore::new(10);
        assert_eq!(store.latest_version(), None);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        assert_eq!(store.latest_version(), Some(0));
        store
            .create_snapshot(
                params(&[("a", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();
        assert_eq!(store.latest_version(), Some(1));
    }

    // --- find_by_source ---

    #[test]
    fn test_find_by_source() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "m1",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 2.0)]),
                HashMap::new(),
                SnapshotSource::PidAdjustment,
                "pid",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 3.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "m2",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 4.0)]),
                HashMap::new(),
                SnapshotSource::Rollback { from_version: 2 },
                "rb",
            )
            .unwrap();

        let manuals = store.find_by_source(&SnapshotSource::Manual);
        assert_eq!(manuals.len(), 2);

        let pids = store.find_by_source(&SnapshotSource::PidAdjustment);
        assert_eq!(pids.len(), 1);

        // Match by variant only — inner value is ignored.
        let rollbacks = store.find_by_source(&SnapshotSource::Rollback { from_version: 0 });
        assert_eq!(rollbacks.len(), 1);
        assert_eq!(rollbacks[0].version, 3);

        let scheduled = store.find_by_source(&SnapshotSource::Scheduled);
        assert!(scheduled.is_empty());
    }

    // --- clear ---

    #[test]
    fn test_clear_empties_store() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 2.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        assert_eq!(store.version_count(), 2);
        store.clear().unwrap();
        assert_eq!(store.version_count(), 0);
        assert!(store.latest().is_err());
    }

    // --- Eviction ---

    #[test]
    fn test_max_snapshots_evicts_oldest() {
        let store = SnapshotStore::new(3);
        for i in 0..5 {
            store
                .create_snapshot(
                    params(&[("v", i as f64)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    format!("snap {i}"),
                )
                .unwrap();
        }

        assert_eq!(store.version_count(), 3);

        // Oldest two (versions 0 and 1) should be evicted.
        assert!(store.get(0).is_err());
        assert!(store.get(1).is_err());
        assert!(store.get(2).is_ok());
        assert!(store.get(3).is_ok());
        assert!(store.get(4).is_ok());
    }

    // --- Clone shares state ---

    #[test]
    fn test_clone_shares_state() {
        let store = SnapshotStore::new(10);
        let store2 = store.clone();

        store
            .create_snapshot(
                params(&[("x", 1.0)]),
                HashMap::new(),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();

        // The clone should see the snapshot created via the original handle.
        assert_eq!(store2.version_count(), 1);
        let snap = store2.get(0).unwrap();
        assert!((snap.parameters["x"] - 1.0).abs() < f64::EPSILON);
    }

    // --- Serialization ---

    #[test]
    fn test_snapshot_serialization() {
        let store = SnapshotStore::new(10);
        let snap = store
            .create_snapshot(
                params(&[("rate", 0.5)]),
                metrics(&[("throughput", 200.0)]),
                SnapshotSource::ExperimentPromotion,
                "serialize test",
            )
            .unwrap();

        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.is_empty());

        let deserialized: ConfigSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.version, snap.version);
        assert_eq!(deserialized.description, snap.description);
        assert_eq!(deserialized.source, snap.source);
        assert!((deserialized.parameters["rate"] - 0.5).abs() < f64::EPSILON);
        assert!((deserialized.metric_scores["throughput"] - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_diff_serialization() {
        let store = SnapshotStore::new(10);
        store
            .create_snapshot(
                params(&[("a", 1.0)]),
                metrics(&[("tp", 100.0)]),
                SnapshotSource::Manual,
                "v0",
            )
            .unwrap();
        store
            .create_snapshot(
                params(&[("a", 2.0)]),
                metrics(&[("tp", 200.0)]),
                SnapshotSource::Manual,
                "v1",
            )
            .unwrap();

        let diff = store.diff(0, 1).unwrap();
        let json = serde_json::to_string(&diff).unwrap();
        assert!(!json.is_empty());

        let deserialized: ConfigDiff = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.from_version, 0);
        assert_eq!(deserialized.to_version, 1);
        assert_eq!(deserialized.changes.len(), diff.changes.len());
        assert_eq!(deserialized.metric_changes.len(), diff.metric_changes.len());
    }

    // --- SnapshotSource variants ---

    #[test]
    fn test_snapshot_source_variants() {
        assert_eq!(SnapshotSource::Manual, SnapshotSource::Manual);
        assert_eq!(SnapshotSource::PidAdjustment, SnapshotSource::PidAdjustment);
        assert_eq!(
            SnapshotSource::ExperimentPromotion,
            SnapshotSource::ExperimentPromotion
        );
        assert_eq!(SnapshotSource::Scheduled, SnapshotSource::Scheduled);
        assert_eq!(
            SnapshotSource::Rollback { from_version: 1 },
            SnapshotSource::Rollback { from_version: 1 }
        );
        assert_ne!(SnapshotSource::Manual, SnapshotSource::Scheduled);
        assert_ne!(
            SnapshotSource::Rollback { from_version: 1 },
            SnapshotSource::Rollback { from_version: 2 }
        );

        // Verify all variants serialize/deserialize.
        let sources = vec![
            SnapshotSource::Manual,
            SnapshotSource::PidAdjustment,
            SnapshotSource::ExperimentPromotion,
            SnapshotSource::Rollback { from_version: 42 },
            SnapshotSource::Scheduled,
        ];
        for src in &sources {
            let json = serde_json::to_string(src).unwrap();
            let deser: SnapshotSource = serde_json::from_str(&json).unwrap();
            assert_eq!(&deser, src);
        }
    }

    // --- SnapshotError::Redis variant ---

    #[test]
    fn test_snapshot_error_redis_variant() {
        let err = SnapshotError::Redis("connection refused".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("redis error"));
        assert!(msg.contains("connection refused"));
    }

    // ========================================================================
    // RedisSnapshotStore tests (distributed feature)
    //
    // These tests exercise the degraded-mode path because no real Redis is
    // available in the test environment. The store should behave identically
    // to the in-memory SnapshotStore when Redis is unreachable.
    // ========================================================================

    #[cfg(feature = "distributed")]
    mod redis_store_tests {
        use super::*;
        use crate::self_tune::snapshot::RedisSnapshotStore;

        /// Helper: create a RedisSnapshotStore in degraded mode (bad URL).
        async fn degraded_store(max: usize) -> RedisSnapshotStore {
            // Use an invalid URL so the store falls back to in-memory only.
            RedisSnapshotStore::new(max, "redis://invalid-host-that-does-not-exist:1").await
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_creation() {
            let store = degraded_store(10).await;
            assert!(!store.is_redis_connected());
            assert_eq!(store.version_count(), 0);
            assert!(store.latest_version().is_none());
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_create_and_get() {
            let store = degraded_store(10).await;
            let snap = store
                .create_snapshot(
                    params(&[("rate", 0.5)]),
                    metrics(&[("throughput", 100.0)]),
                    SnapshotSource::Manual,
                    "first snapshot",
                )
                .await
                .unwrap();

            assert_eq!(snap.version, 0);
            assert_eq!(snap.description, "first snapshot");
            assert!((snap.parameters["rate"] - 0.5).abs() < f64::EPSILON);

            let fetched = store.get(0).await.unwrap();
            assert_eq!(fetched.version, 0);
            assert!((fetched.parameters["rate"] - 0.5).abs() < f64::EPSILON);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_falls_back_to_memory() {
            let store = degraded_store(10).await;
            // In degraded mode, get() for a missing version should fail cleanly.
            let result = store.get(999).await;
            assert!(matches!(result, Err(SnapshotError::SnapshotNotFound(999))));
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_rollback_works() {
            let store = degraded_store(10).await;
            store
                .create_snapshot(
                    params(&[("rate", 0.1), ("depth", 5.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v0",
                )
                .await
                .unwrap();
            store
                .create_snapshot(
                    params(&[("rate", 0.9), ("depth", 10.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v1",
                )
                .await
                .unwrap();

            let rollback_snap = store.rollback(0).await.unwrap();
            assert_eq!(rollback_snap.version, 2);
            assert!((rollback_snap.parameters["rate"] - 0.1).abs() < f64::EPSILON);
            assert!((rollback_snap.parameters["depth"] - 5.0).abs() < f64::EPSILON);
            assert!(matches!(
                rollback_snap.source,
                SnapshotSource::Rollback { from_version: 1 }
            ));
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_history() {
            let store = degraded_store(10).await;
            for i in 0..5 {
                store
                    .create_snapshot(
                        params(&[("v", i as f64)]),
                        HashMap::new(),
                        SnapshotSource::Manual,
                        format!("snap {i}"),
                    )
                    .await
                    .unwrap();
            }

            let history = store.history(3);
            assert_eq!(history.len(), 3);
            assert_eq!(history[0].version, 4);
            assert_eq!(history[1].version, 3);
            assert_eq!(history[2].version, 2);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_diff() {
            let store = degraded_store(10).await;
            store
                .create_snapshot(
                    params(&[("alpha", 1.0)]),
                    metrics(&[("throughput", 100.0)]),
                    SnapshotSource::Manual,
                    "base",
                )
                .await
                .unwrap();
            store
                .create_snapshot(
                    params(&[("alpha", 3.0)]),
                    metrics(&[("throughput", 200.0)]),
                    SnapshotSource::PidAdjustment,
                    "changed",
                )
                .await
                .unwrap();

            let diff = store.diff(0, 1).unwrap();
            assert_eq!(diff.from_version, 0);
            assert_eq!(diff.to_version, 1);
            assert_eq!(diff.changes.len(), 1);
            assert_eq!(diff.changes[0].name, "alpha");
            assert!((diff.changes[0].delta - 2.0).abs() < f64::EPSILON);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_version_count() {
            let store = degraded_store(10).await;
            assert_eq!(store.version_count(), 0);

            store
                .create_snapshot(
                    params(&[("a", 1.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v0",
                )
                .await
                .unwrap();
            assert_eq!(store.version_count(), 1);

            store
                .create_snapshot(
                    params(&[("a", 2.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v1",
                )
                .await
                .unwrap();
            assert_eq!(store.version_count(), 2);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_latest() {
            let store = degraded_store(10).await;
            assert!(store.latest().is_err());

            store
                .create_snapshot(
                    params(&[("a", 1.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v0",
                )
                .await
                .unwrap();
            let latest = store.latest().unwrap();
            assert_eq!(latest.version, 0);

            store
                .create_snapshot(
                    params(&[("a", 2.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v1",
                )
                .await
                .unwrap();
            let latest = store.latest().unwrap();
            assert_eq!(latest.version, 1);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_find_by_source() {
            let store = degraded_store(10).await;
            store
                .create_snapshot(
                    params(&[("a", 1.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "manual",
                )
                .await
                .unwrap();
            store
                .create_snapshot(
                    params(&[("a", 2.0)]),
                    HashMap::new(),
                    SnapshotSource::PidAdjustment,
                    "pid",
                )
                .await
                .unwrap();
            store
                .create_snapshot(
                    params(&[("a", 3.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "manual2",
                )
                .await
                .unwrap();

            let manuals = store.find_by_source(&SnapshotSource::Manual);
            assert_eq!(manuals.len(), 2);

            let pids = store.find_by_source(&SnapshotSource::PidAdjustment);
            assert_eq!(pids.len(), 1);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_clear() {
            let store = degraded_store(10).await;
            store
                .create_snapshot(
                    params(&[("a", 1.0)]),
                    HashMap::new(),
                    SnapshotSource::Manual,
                    "v0",
                )
                .await
                .unwrap();
            assert_eq!(store.version_count(), 1);

            store.clear().unwrap();
            assert_eq!(store.version_count(), 0);
            assert!(store.latest().is_err());
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_eviction() {
            let store = degraded_store(3).await;
            for i in 0..5 {
                store
                    .create_snapshot(
                        params(&[("v", i as f64)]),
                        HashMap::new(),
                        SnapshotSource::Manual,
                        format!("snap {i}"),
                    )
                    .await
                    .unwrap();
            }

            assert_eq!(store.version_count(), 3);
            // Oldest two (versions 0 and 1) should be evicted from memory.
            assert!(store.get(0).await.is_err());
            assert!(store.get(1).await.is_err());
            assert!(store.get(2).await.is_ok());
            assert!(store.get(3).await.is_ok());
            assert!(store.get(4).await.is_ok());
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_best_in_window() {
            let store = degraded_store(10).await;
            store
                .create_snapshot(
                    params(&[("v", 1.0)]),
                    metrics(&[("tp", 100.0)]),
                    SnapshotSource::Manual,
                    "low",
                )
                .await
                .unwrap();
            store
                .create_snapshot(
                    params(&[("v", 2.0)]),
                    metrics(&[("tp", 200.0)]),
                    SnapshotSource::Manual,
                    "high",
                )
                .await
                .unwrap();

            // No score metric configured, so returns latest.
            let best = store.best_in_window(3600).unwrap();
            assert_eq!(best.version, 1);
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_debug_format() {
            let store = degraded_store(10).await;
            let debug = format!("{:?}", store);
            assert!(debug.contains("RedisSnapshotStore"));
            assert!(debug.contains("redis_connected"));
        }

        #[tokio::test]
        async fn test_redis_snapshot_store_redis_key_format() {
            let key = RedisSnapshotStore::redis_key(42);
            assert_eq!(key, "helix:snapshot:42");

            let key = RedisSnapshotStore::redis_key(0);
            assert_eq!(key, "helix:snapshot:0");
        }
    }
}
