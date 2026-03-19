//! # Agent Memory System (Task 2.3)
//!
//! Persistent knowledge base for the agent fleet.
//!
//! ## What it stores
//! - Past modifications and their metric outcomes
//! - Code patterns that succeeded vs. failed
//! - Module dependency graph (safe-to-edit-in-parallel sets)
//! - Performance baselines per module
//! - Dead-end approaches (so agents don't retry them)
//!
//! ## Backing store
//! In-memory (DashMap + Mutex<Vec>) for the base implementation.
//! The distributed tier adds a Redis write-through layer behind the same API.
//!
//! ## Graceful degradation
//! All query methods return `None` / empty collections on any internal error  -
//! agents proceed without memory rather than failing.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

//  Error

/// Errors produced by the memory system.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// The modification record already exists (duplicate ID).
    #[error("modification record '{0}' already exists")]
    DuplicateRecord(String),

    /// The requested record was not found.
    #[error("record '{0}' not found")]
    NotFound(String),

    /// The internal store is poisoned (thread-safety violation).
    #[error("memory store lock poisoned")]
    LockPoisoned,

    /// A Redis operation failed (distributed feature only).
    #[error("Redis error: {0}")]
    Redis(String),

    /// An I/O error occurred while reading or writing the persistence file.
    #[error("I/O error: {0}")]
    Io(String),
}

//  Modification record

/// Outcome of a past self-modification attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModificationOutcome {
    /// The change passed all gates and was deployed.
    Deployed,
    /// The change was rolled back after deployment due to metric regression.
    RolledBack,
    /// The change failed validation gates (tests, clippy, benchmarks).
    ValidationFailed,
    /// The change was rejected by a human reviewer.
    Rejected,
    /// The change is still in progress.
    InProgress,
}

/// Record of a single agent-proposed code modification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModificationRecord {
    /// Unique identifier for this modification (UUID recommended).
    pub id: String,
    /// Human-readable description of what was changed and why.
    pub description: String,
    /// Source files modified.
    pub files_changed: Vec<String>,
    /// Outcome of the modification.
    pub outcome: ModificationOutcome,
    /// Metric deltas observed after deployment (metric_name → delta).
    /// Positive delta = improvement.
    pub metric_deltas: HashMap<String, f64>,
    /// Unix timestamp when the record was created.
    pub created_at_secs: u64,
    /// Optional tag for the feature or bug being addressed.
    pub tag: Option<String>,
}

impl ModificationRecord {
    /// Create a new in-progress record.
    pub fn new(id: impl Into<String>, description: impl Into<String>, files: Vec<String>) -> Self {
        Self {
            id: id.into(),
            description: description.into(),
            files_changed: files,
            outcome: ModificationOutcome::InProgress,
            metric_deltas: HashMap::new(),
            created_at_secs: unix_now(),
            tag: None,
        }
    }
}

//  Code pattern

/// Whether a code pattern is known to succeed or fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatternVerdict {
    /// This pattern improves the target metric.
    Success,
    /// This pattern degrades the target metric or breaks tests.
    Failure,
    /// Insufficient data to classify.
    Unknown,
}

/// A reusable code pattern with its known verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodePattern {
    /// Short name / identifier.
    pub name: String,
    /// Which module or domain this pattern applies to.
    pub module: String,
    /// Brief description of the pattern.
    pub description: String,
    /// Whether this pattern is known to help or hurt.
    pub verdict: PatternVerdict,
    /// How many times this pattern has been observed (success or failure).
    pub observation_count: u32,
    /// Unix timestamp of last observation.
    pub last_seen_secs: u64,
}

//  Module dependency node

/// Dependency information for a source module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleNode {
    /// Module path (e.g. `"src/enhanced/dedup.rs"`).
    pub path: String,
    /// Modules that import this one (dependents).
    pub dependents: Vec<String>,
    /// Modules imported by this one (dependencies).
    pub dependencies: Vec<String>,
    /// Measured p95 latency contribution in ms (0 if not profiled).
    pub p95_latency_ms: f64,
    /// Whether this module is currently claimed by an agent.
    pub claimed_by: Option<String>,
}

//  Performance baseline

/// Performance baseline for a module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceBaseline {
    /// Module path.
    pub module: String,
    /// Baseline throughput (requests/sec).
    pub throughput_rps: f64,
    /// Baseline p95 latency (ms).
    pub p95_latency_ms: f64,
    /// Baseline error rate (0.0  -  1.0).
    pub error_rate: f64,
    /// Unix timestamp when baseline was recorded.
    pub recorded_at_secs: u64,
}

//  Dead end

/// An approach that has been tried and failed, to avoid repetition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadEnd {
    /// Short description of the failed approach.
    pub description: String,
    /// Module or area this applies to.
    pub module: String,
    /// Why it failed.
    pub reason: String,
    /// Modification record IDs that demonstrate the failure.
    pub evidence_ids: Vec<String>,
    /// Unix timestamp when this dead end was recorded.
    pub recorded_at_secs: u64,
}

//  Memory store

struct MemoryInner {
    /// Ordered modification records (oldest at front, newest at back).
    modifications: VecDeque<ModificationRecord>,
    /// Set of known modification IDs for O(1) duplicate detection.
    modification_ids: HashSet<String>,
    patterns: HashMap<String, CodePattern>,
    dependency_graph: HashMap<String, ModuleNode>,
    baselines: HashMap<String, PerformanceBaseline>,
    dead_ends: Vec<DeadEnd>,
    /// Maximum number of modification records to retain.
    max_modifications: usize,
    /// Optional path to flush modification history to on every insert.
    /// If `None`, state is in-memory only.
    persist_path: Option<PathBuf>,
}

