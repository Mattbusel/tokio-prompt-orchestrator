//! Reinforcement-learning-style feedback collector and reward modeler.
//!
//! This module records per-request feedback, computes composite reward signals,
//! and maintains a running weighted average reward per (model, prompt_template)
//! pair. The reward weight decays exponentially with time so that recent
//! observations matter more than old ones.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A single feedback record attached to one request/response pair.
#[derive(Debug, Clone)]
pub struct Feedback {
    /// Identifier of the originating request.
    pub request_id: String,
    /// Identifier of the response that was evaluated.
    pub response_id: String,
    /// Human or automated quality rating in [0, 1].
    pub rating: f64,
    /// End-to-end latency experienced by the caller, in milliseconds.
    pub latency_ms: u64,
    /// Total tokens consumed by the request/response pair.
    pub tokens_used: u64,
    /// Unix epoch seconds at which the feedback was recorded.
    pub timestamp: u64,
}

/// Composite reward derived from a single [`Feedback`] record.
///
/// Formula: `quality_score * 0.5 - cost_penalty * 0.3 - latency_penalty * 0.2`
#[derive(Debug, Clone)]
pub struct RewardSignal {
    /// Raw quality score in [0, 1] (equal to `feedback.rating`).
    pub quality_score: f64,
    /// Normalised cost penalty proportional to tokens used (range [0, 1]).
    pub cost_penalty: f64,
    /// Normalised latency penalty (range [0, 1]).
    pub latency_penalty: f64,
    /// Final composite reward value.
    pub reward: f64,
}

impl RewardSignal {
    /// Compute a [`RewardSignal`] from a raw [`Feedback`] record.
    ///
    /// `max_tokens` and `max_latency_ms` are used to normalise the penalties
    /// into [0, 1].  If either is zero the corresponding penalty is 0.
    pub fn from_feedback(feedback: &Feedback, max_tokens: u64, max_latency_ms: u64) -> Self {
        let quality_score = feedback.rating.clamp(0.0, 1.0);
        let cost_penalty = if max_tokens == 0 {
            0.0
        } else {
            (feedback.tokens_used as f64 / max_tokens as f64).clamp(0.0, 1.0)
        };
        let latency_penalty = if max_latency_ms == 0 {
            0.0
        } else {
            (feedback.latency_ms as f64 / max_latency_ms as f64).clamp(0.0, 1.0)
        };
        let reward = quality_score * 0.5 - cost_penalty * 0.3 - latency_penalty * 0.2;
        Self {
            quality_score,
            cost_penalty,
            latency_penalty,
            reward,
        }
    }
}

/// Thread-safe ring buffer of the last `capacity` feedbacks per model variant.
#[derive(Debug, Clone)]
pub struct FeedbackStore {
    inner: Arc<Mutex<FeedbackStoreInner>>,
}

#[derive(Debug)]
struct FeedbackStoreInner {
    capacity: usize,
    /// variant -> circular buffer of feedbacks
    data: HashMap<String, Vec<Feedback>>,
    /// variant -> write index (next position to overwrite)
    heads: HashMap<String, usize>,
}

impl FeedbackStore {
    /// Create a new store that retains the last `capacity` feedbacks per variant.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FeedbackStoreInner {
                capacity,
                data: HashMap::new(),
                heads: HashMap::new(),
            })),
        }
    }

    /// Append a feedback for `variant`.  Overwrites the oldest entry when full.
    pub fn push(&self, variant: &str, feedback: Feedback) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let cap = guard.capacity;
        let key = variant.to_string();
        // Ensure both maps have an entry.
        guard.data.entry(key.clone()).or_insert_with(Vec::new);
        guard.heads.entry(key.clone()).or_insert(0);
        // Read the current head index without holding a mutable borrow on guard.
        let current_head = *guard.heads.get(&key).expect("just inserted");
        let buf = guard.data.get_mut(&key).expect("just inserted");
        if buf.len() < cap {
            buf.push(feedback);
        } else {
            buf[current_head] = feedback;
            let next_head = (current_head + 1) % cap;
            *guard.heads.get_mut(&key).expect("just inserted") = next_head;
        }
    }

    /// Return a snapshot of all feedbacks for `variant` in insertion order.
    pub fn get(&self, variant: &str) -> Vec<Feedback> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.data.get(variant).cloned().unwrap_or_default()
    }

    /// Return all known variant names.
    pub fn variants(&self) -> Vec<String> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.data.keys().cloned().collect()
    }
}

