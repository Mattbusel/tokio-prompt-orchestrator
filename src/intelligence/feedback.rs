//! # Stage: Feedback Ingestion (Task 3.6)
//!
//! ## Responsibility
//!
//! Collect and unify feedback from multiple sources (user ratings, automated
//! quality checks, latency observations, error reports) into a normalized
//! feedback stream. Each entry is scored on a 0.0–1.0 scale regardless of
//! its original source.
//!
//! ## Guarantees
//!
//! - **Thread-safe**: all operations protected by `Arc<Mutex<_>>`
//! - **Bounded**: entry history is capped at `max_entries` (FIFO eviction)
//! - **Normalized**: all scores are on a unified 0.0–1.0 scale
//! - **Non-blocking**: all operations are O(entries) worst-case
//!
//! ## NOT Responsible For
//!
//! - Acting on feedback (that is the router's or optimizer's job)
//! - Routing changes based on feedback
//! - Persisting feedback to durable storage

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Errors produced by the feedback collector.
#[derive(Error, Debug)]
pub enum FeedbackError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("feedback collector lock poisoned")]
    LockPoisoned,
}

/// Source of a feedback entry, indicating how the feedback was generated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FeedbackSource {
    /// Direct user rating (e.g., thumbs up/down, 1–5 stars).
    UserRating,
    /// Automated quality assessment from the quality estimator.
    AutoQuality,
    /// Latency observation from pipeline instrumentation.
    LatencyObservation,
    /// Error report from a failed request.
    ErrorReport,
    /// Custom feedback source with a descriptive label.
    Custom(String),
}

impl FeedbackSource {
    /// Return a string representation of the source for grouping.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn as_label(&self) -> String {
        match self {
            Self::UserRating => "user_rating".to_string(),
            Self::AutoQuality => "auto_quality".to_string(),
            Self::LatencyObservation => "latency_observation".to_string(),
            Self::ErrorReport => "error_report".to_string(),
            Self::Custom(label) => format!("custom:{}", label),
        }
    }
}

/// A single feedback entry with normalized score and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackEntry {
    /// Unique identifier for this feedback entry.
    pub id: String,
    /// Request ID that this feedback pertains to.
    pub request_id: String,
    /// Source of the feedback.
    pub source: FeedbackSource,
    /// Normalized score (0.0–1.0) where 1.0 is best.
    pub score: f64,
    /// Original raw value before normalization.
    pub raw_value: f64,
    /// Additional metadata key-value pairs.
    pub metadata: HashMap<String, String>,
    /// Unix timestamp in seconds when this feedback was recorded.
    pub timestamp_secs: u64,
}

/// Summary statistics computed from collected feedback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackSummary {
    /// Total number of feedback entries.
    pub total_entries: usize,
    /// Average score across all entries.
    pub avg_score: f64,
    /// Per-source statistics: source_label -> (count, avg_score).
    pub by_source: HashMap<String, (usize, f64)>,
}

/// Internal mutable state of the feedback collector.
#[derive(Debug)]
struct Inner {
    entries: VecDeque<FeedbackEntry>,
    max_entries: usize,
}

/// Feedback collector that ingests and unifies feedback from multiple sources.
///
/// Clone is cheap (`Arc`-based) and all clones share state.
#[derive(Debug, Clone)]
pub struct FeedbackCollector {
    inner: Arc<Mutex<Inner>>,
}