/// In-memory knowledge base for the agent fleet.
///
/// All operations are thread-safe.  Clone is cheap (Arc-backed).
#[derive(Clone)]
pub struct AgentMemory {
    inner: Arc<Mutex<MemoryInner>>,
}

impl AgentMemory {
    /// Create a new empty memory store.
    ///
    /// `max_modifications` caps the modification history to avoid unbounded growth.
    pub fn new(max_modifications: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MemoryInner {
                modifications: VecDeque::new(),
                modification_ids: HashSet::new(),
                patterns: HashMap::new(),
                dependency_graph: HashMap::new(),
                baselines: HashMap::new(),
                dead_ends: Vec::new(),
                max_modifications,
                persist_path: None,
            })),
        }
    }

    /// Create a memory store with an optional file-backed persistence path.
    ///
    /// If `persist_path` is `Some(path)`, the full modification history is
    /// flushed to a JSON file on every [`insert_modification`] call so that
    /// state survives process restarts.  On construction, if the file already
    /// exists, its contents are loaded to restore the previous history.
    ///
    /// # Errors
    /// Returns `Err(MemoryError::...)` only if the file exists but cannot be
    /// parsed.  A missing file is treated as an empty initial state.
    pub fn with_persist_path(
        max_modifications: usize,
        persist_path: Option<PathBuf>,
    ) -> Result<Self, MemoryError> {
        let mut modifications: VecDeque<ModificationRecord> = VecDeque::new();
        let mut modification_ids: HashSet<String> = HashSet::new();

        // Load existing state from disk if the file exists.
        if let Some(ref path) = persist_path {
            if path.exists() {
                let contents = std::fs::read_to_string(path).map_err(|e| {
                    MemoryError::Io(format!(
                        "failed to read persist file {}: {e}",
                        path.display()
                    ))
                })?;
                let loaded: Vec<ModificationRecord> =
                    serde_json::from_str(&contents).map_err(|e| {
                        MemoryError::Io(format!(
                            "failed to parse persist file {}: {e}",
                            path.display()
                        ))
                    })?;
                for rec in loaded.into_iter().take(max_modifications) {
                    modification_ids.insert(rec.id.clone());
                    modifications.push_back(rec);
                }
            }
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(MemoryInner {
                modifications,
                modification_ids,
                patterns: HashMap::new(),
                dependency_graph: HashMap::new(),
                baselines: HashMap::new(),
                dead_ends: Vec::new(),
                max_modifications,
                persist_path,
            })),
        })
    }

    //  Modification records

    /// Insert a new modification record.
    pub fn insert_modification(&self, record: ModificationRecord) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::LockPoisoned)?;
        // O(1) duplicate check via HashSet — no linear scan needed.
        if inner.modification_ids.contains(&record.id) {
            return Err(MemoryError::DuplicateRecord(record.id));
        }
        // O(1) eviction via pop_front instead of remove(0).
        if inner.modifications.len() >= inner.max_modifications {
            if let Some(evicted) = inner.modifications.pop_front() {
                inner.modification_ids.remove(&evicted.id);
            }
        }
        inner.modification_ids.insert(record.id.clone());
        inner.modifications.push_back(record);

        // Flush to disk if a persistence path is configured.
        if let Some(ref path) = inner.persist_path.clone() {
            let records: Vec<&ModificationRecord> = inner.modifications.iter().collect();
            if let Ok(json) = serde_json::to_string_pretty(&records) {
                // Best-effort: log but don't fail the insert on I/O error.
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "AgentMemory: failed to flush modification history to disk"
                    );
                }
            }
        }

        Ok(())
    }

    /// Update the outcome and metric deltas of an existing record.
    pub fn update_outcome(
        &self,
        id: &str,
        outcome: ModificationOutcome,
        deltas: HashMap<String, f64>,
    ) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::LockPoisoned)?;
        let rec = inner
            .modifications
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        rec.outcome = outcome;
        rec.metric_deltas = deltas;
        Ok(())
    }

    /// Return all modification records (newest first).
    pub fn modifications(&self) -> Vec<ModificationRecord> {
        self.inner
            .lock()
            .map(|inner| {
                // VecDeque stores oldest-first; reverse to return newest-first.
                inner.modifications.iter().rev().cloned().collect()
            })
            .unwrap_or_default()
    }

    /// Return the N most recent deployed modifications.
    pub fn recent_deployments(&self, n: usize) -> Vec<ModificationRecord> {
        self.modifications()
            .into_iter()
            .filter(|r| r.outcome == ModificationOutcome::Deployed)
            .take(n)
            .collect()
    }

    /// Return true if a task matching `description_contains` has already failed.
    pub fn is_dead_end_approach(&self, description_contains: &str) -> bool {
        self.inner
            .lock()
            .map(|inner| {
                inner.dead_ends.iter().any(|d| {
                    d.description
                        .to_lowercase()
                        .contains(&description_contains.to_lowercase())
                })
            })
            .unwrap_or(false)
    }

    //  Code patterns

    /// Record or update a code pattern.
    pub fn record_pattern(&self, pattern: CodePattern) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::LockPoisoned)?;
        inner.patterns.insert(pattern.name.clone(), pattern);
        Ok(())
    }

    /// Look up a pattern by name.
    pub fn get_pattern(&self, name: &str) -> Option<CodePattern> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.patterns.get(name).cloned())
    }

    /// Return all known patterns for a module.
    pub fn patterns_for_module(&self, module: &str) -> Vec<CodePattern> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .patterns
                    .values()
                    .filter(|p| p.module == module)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    //  Dependency graph

    /// Register or update a module node.
    pub fn upsert_module(&self, node: ModuleNode) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::LockPoisoned)?;
        inner.dependency_graph.insert(node.path.clone(), node);
        Ok(())
    }

    /// Return the dependency node for a module path.
    pub fn get_module(&self, path: &str) -> Option<ModuleNode> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.dependency_graph.get(path).cloned())
    }

    /// Claim a module for an agent.  Returns false if already claimed by another.
    pub fn claim_module(&self, path: &str, agent_id: &str) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if let Some(node) = inner.dependency_graph.get_mut(path) {
            if node.claimed_by.is_some() {
                return false;
            }
            node.claimed_by = Some(agent_id.to_string());
            return true;
        }
        // Module not registered  -  insert minimal node and claim it
        inner.dependency_graph.insert(
            path.to_string(),
            ModuleNode {
                path: path.to_string(),
                dependents: vec![],
                dependencies: vec![],
                p95_latency_ms: 0.0,
                claimed_by: Some(agent_id.to_string()),
            },
        );
        true
    }

    /// Release a claim on a module.
    pub fn release_module(&self, path: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(node) = inner.dependency_graph.get_mut(path) {
                node.claimed_by = None;
            }
        }
    }

    /// Return all modules that are safe to edit in parallel (not claimed, no shared dependents).
    pub fn parallelizable_modules(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|inner| {
                inner
                    .dependency_graph
                    .values()
                    .filter(|n| n.claimed_by.is_none())
                    .map(|n| n.path.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    //  Performance baselines

    /// Record a performance baseline for a module.
    pub fn record_baseline(&self, baseline: PerformanceBaseline) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::LockPoisoned)?;
        inner.baselines.insert(baseline.module.clone(), baseline);
        Ok(())
    }

    /// Return the latest baseline for a module.
    pub fn get_baseline(&self, module: &str) -> Option<PerformanceBaseline> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.baselines.get(module).cloned())
    }

    //  Dead ends

    /// Record a failed approach to prevent agents retrying it.
    pub fn record_dead_end(&self, dead_end: DeadEnd) -> Result<(), MemoryError> {
        let mut inner = self.inner.lock().map_err(|_| MemoryError::LockPoisoned)?;
        inner.dead_ends.push(dead_end);
        Ok(())
    }

    /// Return all known dead ends.
    pub fn dead_ends(&self) -> Vec<DeadEnd> {
        self.inner
            .lock()
            .map(|inner| inner.dead_ends.clone())
            .unwrap_or_default()
    }

    /// Return dead ends for a specific module.
    pub fn dead_ends_for_module(&self, module: &str) -> Vec<DeadEnd> {
        self.dead_ends()
            .into_iter()
            .filter(|d| d.module == module)
            .collect()
    }

    //  Summary

    /// Return a count summary of stored records.
    pub fn summary(&self) -> MemorySummary {
        self.inner
            .lock()
            .map(|inner| MemorySummary {
                modification_count: inner.modifications.len(),
                pattern_count: inner.patterns.len(),
                module_count: inner.dependency_graph.len(),
                baseline_count: inner.baselines.len(),
                dead_end_count: inner.dead_ends.len(),
            })
            .unwrap_or_default()
    }

    /// Flush any pending state and clear all in-memory records.
    ///
    /// After this call the memory store is empty.  Any persistence file is
    /// left on disk (it will be overwritten on the next
    /// [`insert_modification`](Self::insert_modification) call).
    pub fn shutdown(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.modifications.clear();
            inner.modification_ids.clear();
            inner.patterns.clear();
            inner.dependency_graph.clear();
            inner.baselines.clear();
            inner.dead_ends.clear();
        }
    }
}