/// Running weighted-average reward per `(model, prompt_template)` key.
///
/// Each new reward is blended with the running average using an exponential
/// decay factor: `avg = decay * avg + (1 - decay) * new_reward`.
#[derive(Debug, Clone)]
pub struct RewardModel {
    inner: Arc<Mutex<RewardModelInner>>,
}

#[derive(Debug)]
struct RewardModelInner {
    /// Exponential smoothing factor in (0, 1).  Closer to 1 = slower decay.
    decay: f64,
    /// (model, template) -> (weighted_avg, history)
    averages: HashMap<(String, String), (f64, Vec<f64>)>,
}

impl RewardModel {
    /// Create a new model with the given exponential decay factor.
    ///
    /// `decay` must be in (0, 1).  A value of 0.9 means older observations
    /// retain 90% of their weight after each update.
    pub fn new(decay: f64) -> Self {
        let decay = decay.clamp(1e-6, 1.0 - 1e-6);
        Self {
            inner: Arc::new(Mutex::new(RewardModelInner {
                decay,
                averages: HashMap::new(),
            })),
        }
    }

    /// Update the running average for `(model, template)` with a new reward.
    pub fn update(&self, model: &str, template: &str, reward: f64) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let decay = guard.decay;
        let key = (model.to_string(), template.to_string());
        let entry = guard.averages.entry(key).or_insert((reward, Vec::new()));
        entry.0 = decay * entry.0 + (1.0 - decay) * reward;
        entry.1.push(reward);
    }

    /// Return the current weighted-average reward for `(model, template)`.
    pub fn average(&self, model: &str, template: &str) -> Option<f64> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .averages
            .get(&(model.to_string(), template.to_string()))
            .map(|(avg, _)| *avg)
    }

    /// Return the full chronological reward history for `(model, template)`.
    pub fn history(&self, model: &str, template: &str) -> Vec<f64> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .averages
            .get(&(model.to_string(), template.to_string()))
            .map(|(_, h)| h.clone())
            .unwrap_or_default()
    }
}

/// Closed-loop feedback collector that ties together the store and reward model.
#[derive(Debug, Clone)]
pub struct FeedbackLoop {
    store: FeedbackStore,
    reward_model: RewardModel,
    /// Normalisation ceiling for token counts.
    max_tokens: u64,
    /// Normalisation ceiling for latency.
    max_latency_ms: u64,
}

impl FeedbackLoop {
    /// Create a new `FeedbackLoop`.
    ///
    /// - `ring_capacity`: number of feedbacks retained per variant in the ring buffer.
    /// - `decay`: exponential smoothing factor for the reward model (0 < decay < 1).
    /// - `max_tokens`: token ceiling used when normalising cost penalties.
    /// - `max_latency_ms`: latency ceiling used when normalising latency penalties.
    pub fn new(ring_capacity: usize, decay: f64, max_tokens: u64, max_latency_ms: u64) -> Self {
        Self {
            store: FeedbackStore::new(ring_capacity),
            reward_model: RewardModel::new(decay),
            max_tokens,
            max_latency_ms,
        }
    }

    /// Record a feedback observation.
    ///
    /// The variant key is derived from `request_id` as the model name prefix
    /// (everything before the first `':'`).  Callers should embed the model
    /// name in `request_id` as `"model:rest-of-id"`.  If no `':'` is found the
    /// whole `request_id` is used as the variant key.
    pub fn record(&self, feedback: Feedback) {
        let variant = feedback
            .request_id
            .split(':')
            .next()
            .unwrap_or(&feedback.request_id)
            .to_string();
        let signal = RewardSignal::from_feedback(&feedback, self.max_tokens, self.max_latency_ms);
        self.store.push(&variant, feedback);
        self.reward_model.update(&variant, "default", signal.reward);
    }