impl FeedbackCollector {
    /// Create a new feedback collector with the specified maximum entry count.
    ///
    /// # Arguments
    ///
    /// * `max_entries` — maximum number of feedback entries to retain (FIFO eviction)
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: VecDeque::new(),
                max_entries,
            })),
        }
    }

    /// Record a new feedback entry.
    ///
    /// The entry's score is clamped to `[0.0, 1.0]` before storage.
    /// If the collector is at capacity, the oldest entry is evicted.
    ///
    /// # Errors
    ///
    /// Returns [`FeedbackError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn record(&self, mut entry: FeedbackEntry) -> Result<(), FeedbackError> {
        entry.score = entry.score.max(0.0).min(1.0);

        let mut inner = self.inner.lock().map_err(|_| FeedbackError::LockPoisoned)?;
        inner.entries.push_back(entry);

        while inner.entries.len() > inner.max_entries {
            inner.entries.pop_front();
        }

        Ok(())
    }

    /// Compute a summary of all collected feedback.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn summary(&self) -> FeedbackSummary {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => {
                return FeedbackSummary {
                    total_entries: 0,
                    avg_score: 0.0,
                    by_source: HashMap::new(),
                }
            }
        };

        let total = inner.entries.len();
        if total == 0 {
            return FeedbackSummary {
                total_entries: 0,
                avg_score: 0.0,
                by_source: HashMap::new(),
            };
        }

        let mut total_score: f64 = 0.0;
        let mut by_source: HashMap<String, (usize, f64)> = HashMap::new();

        for entry in &inner.entries {
            total_score += entry.score;
            let label = entry.source.as_label();
            let source_stats = by_source.entry(label).or_insert((0, 0.0));
            source_stats.0 += 1;
            source_stats.1 += entry.score;
        }

        // Convert totals to averages
        for stats in by_source.values_mut() {
            if stats.0 > 0 {
                stats.1 /= stats.0 as f64;
            }
        }

        FeedbackSummary {
            total_entries: total,
            avg_score: total_score / total as f64,
            by_source,
        }
    }

    /// Return the most recent `n` feedback entries.
    ///
    /// If fewer than `n` entries exist, all are returned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn recent(&self, n: usize) -> Vec<FeedbackEntry> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let start = inner.entries.len().saturating_sub(n);
        inner.entries.iter().skip(start).cloned().collect()
    }

    /// Return all feedback entries for a specific request ID.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn by_request(&self, request_id: &str) -> Vec<FeedbackEntry> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        inner
            .entries
            .iter()
            .filter(|e| e.request_id == request_id)
            .cloned()
            .collect()
    }

    /// Return all feedback entries from a specific source.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn by_source(&self, source: &FeedbackSource) -> Vec<FeedbackEntry> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        inner
            .entries
            .iter()
            .filter(|e| e.source == *source)
            .cloned()
            .collect()
    }

    /// Return the average score across all entries.
    ///
    /// Returns 0.0 if no entries have been recorded.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn average_score(&self) -> f64 {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return 0.0,
        };
        if inner.entries.is_empty() {
            return 0.0;
        }
        let sum: f64 = inner.entries.iter().map(|e| e.score).sum();
        sum / inner.entries.len() as f64
    }

    /// Return the total number of feedback entries.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn entry_count(&self) -> usize {
        self.inner.lock().map(|g| g.entries.len()).unwrap_or(0)
    }

    /// Clear all feedback entries.
    ///
    /// # Errors
    ///
    /// Returns [`FeedbackError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    ///
    /// This function never panics.
    pub fn clear(&self) -> Result<(), FeedbackError> {
        let mut inner = self.inner.lock().map_err(|_| FeedbackError::LockPoisoned)?;
        inner.entries.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str, request_id: &str, source: FeedbackSource, score: f64) -> FeedbackEntry {
        FeedbackEntry {
            id: id.to_string(),
            request_id: request_id.to_string(),
            source,
            score,
            raw_value: score * 5.0,
            metadata: HashMap::new(),
            timestamp_secs: 1000,
        }
    }

    #[test]
    fn test_record_entry() {
        let collector = FeedbackCollector::new(100);
        let entry = make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8);
        collector.record(entry).unwrap();
        assert_eq!(collector.entry_count(), 1);
    }

    #[test]
    fn test_record_multiple_entries() {
        let collector = FeedbackCollector::new(100);
        for i in 0..10 {
            let entry = make_entry(
                &format!("fb-{}", i),
                &format!("req-{}", i),
                FeedbackSource::UserRating,
                0.5 + (i as f64 * 0.05),
            );
            collector.record(entry).unwrap();
        }
        assert_eq!(collector.entry_count(), 10);
    }

    #[test]
    fn test_record_clamps_score() {
        let collector = FeedbackCollector::new(100);
        let entry = make_entry("fb-1", "req-1", FeedbackSource::UserRating, 1.5);
        collector.record(entry).unwrap();
        let recent = collector.recent(1);
        assert!((recent[0].score - 1.0).abs() < f64::EPSILON);

        let entry2 = make_entry("fb-2", "req-2", FeedbackSource::UserRating, -0.5);
        collector.record(entry2).unwrap();
        let recent2 = collector.recent(1);
        assert!((recent2[0].score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_capped_entries() {
        let collector = FeedbackCollector::new(3);
        for i in 0..5 {
            let entry = make_entry(
                &format!("fb-{}", i),
                &format!("req-{}", i),
                FeedbackSource::UserRating,
                0.5,
            );
            collector.record(entry).unwrap();
        }
        assert_eq!(collector.entry_count(), 3);
        // Oldest entries should have been evicted
        let recent = collector.recent(10);
        assert_eq!(recent[0].id, "fb-2");
    }

    #[test]
    fn test_summary_empty() {
        let collector = FeedbackCollector::new(100);
        let summary = collector.summary();
        assert_eq!(summary.total_entries, 0);
        assert!((summary.avg_score - 0.0).abs() < f64::EPSILON);
        assert!(summary.by_source.is_empty());
    }

    #[test]
    fn test_summary_with_entries() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        collector
            .record(make_entry("fb-2", "req-2", FeedbackSource::UserRating, 0.6))
            .unwrap();
        collector
            .record(make_entry(
                "fb-3",
                "req-3",
                FeedbackSource::AutoQuality,
                0.9,
            ))
            .unwrap();

        let summary = collector.summary();
        assert_eq!(summary.total_entries, 3);
        assert!((summary.avg_score - (0.8 + 0.6 + 0.9) / 3.0).abs() < 0.01);

        let user_stats = summary.by_source.get("user_rating").unwrap();
        assert_eq!(user_stats.0, 2);
        assert!((user_stats.1 - 0.7).abs() < 0.01);

        let auto_stats = summary.by_source.get("auto_quality").unwrap();
        assert_eq!(auto_stats.0, 1);
        assert!((auto_stats.1 - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_recent_returns_latest() {
        let collector = FeedbackCollector::new(100);
        for i in 0..5 {
            collector
                .record(make_entry(
                    &format!("fb-{}", i),
                    &format!("req-{}", i),
                    FeedbackSource::UserRating,
                    0.5,
                ))
                .unwrap();
        }
        let recent = collector.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, "fb-3");
        assert_eq!(recent[1].id, "fb-4");
    }

    #[test]
    fn test_recent_fewer_than_requested() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        let recent = collector.recent(10);
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn test_by_request() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        collector
            .record(make_entry("fb-2", "req-2", FeedbackSource::UserRating, 0.6))
            .unwrap();
        collector
            .record(make_entry(
                "fb-3",
                "req-1",
                FeedbackSource::AutoQuality,
                0.9,
            ))
            .unwrap();

        let req1_entries = collector.by_request("req-1");
        assert_eq!(req1_entries.len(), 2);
        assert!(req1_entries.iter().all(|e| e.request_id == "req-1"));
    }

    #[test]
    fn test_by_request_no_matches() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        let entries = collector.by_request("nonexistent");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_by_source() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        collector
            .record(make_entry(
                "fb-2",
                "req-2",
                FeedbackSource::AutoQuality,
                0.6,
            ))
            .unwrap();
        collector
            .record(make_entry("fb-3", "req-3", FeedbackSource::UserRating, 0.9))
            .unwrap();

        let user_entries = collector.by_source(&FeedbackSource::UserRating);
        assert_eq!(user_entries.len(), 2);

        let auto_entries = collector.by_source(&FeedbackSource::AutoQuality);
        assert_eq!(auto_entries.len(), 1);

        let latency_entries = collector.by_source(&FeedbackSource::LatencyObservation);
        assert!(latency_entries.is_empty());
    }

    #[test]
    fn test_by_source_custom() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry(
                "fb-1",
                "req-1",
                FeedbackSource::Custom("manual".to_string()),
                0.7,
            ))
            .unwrap();
        let entries = collector.by_source(&FeedbackSource::Custom("manual".to_string()));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_average_score() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        collector
            .record(make_entry("fb-2", "req-2", FeedbackSource::UserRating, 0.6))
            .unwrap();
        let avg = collector.average_score();
        assert!((avg - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_average_score_empty() {
        let collector = FeedbackCollector::new(100);
        assert!((collector.average_score() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clear() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        collector
            .record(make_entry("fb-2", "req-2", FeedbackSource::UserRating, 0.6))
            .unwrap();
        collector.clear().unwrap();
        assert_eq!(collector.entry_count(), 0);
    }

    #[test]
    fn test_clone_shares_state() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        let clone = collector.clone();
        assert_eq!(clone.entry_count(), 1);

        clone
            .record(make_entry("fb-2", "req-2", FeedbackSource::UserRating, 0.6))
            .unwrap();
        assert_eq!(collector.entry_count(), 2);
    }

    #[test]
    fn test_feedback_source_labels() {
        assert_eq!(FeedbackSource::UserRating.as_label(), "user_rating");
        assert_eq!(FeedbackSource::AutoQuality.as_label(), "auto_quality");
        assert_eq!(
            FeedbackSource::LatencyObservation.as_label(),
            "latency_observation"
        );
        assert_eq!(FeedbackSource::ErrorReport.as_label(), "error_report");
        assert_eq!(
            FeedbackSource::Custom("my_source".to_string()).as_label(),
            "custom:my_source"
        );
    }

    #[test]
    fn test_feedback_entry_serialization() {
        let entry = make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8);
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: FeedbackEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "fb-1");
        assert!((deserialized.score - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_feedback_source_serialization() {
        let source = FeedbackSource::Custom("test".to_string());
        let json = serde_json::to_string(&source).unwrap();
        let deserialized: FeedbackSource = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, FeedbackSource::Custom("test".to_string()));
    }

    #[test]
    fn test_feedback_summary_serialization() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        let summary = collector.summary();
        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: FeedbackSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.total_entries, 1);
    }

    #[test]
    fn test_error_display() {
        let err = FeedbackError::LockPoisoned;
        assert!(err.to_string().contains("poisoned"));
    }

    #[test]
    fn test_entry_with_metadata() {
        let mut metadata = HashMap::new();
        metadata.insert("model".to_string(), "gpt-4".to_string());
        metadata.insert("region".to_string(), "us-east".to_string());

        let entry = FeedbackEntry {
            id: "fb-1".to_string(),
            request_id: "req-1".to_string(),
            source: FeedbackSource::UserRating,
            score: 0.9,
            raw_value: 4.5,
            metadata,
            timestamp_secs: 1700000000,
        };

        let collector = FeedbackCollector::new(100);
        collector.record(entry).unwrap();

        let recent = collector.recent(1);
        assert_eq!(recent[0].metadata.get("model"), Some(&"gpt-4".to_string()));
    }

    #[test]
    fn test_feedback_source_equality() {
        assert_eq!(FeedbackSource::UserRating, FeedbackSource::UserRating);
        assert_ne!(FeedbackSource::UserRating, FeedbackSource::AutoQuality);
        assert_eq!(
            FeedbackSource::Custom("a".to_string()),
            FeedbackSource::Custom("a".to_string())
        );
        assert_ne!(
            FeedbackSource::Custom("a".to_string()),
            FeedbackSource::Custom("b".to_string())
        );
    }

    #[test]
    fn test_error_report_source() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry(
                "fb-1",
                "req-1",
                FeedbackSource::ErrorReport,
                0.0,
            ))
            .unwrap();
        let entries = collector.by_source(&FeedbackSource::ErrorReport);
        assert_eq!(entries.len(), 1);
        assert!((entries[0].score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_mixed_sources_summary() {
        let collector = FeedbackCollector::new(100);
        collector
            .record(make_entry("fb-1", "req-1", FeedbackSource::UserRating, 0.8))
            .unwrap();
        collector
            .record(make_entry(
                "fb-2",
                "req-2",
                FeedbackSource::LatencyObservation,
                0.6,
            ))
            .unwrap();
        collector
            .record(make_entry(
                "fb-3",
                "req-3",
                FeedbackSource::ErrorReport,
                0.1,
            ))
            .unwrap();
        collector
            .record(make_entry(
                "fb-4",
                "req-4",
                FeedbackSource::Custom("manual".to_string()),
                0.9,
            ))
            .unwrap();

        let summary = collector.summary();
        assert_eq!(summary.total_entries, 4);
        assert_eq!(summary.by_source.len(), 4);
    }
}
