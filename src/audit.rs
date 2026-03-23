//! # Request Replay / Audit Log
//!
//! An append-only, capacity-bounded audit log for LLM inference requests and
//! responses.  Entries can be filtered, queried, and exported as JSONL.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::audit::{AuditEntry, AuditFilter, AuditLog};
//! use chrono::Utc;
//!
//! let mut log = AuditLog::new(1000);
//!
//! let entry = AuditEntry {
//!     id: 1,
//!     timestamp: Utc::now(),
//!     model_id: "claude-3-5-sonnet".to_string(),
//!     input: "Hello!".to_string(),
//!     output: vec!["Hi there!".to_string()],
//!     latency_ms: 320,
//!     cache_hit: false,
//!     tags: vec!["prod".to_string()],
//! };
//!
//! log.append(entry);
//!
//! let results = log.query(&AuditFilter::default());
//! assert_eq!(results.len(), 1);
//! ```

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Write;

// ── Entry ──────────────────────────────────────────────────────────────────────

/// A single audit log entry recording one inference request/response pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Monotonically increasing entry identifier within this log instance.
    pub id: u64,
    /// UTC timestamp at which the request was received.
    pub timestamp: DateTime<Utc>,
    /// Model identifier (e.g. `"claude-3-5-sonnet"`, `"gpt-4o"`).
    pub model_id: String,
    /// The raw prompt / input text sent to the model.
    pub input: String,
    /// One or more output tokens/chunks returned by the model.
    pub output: Vec<String>,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the response was served from a local cache.
    pub cache_hit: bool,
    /// Arbitrary string tags for grouping / filtering (e.g. `"prod"`, `"user:abc"`).
    pub tags: Vec<String>,
}

// ── Filter ─────────────────────────────────────────────────────────────────────

/// Predicate for [`AuditLog::query`].
///
/// All provided fields are AND-combined.  A `None` field matches any value.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// Only return entries with `timestamp >= since`.
    pub since: Option<DateTime<Utc>>,
    /// Only return entries whose `model_id` equals this value.
    pub model_id: Option<String>,
    /// Only return entries matching this cache-hit status.
    pub cache_hit: Option<bool>,
    /// Only return entries with `latency_ms >= min_latency_ms`.
    pub min_latency_ms: Option<u64>,
}

impl AuditFilter {
    /// Return `true` if `entry` satisfies all filter predicates.
    pub fn matches(&self, entry: &AuditEntry) -> bool {
        if let Some(since) = self.since {
            if entry.timestamp < since {
                return false;
            }
        }
        if let Some(ref model) = self.model_id {
            if &entry.model_id != model {
                return false;
            }
        }
        if let Some(hit) = self.cache_hit {
            if entry.cache_hit != hit {
                return false;
            }
        }
        if let Some(min_lat) = self.min_latency_ms {
            if entry.latency_ms < min_lat {
                return false;
            }
        }
        true
    }
}

// ── Stats ──────────────────────────────────────────────────────────────────────

/// Aggregate statistics computed over all entries currently in the log.
#[derive(Debug, Clone)]
pub struct AuditStats {
    /// Total number of entries in the log.
    pub total_entries: usize,
    /// Fraction of entries that were cache hits (0.0–1.0).
    pub cache_hit_rate: f64,
    /// Mean latency across all entries in milliseconds.
    pub avg_latency_ms: f64,
    /// Per-model entry counts.
    pub by_model: HashMap<String, u64>,
}

// ── Log ───────────────────────────────────────────────────────────────────────

/// Append-only audit log backed by a [`VecDeque`].
///
/// When the log reaches `capacity` entries the oldest entry is evicted before
/// the new one is inserted, keeping memory usage bounded.
pub struct AuditLog {
    entries: VecDeque<AuditEntry>,
    capacity: usize,
    next_id: u64,
}