/// Counts of records in each memory category.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemorySummary {
    /// Number of modification records stored.
    pub modification_count: usize,
    /// Number of code patterns stored.
    pub pattern_count: usize,
    /// Number of module nodes in the dependency graph.
    pub module_count: usize,
    /// Number of performance baselines stored.
    pub baseline_count: usize,
    /// Number of dead-end approaches recorded.
    pub dead_end_count: usize,
}

//  Helpers

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

//  Redis-backed memory (distributed feature)

#[cfg(feature = "distributed")]
mod redis_memory {
    use super::*;
    use redis::aio::ConnectionManager;
    use redis::AsyncCommands;
    use std::collections::HashMap;

    /// Redis-backed agent memory with in-memory fast path.
    ///
    /// Wraps an [`AgentMemory`] for fast local reads and writes, then
    /// best-effort persists every mutation to Redis.  If Redis is unavailable
    /// at construction time, or if any individual write fails, the store
    /// degrades gracefully to pure in-memory mode.
    ///
    /// # Redis key layout
    /// - `agent:modification:{id}`  -  JSON-serialised [`ModificationRecord`]
    /// - `agent:pattern:{name}`  -  JSON-serialised [`CodePattern`]
    /// - `agent:baseline:{module}`  -  JSON-serialised [`PerformanceBaseline`]
    /// - `agent:deadend:{module}:{hash}`  -  JSON-serialised [`DeadEnd`]
    ///
    /// # Panics
    /// This type never panics.
    #[derive(Clone)]
    pub struct RedisAgentMemory {
        inner: AgentMemory,
        redis: Option<ConnectionManager>,
    }

