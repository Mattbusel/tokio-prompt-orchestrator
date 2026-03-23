//! Model fallback chains for resilience — automatic failover across model tiers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Errors that can be returned by a model inference call.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelError {
    /// HTTP 429 / token-bucket exhausted.
    RateLimit,
    /// Prompt + completion would exceed the model's context window.
    ContextTooLong,
    /// The request itself was malformed (bad parameters, etc.).
    InvalidRequest(String),
    /// The provider returned a 5xx or equivalent error.
    ServerError(String),
    /// No response received within the allowed time window.
    Timeout,
    /// The model endpoint is temporarily unavailable.
    Unavailable,
}

impl ModelError {
    /// Returns `true` for errors that are worth retrying on the *same* model.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Timeout | Self::ServerError(_))
    }

    /// Returns `true` for errors that should trigger a move to the next model in the chain.
    pub fn suggests_fallback(&self) -> bool {
        matches!(self, Self::RateLimit | Self::ContextTooLong | Self::Unavailable)
    }
}

/// A single model in a fallback chain.
#[derive(Debug, Clone)]
pub struct FallbackModel {
    /// Provider-specific model identifier (e.g. `"gpt-4o"`, `"claude-3-5-sonnet-20241022"`).
    pub model_id: String,
    /// Lower priority value = preferred (0 is highest).
    pub priority: u8,
    /// Maximum context tokens supported by this model.
    pub max_context_tokens: usize,
    /// Estimated cost per 1 000 tokens (blended input+output).
    pub cost_per_1k_tokens: f64,
    /// Whether this model supports tool/function calling.
    pub supports_tools: bool,
    /// Whether this model supports streaming.
    pub supports_streaming: bool,
}

/// Report about a single model in the chain.
#[derive(Debug, Clone)]
pub struct FallbackReport {
    /// Model identifier.
    pub model_id: String,
    /// Chain priority.
    pub priority: u8,
    /// Consecutive failure count since last success.
    pub failures: u32,
    /// Whether the model is currently in its cooldown window.
    pub in_cooldown: bool,
    /// Seconds remaining on the cooldown, if applicable.
    pub cooldown_remaining_secs: Option<u64>,
}

/// Ordered list of models with failure tracking and cooldown management.
pub struct FallbackChain {
    models: Vec<FallbackModel>,
    current_idx: usize,
    failure_counts: HashMap<String, u32>,
    cooldown_until: HashMap<String, Instant>,
}

impl FallbackChain {
    /// Build a new chain from the supplied models, sorted by ascending priority.
    pub fn new(mut models: Vec<FallbackModel>) -> Self {
        models.sort_by_key(|m| m.priority);
        Self {
            models,
            current_idx: 0,
            failure_counts: HashMap::new(),
            cooldown_until: HashMap::new(),
        }
    }

    /// Find the next available model that meets the given constraints.
    ///
    /// Skips models that:
    /// - Are currently within their cooldown window
    /// - Have accumulated more than 3 consecutive failures
    /// - Do not support tools when `require_tools` is `true`
    /// - Have a context window smaller than `min_context`
    pub fn next_available(&self, require_tools: bool, min_context: usize) -> Option<&FallbackModel> {
        let now = Instant::now();
        for model in &self.models {
            // Cooldown check
            if let Some(&until) = self.cooldown_until.get(&model.model_id) {
                if now < until {
                    continue;
                }
            }
            // Failure threshold
            if self.failure_counts.get(&model.model_id).copied().unwrap_or(0) > 3 {
                continue;
            }
            // Capability checks
            if require_tools && !model.supports_tools {
                continue;
            }
            if model.max_context_tokens < min_context {
                continue;
            }
            return Some(model);
        }
        None
    }

    /// Record a failure for `model_id`.
    ///
    /// Applies a 60-second cooldown for [`ModelError::RateLimit`] errors.
    pub fn record_failure(&mut self, model_id: &str, error: &ModelError) {
        let count = self.failure_counts.entry(model_id.to_string()).or_insert(0);
        *count += 1;

        if matches!(error, ModelError::RateLimit) {
            self.cooldown_until
                .insert(model_id.to_string(), Instant::now() + Duration::from_secs(60));
        }
    }