impl AuditLog {
    /// Create a new log with the given maximum `capacity`.
    ///
    /// A capacity of `0` is legal but means every appended entry is immediately
    /// evicted.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            next_id: 1,
        }
    }

    /// Append `entry` to the log, assigning a fresh `id` to it.
    ///
    /// If the log is already at capacity the oldest entry is removed first.
    pub fn append(&mut self, mut entry: AuditEntry) {
        if self.capacity == 0 {
            return;
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        entry.id = self.next_id;
        self.next_id += 1;
        self.entries.push_back(entry);
    }

    /// Return all entries matching `filter`, in insertion order (oldest first).
    pub fn query(&self, filter: &AuditFilter) -> Vec<&AuditEntry> {
        self.entries.iter().filter(|e| filter.matches(e)).collect()
    }

    /// Compute aggregate statistics over all entries currently in the log.
    pub fn stats(&self) -> AuditStats {
        let total = self.entries.len();
        if total == 0 {
            return AuditStats {
                total_entries: 0,
                cache_hit_rate: 0.0,
                avg_latency_ms: 0.0,
                by_model: HashMap::new(),
            };
        }
        let hits = self.entries.iter().filter(|e| e.cache_hit).count();
        let total_latency: u64 = self.entries.iter().map(|e| e.latency_ms).sum();
        let mut by_model: HashMap<String, u64> = HashMap::new();
        for e in &self.entries {
            *by_model.entry(e.model_id.clone()).or_insert(0) += 1;
        }
        AuditStats {
            total_entries: total,
            cache_hit_rate: hits as f64 / total as f64,
            avg_latency_ms: total_latency as f64 / total as f64,
            by_model,
        }
    }

    /// Serialize all entries to `writer` as newline-delimited JSON (JSONL).
    ///
    /// Each line is a JSON object corresponding to one [`AuditEntry`].
    pub fn export_jsonl(&self, writer: &mut dyn Write) -> std::io::Result<()> {
        for entry in &self.entries {
            let line = serde_json::to_string(entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(writer, "{}", line)?;
        }
        Ok(())
    }

    /// Return the number of entries currently stored.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return `true` if the log contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return the configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

// ── HTTP handler helpers ───────────────────────────────────────────────────────
// These are standalone functions rather than axum routes so the audit module
// compiles without the `web-api` feature.  The web_api module can re-export
// them when the feature is active.

/// Serialisable response body for `GET /api/v1/audit`.
#[derive(Debug, Serialize)]
pub struct AuditQueryResponse {
    /// Entries returned by the query.
    pub entries: Vec<AuditEntry>,
    /// Total number of entries returned.
    pub count: usize,
}

/// Serialisable response body for `GET /api/v1/audit/stats`.
#[derive(Debug, Serialize)]
pub struct AuditStatsResponse {
    /// Total entries in the log.
    pub total_entries: usize,
    /// Cache hit rate (0.0–1.0).
    pub cache_hit_rate: f64,
    /// Average latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Per-model entry counts.
    pub by_model: HashMap<String, u64>,
}

impl From<AuditStats> for AuditStatsResponse {
    fn from(s: AuditStats) -> Self {
        Self {
            total_entries: s.total_entries,
            cache_hit_rate: s.cache_hit_rate,
            avg_latency_ms: s.avg_latency_ms,
            by_model: s.by_model,
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn make_entry(model: &str, latency_ms: u64, cache_hit: bool) -> AuditEntry {
        AuditEntry {
            id: 0, // will be assigned by AuditLog::append
            timestamp: Utc::now(),
            model_id: model.to_string(),
            input: "test input".to_string(),
            output: vec!["test output".to_string()],
            latency_ms,
            cache_hit,
            tags: vec![],
        }
    }

    // ── Basic append / query ──────────────────────────────────────────────────

    #[test]
    fn append_and_query_all() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("gpt-4o", 200, false));
        log.append(make_entry("claude-3", 150, true));
        let all = log.query(&AuditFilter::default());
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn ids_are_assigned_sequentially() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("m", 100, false));
        log.append(make_entry("m", 100, false));
        log.append(make_entry("m", 100, false));
        let all = log.query(&AuditFilter::default());
        let ids: Vec<u64> = all.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut log = AuditLog::new(2);
        log.append(make_entry("m", 100, false));
        log.append(make_entry("m", 200, false));
        log.append(make_entry("m", 300, false)); // evicts first
        assert_eq!(log.len(), 2);
        let latencies: Vec<u64> = log.query(&AuditFilter::default())
            .iter().map(|e| e.latency_ms).collect();
        assert_eq!(latencies, vec![200, 300]);
    }

    #[test]
    fn capacity_zero_stores_nothing() {
        let mut log = AuditLog::new(0);
        log.append(make_entry("m", 100, false));
        assert!(log.is_empty());
    }

    // ── Filter ────────────────────────────────────────────────────────────────

    #[test]
    fn filter_by_model_id() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("gpt-4o", 100, false));
        log.append(make_entry("claude-3", 200, false));
        let filter = AuditFilter { model_id: Some("gpt-4o".to_string()), ..Default::default() };
        let results = log.query(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].model_id, "gpt-4o");
    }

    #[test]
    fn filter_by_cache_hit() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("m", 100, false));
        log.append(make_entry("m", 200, true));
        log.append(make_entry("m", 300, true));
        let filter = AuditFilter { cache_hit: Some(true), ..Default::default() };
        let results = log.query(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn filter_by_min_latency() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("m", 50, false));
        log.append(make_entry("m", 100, false));
        log.append(make_entry("m", 500, false));
        let filter = AuditFilter { min_latency_ms: Some(100), ..Default::default() };
        let results = log.query(&filter);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn filter_by_since() {
        let mut log = AuditLog::new(100);
        let old_entry = AuditEntry {
            id: 0,
            timestamp: Utc::now() - Duration::hours(2),
            model_id: "m".to_string(),
            input: "old".to_string(),
            output: vec![],
            latency_ms: 100,
            cache_hit: false,
            tags: vec![],
        };
        let new_entry = make_entry("m", 100, false);
        log.append(old_entry);
        log.append(new_entry);
        let cutoff = Utc::now() - Duration::minutes(30);
        let filter = AuditFilter { since: Some(cutoff), ..Default::default() };
        let results = log.query(&filter);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn filter_combined_predicates() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("gpt-4o", 100, false));
        log.append(make_entry("gpt-4o", 500, true));
        log.append(make_entry("claude-3", 500, true));
        let filter = AuditFilter {
            model_id: Some("gpt-4o".to_string()),
            cache_hit: Some(true),
            ..Default::default()
        };
        let results = log.query(&filter);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].latency_ms, 500);
    }

    // ── Stats ─────────────────────────────────────────────────────────────────

    #[test]
    fn stats_empty_log() {
        let log = AuditLog::new(100);
        let stats = log.stats();
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.cache_hit_rate, 0.0);
        assert_eq!(stats.avg_latency_ms, 0.0);
        assert!(stats.by_model.is_empty());
    }

    #[test]
    fn stats_cache_hit_rate() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("m", 100, true));
        log.append(make_entry("m", 100, false));
        let stats = log.stats();
        assert!((stats.cache_hit_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn stats_avg_latency() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("m", 100, false));
        log.append(make_entry("m", 200, false));
        let stats = log.stats();
        assert!((stats.avg_latency_ms - 150.0).abs() < 1e-9);
    }

    #[test]
    fn stats_by_model_counts() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("gpt-4o", 100, false));
        log.append(make_entry("gpt-4o", 100, false));
        log.append(make_entry("claude-3", 100, false));
        let stats = log.stats();
        assert_eq!(stats.by_model["gpt-4o"], 2);
        assert_eq!(stats.by_model["claude-3"], 1);
    }

    // ── JSONL export ──────────────────────────────────────────────────────────

    #[test]
    fn export_jsonl_produces_valid_lines() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("gpt-4o", 100, false));
        log.append(make_entry("claude-3", 200, true));

        let mut buf: Vec<u8> = Vec::new();
        log.export_jsonl(&mut buf).unwrap();

        let text = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line should parse as valid JSON with expected fields.
        for line in &lines {
            let val: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(val.get("model_id").is_some());
            assert!(val.get("latency_ms").is_some());
        }
    }

    #[test]
    fn export_jsonl_empty_log_empty_output() {
        let log = AuditLog::new(100);
        let mut buf: Vec<u8> = Vec::new();
        log.export_jsonl(&mut buf).unwrap();
        assert!(buf.is_empty());
    }

    // ── AuditStatsResponse conversion ─────────────────────────────────────────

    #[test]
    fn audit_stats_response_from_stats() {
        let mut log = AuditLog::new(100);
        log.append(make_entry("m", 100, true));
        let stats = log.stats();
        let resp = AuditStatsResponse::from(stats.clone());
        assert_eq!(resp.total_entries, stats.total_entries);
        assert!((resp.cache_hit_rate - stats.cache_hit_rate).abs() < 1e-9);
    }
}
