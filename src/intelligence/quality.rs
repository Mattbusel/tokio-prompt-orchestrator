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
#![cfg(feature = "intelligence")]

//! # Stage: Quality Estimator
//! Estimates output quality via heuristic signals: coherence, completeness,
//! and confidence. Detects per-backend regressions over time.

use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum QualityError {
    #[error("estimation failed: {0}")]
    EstimationFailed(String),
    #[error("insufficient data for quality estimation")]
    InsufficientData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityEstimatorConfig {
    pub min_response_len: usize,
    pub coherence_weight: f64,
    pub completeness_weight: f64,
    pub confidence_weight: f64,
    pub regression_window: usize,
}
impl Default for QualityEstimatorConfig {
    fn default() -> Self {
        Self {
            min_response_len: 10,
            coherence_weight: 0.4,
            completeness_weight: 0.4,
            confidence_weight: 0.2,
            regression_window: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityEstimate {
    pub coherence_score: f64,
    pub completeness_score: f64,
    pub confidence_score: f64,
    pub overall_score: f64,
    pub flags: Vec<String>,
    pub estimated_at_ms: u64,
}

#[derive(Debug, Clone)]
pub struct QualityRecord {
    pub prompt_hash: u64,
    pub response_len: usize,
    pub estimate: QualityEstimate,
    pub backend: String,
}

pub struct QualityEstimator {
    history: Arc<Mutex<VecDeque<QualityRecord>>>,
    config: QualityEstimatorConfig,
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl QualityEstimator {
    pub fn new(config: QualityEstimatorConfig) -> Self {
        Self {
            history: Arc::new(Mutex::new(VecDeque::new())),
            config,
        }
    }

    pub fn estimate(
        &self,
        prompt: &str,
        response: &str,
        backend: &str,
    ) -> Result<QualityEstimate, QualityError> {
        // Coherence: length check + sentence ending punctuation
        let len_ok = response.len() >= self.config.min_response_len;
        let ends_with_punct = response.trim_end().ends_with('.')
            || response.trim_end().ends_with('!')
            || response.trim_end().ends_with('?');
        let coherence_score = if !len_ok {
            0.0
        } else if ends_with_punct {
            0.85
        } else {
            0.5
        };

        // Completeness: check if response addresses question words from prompt
        let question_words = ["how", "what", "why", "when", "where", "who", "which"];
        let prompt_lower = prompt.to_lowercase();
        let question_count = question_words
            .iter()
            .filter(|w| prompt_lower.contains(**w))
            .count();
        let resp_lower = response.to_lowercase();
        let addressed = question_words
            .iter()
            .filter(|w| prompt_lower.contains(**w) && resp_lower.contains(**w))
            .count();
        let completeness_score = if question_count == 0 {
            0.7
        } else {
            (addressed as f64 / question_count as f64).min(1.0)
        };

        // Confidence: longer responses get higher confidence score
        let confidence_score = 1.0 - (1.0 / (response.len() as f64).sqrt().max(1.0));

        let overall_score = self.config.coherence_weight * coherence_score
            + self.config.completeness_weight * completeness_score
            + self.config.confidence_weight * confidence_score;

        let mut flags = Vec::new();
        if coherence_score < 0.4 {
            flags.push("low_coherence".to_string());
        }
        if completeness_score < 0.4 {
            flags.push("possibly_incomplete".to_string());
        }
        if response.len() < self.config.min_response_len {
            flags.push("too_short".to_string());
        }

        let estimate = QualityEstimate {
            coherence_score,
            completeness_score,
            confidence_score,
            overall_score,
            flags,
            estimated_at_ms: now_ms(),
        };

        let record = QualityRecord {
            prompt_hash: hash_str(prompt),
            response_len: response.len(),
            estimate: estimate.clone(),
            backend: backend.to_string(),
        };
        match self.history.lock() {
            Ok(mut guard) => {
                guard.push_back(record);
                let window = self.config.regression_window;
                if guard.len() > window * 2 {
                    guard.pop_front();
                }
            }
            Err(e) => warn!(error = %e, "quality history lock poisoned"),
        }
        debug!(backend, overall = overall_score, "quality estimated");
        Ok(estimate)
    }

    pub fn detect_regression(&self, backend: &str) -> bool {
        let guard = match self.history.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let backend_records: Vec<f64> = guard
            .iter()
            .filter(|r| r.backend == backend)
            .map(|r| r.estimate.overall_score)
            .collect();
        if backend_records.len() < 10 {
            return false;
        }
        let n = backend_records.len();
        let recent_avg = backend_records[n.saturating_sub(10)..].iter().sum::<f64>() / 10.0;
        let overall_avg = backend_records.iter().sum::<f64>() / n as f64;
        overall_avg > 0.0 && recent_avg < overall_avg * 0.85
    }

    pub fn backend_avg_quality(&self, backend: &str) -> Option<f64> {
        let guard = self.history.lock().ok()?;
        let scores: Vec<f64> = guard
            .iter()
            .filter(|r| r.backend == backend)
            .map(|r| r.estimate.overall_score)
            .collect();
        if scores.is_empty() {
            return None;
        }
        Some(scores.iter().sum::<f64>() / scores.len() as f64)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn estimator() -> QualityEstimator {
        QualityEstimator::new(QualityEstimatorConfig::default())
    }

    #[test]
    fn test_default_min_response_len_is_10() {
        assert_eq!(QualityEstimatorConfig::default().min_response_len, 10);
    }

    #[test]
    fn test_short_response_flags_too_short() {
        let e = estimator();
        let est = e.estimate("q", "hi", "local").unwrap();
        assert!(est.flags.contains(&"too_short".to_string()));
    }

    #[test]
    fn test_response_with_punctuation_has_high_coherence() {
        let e = estimator();
        let resp = "This is a well-formed response sentence.";
        let est = e.estimate("question?", resp, "local").unwrap();
        assert!(est.coherence_score >= 0.8);
    }

    #[test]
    fn test_response_without_punctuation_lower_coherence() {
        let e = estimator();
        let resp = "some answer without ending punctuation here ok";
        let est = e.estimate("what is x", resp, "cloud").unwrap();
        assert!(est.coherence_score < 0.8);
    }

    #[test]
    fn test_overall_score_in_unit_range() {
        let e = estimator();
        let est = e
            .estimate(
                "how does it work?",
                "It works by doing the thing properly.",
                "local",
            )
            .unwrap();
        assert!(est.overall_score >= 0.0 && est.overall_score <= 1.0);
    }

    #[test]
    fn test_question_words_addressed_increases_completeness() {
        let e = estimator();
        let est = e
            .estimate(
                "what is this?",
                "This is what you need to know about it.",
                "local",
            )
            .unwrap();
        assert!(est.completeness_score > 0.0);
    }

    #[test]
    fn test_no_question_words_gives_default_completeness() {
        let e = estimator();
        let est = e
            .estimate(
                "summarize this text",
                "The text covers many topics in detail.",
                "local",
            )
            .unwrap();
        assert!((est.completeness_score - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_confidence_increases_with_response_length() {
        let e = estimator();
        let short_est = e.estimate("q", "short answer here.", "local").unwrap();
        let long_resp = "a".repeat(500) + ".";
        let long_est = e.estimate("q", &long_resp, "local").unwrap();
        assert!(long_est.confidence_score > short_est.confidence_score);
    }

    #[test]
    fn test_estimate_records_to_history() {
        let e = estimator();
        e.estimate("q", "a longer response text here.", "local")
            .unwrap();
        let g = e.history.lock().unwrap();
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn test_backend_avg_quality_none_for_unknown_backend() {
        let e = estimator();
        assert!(e.backend_avg_quality("ghost").is_none());
    }

    #[test]
    fn test_backend_avg_quality_computed() {
        let e = estimator();
        e.estimate("how?", "Because it works this way for everyone.", "cloud")
            .unwrap();
        e.estimate("why?", "Due to the underlying mechanism here.", "cloud")
            .unwrap();
        assert!(e.backend_avg_quality("cloud").unwrap() > 0.0);
    }

    #[test]
    fn test_detect_regression_false_with_insufficient_data() {
        let e = estimator();
        e.estimate("q", "a fine answer here for sure.", "local")
            .unwrap();
        assert!(!e.detect_regression("local"));
    }

    #[test]
    fn test_low_coherence_flag_present_for_short_response() {
        let e = estimator();
        let est = e.estimate("x", "no", "local").unwrap();
        assert!(est
            .flags
            .iter()
            .any(|f| f == "too_short" || f == "low_coherence"));
    }
}