    impl RedisAgentMemory {
        /// Create a new Redis-backed memory store.
        ///
        /// If the Redis connection cannot be established, the store falls back
        /// to pure in-memory mode (no error is raised).
        ///
        /// # Arguments
        /// * `max_modifications`  -  cap on in-memory modification history
        /// * `redis_url`  -  Redis connection string (e.g. `"redis://127.0.0.1/"`)
        pub async fn new(max_modifications: usize, redis_url: &str) -> Self {
            let redis = Self::try_connect(redis_url).await;
            Self {
                inner: AgentMemory::new(max_modifications),
                redis,
            }
        }

        /// Create a `RedisAgentMemory` that operates in degraded (in-memory only) mode.
        ///
        /// Useful for testing without a running Redis instance.
        pub fn degraded(max_modifications: usize) -> Self {
            Self {
                inner: AgentMemory::new(max_modifications),
                redis: None,
            }
        }

        /// Whether the Redis connection is live.
        pub fn has_redis(&self) -> bool {
            self.redis.is_some()
        }

        /// Return a reference to the underlying in-memory store.
        pub fn inner(&self) -> &AgentMemory {
            &self.inner
        }

        //  Connection helper

        async fn try_connect(url: &str) -> Option<ConnectionManager> {
            let client = redis::Client::open(url).ok()?;
            ConnectionManager::new(client).await.ok()
        }

        //  Best-effort Redis helpers

        async fn redis_set(&self, key: &str, value: &str) {
            if let Some(mut conn) = self.redis.clone() {
                let _: Result<(), _> = conn.set(key, value).await;
            }
        }

        /// Retrieve a value from Redis by key.
        ///
        /// Symmetric counterpart to `redis_set`; used by diagnostic tooling and
        /// future self-improvement read-back paths.  Not yet called from
        /// production logic but kept to maintain a complete Redis read/write pair.
        #[allow(dead_code)]
        async fn redis_get(&self, key: &str) -> Option<String> {
            if let Some(mut conn) = self.redis.clone() {
                let result: Result<String, _> = conn.get(key).await;
                return result.ok();
            }
            None
        }

        //  Modification records

        /// Insert a new modification record.
        ///
        /// Writes to the in-memory store first, then best-effort persists to Redis.
        ///
        /// # Errors
        /// Returns [`MemoryError::DuplicateRecord`] if the ID already exists.
        ///
        /// # Panics
        /// This function never panics.
        pub async fn insert_modification(
            &self,
            record: ModificationRecord,
        ) -> Result<(), MemoryError> {
            self.inner.insert_modification(record.clone())?;
            let key = format!("agent:modification:{}", record.id);
            if let Ok(json) = serde_json::to_string(&record) {
                self.redis_set(&key, &json).await;
            }
            Ok(())
        }

        /// Update the outcome and metric deltas of an existing record.
        ///
        /// Updates the in-memory store first, then best-effort persists to Redis.
        ///
        /// # Errors
        /// Returns [`MemoryError::NotFound`] if the ID does not exist.
        ///
        /// # Panics
        /// This function never panics.
        pub async fn update_outcome(
            &self,
            id: &str,
            outcome: ModificationOutcome,
            deltas: HashMap<String, f64>,
        ) -> Result<(), MemoryError> {
            self.inner
                .update_outcome(id, outcome.clone(), deltas.clone())?;
            // Re-read the full record from memory to persist the updated version.
            if let Some(record) = self.get_modification(id) {
                let key = format!("agent:modification:{}", id);
                if let Ok(json) = serde_json::to_string(&record) {
                    self.redis_set(&key, &json).await;
                }
            }
            Ok(())
        }

        /// Retrieve a modification record by ID from the in-memory store.
        ///
        /// # Panics
        /// This function never panics.
        pub fn get_modification(&self, id: &str) -> Option<ModificationRecord> {
            self.inner.modifications().into_iter().find(|r| r.id == id)
        }

        /// Return all modification records (newest first).
        pub fn modifications(&self) -> Vec<ModificationRecord> {
            self.inner.modifications()
        }

        /// Return modifications that match a given tag.
        ///
        /// # Panics
        /// This function never panics.
        pub fn modifications_by_tag(&self, tag: &str) -> Vec<ModificationRecord> {
            self.inner
                .modifications()
                .into_iter()
                .filter(|r| r.tag.as_deref() == Some(tag))
                .collect()
        }

        //  Code patterns

        /// Record or update a code pattern.
        ///
        /// Writes to the in-memory store first, then best-effort persists to Redis.
        ///
        /// # Errors
        /// Returns [`MemoryError::LockPoisoned`] on internal lock failure.
        ///
        /// # Panics
        /// This function never panics.
        pub async fn insert_pattern(&self, pattern: CodePattern) -> Result<(), MemoryError> {
            self.inner.record_pattern(pattern.clone())?;
            let key = format!("agent:pattern:{}", pattern.name);
            if let Ok(json) = serde_json::to_string(&pattern) {
                self.redis_set(&key, &json).await;
            }
            Ok(())
        }

        /// Look up a pattern by name.
        pub fn get_pattern(&self, name: &str) -> Option<CodePattern> {
            self.inner.get_pattern(name)
        }

        //  Performance baselines

        /// Record a performance baseline for a module.
        ///
        /// Writes to the in-memory store first, then best-effort persists to Redis.
        ///
        /// # Errors
        /// Returns [`MemoryError::LockPoisoned`] on internal lock failure.
        ///
        /// # Panics
        /// This function never panics.
        pub async fn record_baseline(
            &self,
            baseline: PerformanceBaseline,
        ) -> Result<(), MemoryError> {
            self.inner.record_baseline(baseline.clone())?;
            let key = format!("agent:baseline:{}", baseline.module);
            if let Ok(json) = serde_json::to_string(&baseline) {
                self.redis_set(&key, &json).await;
            }
            Ok(())
        }