    /// Return the variant (from `model_options`) with the highest average reward.
    ///
    /// Returns `None` if none of the provided options have any recorded rewards.
    pub fn best_variant(&self, model_options: &[String]) -> Option<String> {
        model_options
            .iter()
            .filter_map(|m| {
                self.reward_model
                    .average(m, "default")
                    .map(|avg| (m.clone(), avg))
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(m, _)| m)
    }

    /// Return the chronological reward history for a given variant.
    pub fn reward_history(&self, variant: &str) -> Vec<f64> {
        self.reward_model.history(variant, "default")
    }

    /// Compute a convergence score as the variance of the most recent rewards
    /// across all known variants.
    ///
    /// A low value (close to 0) indicates that rewards have stabilised.
    pub fn convergence_score(&self) -> f64 {
        let variants = self.store.variants();
        if variants.is_empty() {
            return 0.0;
        }
        let averages: Vec<f64> = variants
            .iter()
            .filter_map(|v| self.reward_model.average(v, "default"))
            .collect();
        if averages.len() < 2 {
            return 0.0;
        }
        let mean = averages.iter().sum::<f64>() / averages.len() as f64;
        let variance = averages.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
            / averages.len() as f64;
        variance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_feedback(request_id: &str, rating: f64, latency_ms: u64, tokens_used: u64) -> Feedback {
        Feedback {
            request_id: request_id.to_string(),
            response_id: format!("resp-{}", request_id),
            rating,
            latency_ms,
            tokens_used,
            timestamp: 1_000_000,
        }
    }

    #[test]
    fn reward_signal_formula() {
        let fb = make_feedback("m", 1.0, 500, 1000);
        let sig = RewardSignal::from_feedback(&fb, 2000, 2000);
        // quality=1.0, cost_penalty=0.5, latency_penalty=0.25
        let expected = 1.0 * 0.5 - 0.5 * 0.3 - 0.25 * 0.2;
        assert!((sig.reward - expected).abs() < 1e-9);
    }

    #[test]
    fn reward_signal_zero_max() {
        let fb = make_feedback("m", 0.8, 100, 100);
        let sig = RewardSignal::from_feedback(&fb, 0, 0);
        // penalties should both be 0
        let expected = 0.8 * 0.5;
        assert!((sig.reward - expected).abs() < 1e-9);
    }

    #[test]
    fn feedback_store_ring_buffer() {
        let store = FeedbackStore::new(3);
        for i in 0u64..5 {
            store.push("m", make_feedback("m", 0.5, i * 10, i * 100));
        }
        // Should retain exactly 3 entries
        assert_eq!(store.get("m").len(), 3);
    }

    #[test]
    fn reward_model_weighted_average() {
        let model = RewardModel::new(0.9);
        model.update("gpt4", "default", 1.0);
        model.update("gpt4", "default", 0.0);
        let avg = model.average("gpt4", "default").expect("should have avg");
        // After two updates: first avg=1.0, second avg=0.9*1.0 + 0.1*0.0 = 0.9
        assert!((avg - 0.9).abs() < 1e-9);
    }

    #[test]
    fn reward_model_history_length() {
        let model = RewardModel::new(0.8);
        model.update("claude", "default", 0.5);
        model.update("claude", "default", 0.7);
        model.update("claude", "default", 0.6);
        assert_eq!(model.history("claude", "default").len(), 3);
    }

    #[test]
    fn feedback_loop_best_variant() {
        let fl = FeedbackLoop::new(10, 0.5, 10_000, 5_000);
        // record high-reward feedbacks under "good_model"
        for _ in 0..5 {
            fl.record(make_feedback("good_model:req1", 1.0, 100, 100));
        }
        // record low-reward feedbacks under "bad_model"
        for _ in 0..5 {
            fl.record(make_feedback("bad_model:req1", 0.1, 4000, 9000));
        }
        let options = vec!["good_model".to_string(), "bad_model".to_string()];
        let best = fl.best_variant(&options).expect("should find best");
        assert_eq!(best, "good_model");
    }

    #[test]
    fn feedback_loop_reward_history() {
        let fl = FeedbackLoop::new(10, 0.5, 10_000, 5_000);
        fl.record(make_feedback("modelA:r1", 0.8, 200, 500));
        fl.record(make_feedback("modelA:r2", 0.9, 300, 600));
        let h = fl.reward_history("modelA");
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn feedback_loop_convergence_score_single_variant() {
        let fl = FeedbackLoop::new(10, 0.5, 10_000, 5_000);
        fl.record(make_feedback("only:req", 0.5, 500, 500));
        // Single variant — variance is 0
        assert_eq!(fl.convergence_score(), 0.0);
    }

    #[test]
    fn feedback_loop_convergence_score_two_variants() {
        let fl = FeedbackLoop::new(10, 0.5, 10_000, 5_000);
        fl.record(make_feedback("alpha:req", 1.0, 100, 100));
        fl.record(make_feedback("beta:req", 0.0, 100, 100));
        // Two very different variants → variance > 0
        assert!(fl.convergence_score() > 0.0);
    }

    #[test]
    fn best_variant_returns_none_when_no_data() {
        let fl = FeedbackLoop::new(10, 0.5, 10_000, 5_000);
        let options = vec!["x".to_string(), "y".to_string()];
        assert!(fl.best_variant(&options).is_none());
    }
}