    /// Record a successful call for `model_id`, resetting its failure counter.
    pub fn record_success(&mut self, model_id: &str) {
        self.failure_counts.remove(model_id);
        self.cooldown_until.remove(model_id);
        // Advance current index to point at this model for future fast-path.
        if let Some(idx) = self.models.iter().position(|m| m.model_id == model_id) {
            self.current_idx = idx;
        }
    }

    /// Manually clear the cooldown for `model_id`.
    pub fn reset_cooldown(&mut self, model_id: &str) {
        self.cooldown_until.remove(model_id);
    }

    /// Generate a status report for every model in the chain.
    pub fn chain_report(&self) -> Vec<FallbackReport> {
        let now = Instant::now();
        self.models
            .iter()
            .map(|m| {
                let failures = self.failure_counts.get(&m.model_id).copied().unwrap_or(0);
                let cooldown_until = self.cooldown_until.get(&m.model_id).copied();
                let in_cooldown = cooldown_until.map(|u| now < u).unwrap_or(false);
                let cooldown_remaining_secs = cooldown_until.and_then(|u| {
                    if now < u {
                        Some(u.duration_since(now).as_secs())
                    } else {
                        None
                    }
                });
                FallbackReport {
                    model_id: m.model_id.clone(),
                    priority: m.priority,
                    failures,
                    in_cooldown,
                    cooldown_remaining_secs,
                }
            })
            .collect()
    }

    /// Number of models in the chain.
    pub fn len(&self) -> usize {
        self.models.len()
    }

    /// Returns `true` if the chain contains no models.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// Thread-safe wrapper around [`FallbackChain`] that executes closures against
/// the best available model, retrying down the chain on failure.
pub struct FallbackManager {
    chain: Arc<Mutex<FallbackChain>>,
}

impl FallbackManager {
    /// Create a new manager wrapping the given chain.
    pub fn new(chain: FallbackChain) -> Self {
        Self {
            chain: Arc::new(Mutex::new(chain)),
        }
    }

    /// Execute `f` against the best available model.
    ///
    /// Walks the chain until a model succeeds or all models are exhausted.
    ///
    /// - `require_tools`: skip models that do not support tool/function calling.
    /// - `context_size`: skip models whose context window is too small.
    pub fn execute<F, T>(
        &self,
        f: F,
        require_tools: bool,
        context_size: usize,
    ) -> Result<T, String>
    where
        F: Fn(&str) -> Result<T, ModelError>,
    {
        // Collect candidate model IDs up-front to avoid holding the lock during `f`.
        let candidates: Vec<String> = {
            let chain = self.chain.lock().map_err(|e| format!("lock poisoned: {}", e))?;
            chain
                .models
                .iter()
                .filter(|m| {
                    let now = Instant::now();
                    let in_cooldown = chain
                        .cooldown_until
                        .get(&m.model_id)
                        .map(|&u| now < u)
                        .unwrap_or(false);
                    let failures = chain.failure_counts.get(&m.model_id).copied().unwrap_or(0);
                    !in_cooldown
                        && failures <= 3
                        && (!require_tools || m.supports_tools)
                        && m.max_context_tokens >= context_size
                })
                .map(|m| m.model_id.clone())
                .collect()
        };

        if candidates.is_empty() {
            return Err("No available models in fallback chain".to_string());
        }

        for model_id in &candidates {
            match f(model_id) {
                Ok(result) => {
                    if let Ok(mut chain) = self.chain.lock() {
                        chain.record_success(model_id);
                    }
                    return Ok(result);
                }
                Err(err) => {
                    if let Ok(mut chain) = self.chain.lock() {
                        chain.record_failure(model_id, &err);
                    }
                    if !err.suggests_fallback() && !err.is_retryable() {
                        return Err(format!("Non-recoverable error on {}: {:?}", model_id, err));
                    }
                    // Continue to next candidate
                }
            }
        }

        Err("All models in fallback chain failed".to_string())
    }

    /// Borrow a clone of the underlying chain handle for inspection.
    pub fn chain(&self) -> Arc<Mutex<FallbackChain>> {
        Arc::clone(&self.chain)
    }
}