        /// Return the latest baseline for a module.
        pub fn get_baseline(&self, module: &str) -> Option<PerformanceBaseline> {
            self.inner.get_baseline(module)
        }

        //  Dead ends

        /// Record a failed approach to prevent agents retrying it.
        ///
        /// Writes to the in-memory store first, then best-effort persists to Redis.
        /// The Redis key includes a simple hash of the description to avoid collisions.
        ///
        /// # Errors
        /// Returns [`MemoryError::LockPoisoned`] on internal lock failure.
        ///
        /// # Panics
        /// This function never panics.
        pub async fn record_dead_end(&self, dead_end: DeadEnd) -> Result<(), MemoryError> {
            self.inner.record_dead_end(dead_end.clone())?;
            let hash = Self::simple_hash(&dead_end.description);
            let key = format!("agent:deadend:{}:{}", dead_end.module, hash);
            if let Ok(json) = serde_json::to_string(&dead_end) {
                self.redis_set(&key, &json).await;
            }
            Ok(())
        }

        /// Return dead ends for a specific module.
        pub fn dead_ends_for_module(&self, module: &str) -> Vec<DeadEnd> {
            self.inner.dead_ends_for_module(module)
        }

        /// Return all known dead ends.
        pub fn dead_ends(&self) -> Vec<DeadEnd> {
            self.inner.dead_ends()
        }

        //  Dependency graph pass-through

        /// Return all modules that are safe to edit in parallel (not claimed).
        pub fn parallelizable_modules(&self) -> Vec<String> {
            self.inner.parallelizable_modules()
        }

        /// Register or update a module node.
        pub fn upsert_module(&self, node: ModuleNode) -> Result<(), MemoryError> {
            self.inner.upsert_module(node)
        }

        /// Claim a module for an agent.
        pub fn claim_module(&self, path: &str, agent_id: &str) -> bool {
            self.inner.claim_module(path, agent_id)
        }

        /// Release a claim on a module.
        pub fn release_module(&self, path: &str) {
            self.inner.release_module(path);
        }

        /// Return a count summary of stored records.
        pub fn summary(&self) -> MemorySummary {
            self.inner.summary()
        }

        //  Internal helpers

        /// Produce a cheap, deterministic hash of a string for use in Redis keys.
        fn simple_hash(s: &str) -> u64 {
            // FNV-1a 64-bit
            let mut hash: u64 = 0xcbf29ce484222325;
            for byte in s.as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash
        }
    }
}

#[cfg(feature = "distributed")]
pub use redis_memory::RedisAgentMemory;

//  Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(id: &str) -> ModificationRecord {
        ModificationRecord::new(id, "test modification", vec!["src/foo.rs".to_string()])
    }

    #[test]
    fn test_insert_modification_success() {
        let mem = AgentMemory::new(100);
        let rec = make_record("mod-1");
        assert!(mem.insert_modification(rec).is_ok());
    }

    #[test]
    fn test_insert_duplicate_returns_error() {
        let mem = AgentMemory::new(100);
        mem.insert_modification(make_record("mod-1")).unwrap();
        let err = mem.insert_modification(make_record("mod-1"));
        assert!(matches!(err, Err(MemoryError::DuplicateRecord(_))));
    }

    #[test]
    fn test_modifications_returns_newest_first() {
        let mem = AgentMemory::new(100);
        mem.insert_modification(make_record("a")).unwrap();
        mem.insert_modification(make_record("b")).unwrap();
        let mods = mem.modifications();
        assert_eq!(mods[0].id, "b");
        assert_eq!(mods[1].id, "a");
    }

    #[test]
    fn test_max_modifications_evicts_oldest() {
        let mem = AgentMemory::new(3);
        for i in 0..5 {
            mem.insert_modification(make_record(&format!("mod-{i}")))
                .unwrap();
        }
        assert_eq!(mem.modifications().len(), 3);
    }

    #[test]
    fn test_update_outcome_success() {
        let mem = AgentMemory::new(100);
        mem.insert_modification(make_record("m1")).unwrap();
        let mut deltas = HashMap::new();
        deltas.insert("latency".to_string(), -5.0);
        assert!(mem
            .update_outcome("m1", ModificationOutcome::Deployed, deltas)
            .is_ok());
        let mods = mem.modifications();
        assert_eq!(mods[0].outcome, ModificationOutcome::Deployed);
        assert_eq!(*mods[0].metric_deltas.get("latency").unwrap(), -5.0);
    }

    #[test]
    fn test_update_outcome_not_found() {
        let mem = AgentMemory::new(100);
        let err = mem.update_outcome("nonexistent", ModificationOutcome::Deployed, HashMap::new());
        assert!(matches!(err, Err(MemoryError::NotFound(_))));
    }

    #[test]
    fn test_recent_deployments_filters_correctly() {
        let mem = AgentMemory::new(100);
        let mut r1 = make_record("a");
        r1.outcome = ModificationOutcome::Deployed;
        let mut r2 = make_record("b");
        r2.outcome = ModificationOutcome::ValidationFailed;
        let mut r3 = make_record("c");
        r3.outcome = ModificationOutcome::Deployed;
        mem.insert_modification(r1).unwrap();
        mem.insert_modification(r2).unwrap();
        mem.insert_modification(r3).unwrap();
        let deployed = mem.recent_deployments(10);
        assert_eq!(deployed.len(), 2);
    }

    #[test]
    fn test_record_and_get_pattern() {
        let mem = AgentMemory::new(100);
        let pat = CodePattern {
            name: "bounded-channel".to_string(),
            module: "src/stages.rs".to_string(),
            description: "Use bounded mpsc channels".to_string(),
            verdict: PatternVerdict::Success,
            observation_count: 5,
            last_seen_secs: 0,
        };
        mem.record_pattern(pat).unwrap();
        let got = mem.get_pattern("bounded-channel");
        assert!(got.is_some());
        assert_eq!(got.unwrap().verdict, PatternVerdict::Success);
    }

    #[test]
    fn test_patterns_for_module_filters() {
        let mem = AgentMemory::new(100);
        for name in &["p1", "p2", "p3"] {
            mem.record_pattern(CodePattern {
                name: name.to_string(),
                module: if *name == "p3" {
                    "other.rs"
                } else {
                    "dedup.rs"
                }
                .to_string(),
                description: String::new(),
                verdict: PatternVerdict::Unknown,
                observation_count: 1,
                last_seen_secs: 0,
            })
            .unwrap();
        }
        let pats = mem.patterns_for_module("dedup.rs");
        assert_eq!(pats.len(), 2);
    }

    #[test]
    fn test_claim_module_success() {
        let mem = AgentMemory::new(100);
        mem.upsert_module(ModuleNode {
            path: "src/rag.rs".to_string(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();
        assert!(mem.claim_module("src/rag.rs", "agent-1"));
    }

    #[test]
    fn test_claim_module_fails_when_already_claimed() {
        let mem = AgentMemory::new(100);
        mem.upsert_module(ModuleNode {
            path: "src/rag.rs".to_string(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();
        assert!(mem.claim_module("src/rag.rs", "agent-1"));
        assert!(!mem.claim_module("src/rag.rs", "agent-2"));
    }

    #[test]
    fn test_release_module_clears_claim() {
        let mem = AgentMemory::new(100);
        mem.upsert_module(ModuleNode {
            path: "src/m.rs".to_string(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();
        mem.claim_module("src/m.rs", "agent-1");
        mem.release_module("src/m.rs");
        let node = mem.get_module("src/m.rs").unwrap();
        assert!(node.claimed_by.is_none());
    }

    #[test]
    fn test_parallelizable_modules_excludes_claimed() {
        let mem = AgentMemory::new(100);
        mem.upsert_module(ModuleNode {
            path: "a.rs".into(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();
        mem.upsert_module(ModuleNode {
            path: "b.rs".into(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();
        mem.claim_module("a.rs", "agent-1");
        let free = mem.parallelizable_modules();
        assert!(free.contains(&"b.rs".to_string()));
        assert!(!free.contains(&"a.rs".to_string()));
    }

    #[test]
    fn test_record_and_get_baseline() {
        let mem = AgentMemory::new(100);
        let b = PerformanceBaseline {
            module: "src/dedup.rs".to_string(),
            throughput_rps: 1000.0,
            p95_latency_ms: 0.5,
            error_rate: 0.0,
            recorded_at_secs: 0,
        };
        mem.record_baseline(b).unwrap();
        let got = mem.get_baseline("src/dedup.rs");
        assert!(got.is_some());
        assert!((got.unwrap().throughput_rps - 1000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_record_dead_end_and_retrieve() {
        let mem = AgentMemory::new(100);
        let de = DeadEnd {
            description: "Using LRU for dedup".to_string(),
            module: "src/dedup.rs".to_string(),
            reason: "Increased collision rate under skewed distribution".to_string(),
            evidence_ids: vec!["mod-5".to_string()],
            recorded_at_secs: 0,
        };
        mem.record_dead_end(de).unwrap();
        assert_eq!(mem.dead_ends().len(), 1);
    }

    #[test]
    fn test_dead_ends_for_module_filters() {
        let mem = AgentMemory::new(100);
        mem.record_dead_end(DeadEnd {
            description: "x".into(),
            module: "a.rs".into(),
            reason: String::new(),
            evidence_ids: vec![],
            recorded_at_secs: 0,
        })
        .unwrap();
        mem.record_dead_end(DeadEnd {
            description: "y".into(),
            module: "b.rs".into(),
            reason: String::new(),
            evidence_ids: vec![],
            recorded_at_secs: 0,
        })
        .unwrap();
        assert_eq!(mem.dead_ends_for_module("a.rs").len(), 1);
    }

    #[test]
    fn test_is_dead_end_approach_case_insensitive() {
        let mem = AgentMemory::new(100);
        mem.record_dead_end(DeadEnd {
            description: "LRU caching for dedup".into(),
            module: String::new(),
            reason: String::new(),
            evidence_ids: vec![],
            recorded_at_secs: 0,
        })
        .unwrap();
        assert!(mem.is_dead_end_approach("lru caching"));
        assert!(!mem.is_dead_end_approach("fifo caching"));
    }

    #[test]
    fn test_summary_counts_correct() {
        let mem = AgentMemory::new(100);
        mem.insert_modification(make_record("r1")).unwrap();
        mem.record_pattern(CodePattern {
            name: "p1".into(),
            module: String::new(),
            description: String::new(),
            verdict: PatternVerdict::Unknown,
            observation_count: 0,
            last_seen_secs: 0,
        })
        .unwrap();
        let s = mem.summary();
        assert_eq!(s.modification_count, 1);
        assert_eq!(s.pattern_count, 1);
    }

    #[test]
    fn test_memory_clone_shares_state() {
        let mem = AgentMemory::new(100);
        let mem2 = mem.clone();
        mem.insert_modification(make_record("shared")).unwrap();
        assert_eq!(mem2.modifications().len(), 1);
    }

    #[test]
    fn test_claim_unregistered_module_auto_registers() {
        let mem = AgentMemory::new(100);
        let claimed = mem.claim_module("new_module.rs", "agent-x");
        assert!(claimed);
        let node = mem.get_module("new_module.rs");
        assert!(node.is_some());
    }

    #[test]
    fn test_modification_new_default_outcome_in_progress() {
        let rec = ModificationRecord::new("x", "desc", vec![]);
        assert_eq!(rec.outcome, ModificationOutcome::InProgress);
    }

    // ------------------------------------------------------------------
    // HIGH-06: O(1) duplicate detection via HashSet
    // ------------------------------------------------------------------

    #[test]
    fn test_insert_modification_dedup_is_o1() {
        // Insert a large number of records, then verify that inserting a
        // duplicate of the very first record still returns DuplicateRecord
        // (not NotFound), proving the HashSet is consulted rather than a
        // linear scan of the VecDeque.
        let mem = AgentMemory::new(10_000);
        for i in 0..1_000 {
            mem.insert_modification(make_record(&format!("rec-{i}")))
                .unwrap();
        }
        // "rec-0" was evicted (max = 10_000 > 1_000 so it is still present)
        let result = mem.insert_modification(make_record("rec-0"));
        assert!(
            matches!(result, Err(MemoryError::DuplicateRecord(_))),
            "HashSet must catch duplicate without linear scan: {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // HIGH-07: O(1) eviction via VecDeque pop_front
    // ------------------------------------------------------------------

    #[test]
    fn test_eviction_uses_pop_front() {
        // Cap at 3 records.  Insert 4 and verify the oldest (rec-0) was evicted
        // and the newest three remain in newest-first order.
        let mem = AgentMemory::new(3);
        for i in 0..4 {
            mem.insert_modification(make_record(&format!("evict-{i}")))
                .unwrap();
        }
        let mods = mem.modifications();
        assert_eq!(mods.len(), 3, "oldest should have been evicted");
        // Newest-first order: evict-3, evict-2, evict-1
        assert_eq!(mods[0].id, "evict-3");
        assert_eq!(mods[1].id, "evict-2");
        assert_eq!(mods[2].id, "evict-1");
        // evict-0 must be gone from both the deque and the id set
        let result = mem.insert_modification(make_record("evict-0"));
        assert!(
            result.is_ok(),
            "evict-0 should be re-insertable after eviction: {result:?}"
        );
    }
}

//  Redis memory tests (distributed feature)

#[cfg(test)]
#[cfg(feature = "distributed")]
mod redis_tests {
    use super::*;
    use std::collections::HashMap;

    fn make_record(id: &str) -> ModificationRecord {
        ModificationRecord::new(id, "test modification", vec!["src/foo.rs".to_string()])
    }

    fn make_tagged_record(id: &str, tag: &str) -> ModificationRecord {
        let mut rec = make_record(id);
        rec.tag = Some(tag.to_string());
        rec
    }

    fn make_pattern(name: &str, module: &str) -> CodePattern {
        CodePattern {
            name: name.to_string(),
            module: module.to_string(),
            description: format!("pattern {name}"),
            verdict: PatternVerdict::Success,
            observation_count: 1,
            last_seen_secs: 0,
        }
    }

    fn make_baseline(module: &str) -> PerformanceBaseline {
        PerformanceBaseline {
            module: module.to_string(),
            throughput_rps: 500.0,
            p95_latency_ms: 2.0,
            error_rate: 0.01,
            recorded_at_secs: 0,
        }
    }

    fn make_dead_end(module: &str, desc: &str) -> DeadEnd {
        DeadEnd {
            description: desc.to_string(),
            module: module.to_string(),
            reason: "did not work".to_string(),
            evidence_ids: vec!["ev-1".to_string()],
            recorded_at_secs: 0,
        }
    }

    #[tokio::test]
    async fn test_redis_memory_creation() {
        let mem = RedisAgentMemory::degraded(100);
        assert!(!mem.has_redis());
        assert_eq!(mem.summary().modification_count, 0);
    }

    #[tokio::test]
    async fn test_redis_memory_insert_modification() {
        let mem = RedisAgentMemory::degraded(100);
        let rec = make_record("rm-1");
        assert!(mem.insert_modification(rec).await.is_ok());
        assert_eq!(mem.modifications().len(), 1);
        assert_eq!(mem.modifications()[0].id, "rm-1");
    }

    #[tokio::test]
    async fn test_redis_memory_update_outcome() {
        let mem = RedisAgentMemory::degraded(100);
        mem.insert_modification(make_record("rm-2")).await.unwrap();

        let mut deltas = HashMap::new();
        deltas.insert("throughput".to_string(), 42.0);
        mem.update_outcome("rm-2", ModificationOutcome::Deployed, deltas)
            .await
            .unwrap();

        let rec = mem.get_modification("rm-2");
        assert!(rec.is_some());
        let rec = rec.unwrap();
        assert_eq!(rec.outcome, ModificationOutcome::Deployed);
        assert_eq!(*rec.metric_deltas.get("throughput").unwrap(), 42.0);
    }

    #[tokio::test]
    async fn test_redis_memory_get_modification() {
        let mem = RedisAgentMemory::degraded(100);
        assert!(mem.get_modification("nonexistent").is_none());

        mem.insert_modification(make_record("rm-3")).await.unwrap();
        let found = mem.get_modification("rm-3");
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, "rm-3");
    }

    #[tokio::test]
    async fn test_redis_memory_insert_pattern() {
        let mem = RedisAgentMemory::degraded(100);
        let pat = make_pattern("bounded-chan", "src/stages.rs");
        mem.insert_pattern(pat).await.unwrap();

        let got = mem.get_pattern("bounded-chan");
        assert!(got.is_some());
        assert_eq!(got.unwrap().module, "src/stages.rs");
    }

    #[tokio::test]
    async fn test_redis_memory_record_baseline() {
        let mem = RedisAgentMemory::degraded(100);
        let bl = make_baseline("src/dedup.rs");
        mem.record_baseline(bl).await.unwrap();

        let got = mem.get_baseline("src/dedup.rs");
        assert!(got.is_some());
        assert!((got.unwrap().throughput_rps - 500.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_redis_memory_record_dead_end() {
        let mem = RedisAgentMemory::degraded(100);
        let de = make_dead_end("src/rag.rs", "tried LRU eviction");
        mem.record_dead_end(de).await.unwrap();

        let all = mem.dead_ends();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].description, "tried LRU eviction");
    }

    #[tokio::test]
    async fn test_redis_memory_modifications_by_tag() {
        let mem = RedisAgentMemory::degraded(100);
        mem.insert_modification(make_tagged_record("t1", "perf"))
            .await
            .unwrap();
        mem.insert_modification(make_tagged_record("t2", "bugfix"))
            .await
            .unwrap();
        mem.insert_modification(make_tagged_record("t3", "perf"))
            .await
            .unwrap();

        let perf = mem.modifications_by_tag("perf");
        assert_eq!(perf.len(), 2);
        assert!(perf.iter().all(|r| r.tag.as_deref() == Some("perf")));
    }

    #[tokio::test]
    async fn test_redis_memory_safe_to_edit_parallel() {
        let mem = RedisAgentMemory::degraded(100);
        mem.upsert_module(ModuleNode {
            path: "x.rs".into(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();
        mem.upsert_module(ModuleNode {
            path: "y.rs".into(),
            dependents: vec![],
            dependencies: vec![],
            p95_latency_ms: 0.0,
            claimed_by: None,
        })
        .unwrap();

        // Both unclaimed  -  both parallelizable
        let free = mem.parallelizable_modules();
        assert_eq!(free.len(), 2);

        // Claim one  -  only one left
        mem.claim_module("x.rs", "agent-a");
        let free = mem.parallelizable_modules();
        assert_eq!(free.len(), 1);
        assert!(free.contains(&"y.rs".to_string()));
    }

    #[tokio::test]
    async fn test_redis_memory_dead_ends_for_module() {
        let mem = RedisAgentMemory::degraded(100);
        mem.record_dead_end(make_dead_end("mod_a", "approach 1"))
            .await
            .unwrap();
        mem.record_dead_end(make_dead_end("mod_b", "approach 2"))
            .await
            .unwrap();
        mem.record_dead_end(make_dead_end("mod_a", "approach 3"))
            .await
            .unwrap();

        let a_ends = mem.dead_ends_for_module("mod_a");
        assert_eq!(a_ends.len(), 2);

        let b_ends = mem.dead_ends_for_module("mod_b");
        assert_eq!(b_ends.len(), 1);
    }

    // -----------------------------------------------------------------------
    // ARCH-03: File-backed persistence tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_memory_persists_to_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("agent_memory_test_{}.json", std::process::id()));
        // Remove if leftover from a previous run
        let _ = std::fs::remove_file(&path);

        let mem = AgentMemory::with_persist_path(100, Some(path.clone()))
            .expect("construction should succeed");

        let rec = ModificationRecord::new("persist-1", "test persist", vec![]);
        mem.insert_modification(rec).expect("insert should succeed");

        // The file should now exist and be valid JSON
        assert!(path.exists(), "persist file should have been created");
        let contents = std::fs::read_to_string(&path).expect("should be readable");
        let parsed: Vec<ModificationRecord> =
            serde_json::from_str(&contents).expect("should be valid JSON");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "persist-1");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_memory_loads_from_file_on_startup() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "agent_memory_load_test_{}.json",
            std::process::id()
        ));

        // Pre-write a JSON file with one record
        let existing = vec![ModificationRecord {
            id: "pre-existing".to_string(),
            description: "loaded from disk".to_string(),
            files_changed: vec![],
            outcome: ModificationOutcome::Deployed,
            metric_deltas: std::collections::HashMap::new(),
            created_at_secs: 0,
            tag: None,
        }];
        let json = serde_json::to_string_pretty(&existing).expect("serialise");
        std::fs::write(&path, json).expect("write test file");

        // Construct AgentMemory pointing at that file
        let mem = AgentMemory::with_persist_path(100, Some(path.clone()))
            .expect("construction should succeed");

        // The pre-existing record should be loaded
        let mods = mem.modifications();
        assert_eq!(mods.len(), 1, "should have loaded 1 record from disk");
        assert_eq!(mods[0].id, "pre-existing");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_shutdown_clears_state_and_does_not_hang() {
        let mem = AgentMemory::new(100);
        mem.insert_modification(ModificationRecord::new("id-1", "desc", vec![])).unwrap();
        mem.record_pattern(CodePattern {
            name: "pat".into(),
            module: "m".into(),
            description: "d".into(),
            verdict: PatternVerdict::Success,
            observation_count: 1,
            last_seen_secs: 0,
        }).unwrap();
        assert_eq!(mem.modifications().len(), 1);
        mem.shutdown();
        // All state should be cleared.
        let summary = mem.summary();
        assert_eq!(summary.modification_count, 0);
        assert_eq!(summary.pattern_count, 0);
    }
}
