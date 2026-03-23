//! # Model Registry
//!
//! Tracks LLM model metadata, capabilities, pricing, and rate limits.
//! Provides routing hints and deprecation warnings.

use dashmap::DashMap;
use std::fmt;

// ── ModelCapability ───────────────────────────────────────────────────────────

/// A capability a model may support.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ModelCapability {
    /// Free-form text generation.
    TextGeneration,
    /// Code generation and completion.
    CodeGeneration,
    /// Structured function / tool calling.
    FunctionCalling,
    /// Accepts image inputs.
    ImageUnderstanding,
    /// Produces dense vector embeddings.
    Embedding,
    /// Condensed document summarization.
    Summarization,
    /// Language translation.
    Translation,
    /// Multi-class text classification.
    Classification,
}

impl fmt::Display for ModelCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::TextGeneration => "TextGeneration",
            Self::CodeGeneration => "CodeGeneration",
            Self::FunctionCalling => "FunctionCalling",
            Self::ImageUnderstanding => "ImageUnderstanding",
            Self::Embedding => "Embedding",
            Self::Summarization => "Summarization",
            Self::Translation => "Translation",
            Self::Classification => "Classification",
        };
        write!(f, "{}", s)
    }
}

// ── ModelTier ─────────────────────────────────────────────────────────────────

/// Tier classification for a model, reflecting quality and price.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModelTier {
    /// Cheapest, lowest capability.
    Economy,
    /// Balanced cost and capability.
    Standard,
    /// High capability, higher cost.
    Advanced,
    /// Flagship quality.
    Premium,
}

impl ModelTier {
    /// Relative pricing weight for budget estimation (Economy=1.0 … Premium=8.0).
    pub fn pricing_weight(&self) -> f64 {
        match self {
            Self::Economy => 1.0,
            Self::Standard => 2.5,
            Self::Advanced => 5.0,
            Self::Premium => 8.0,
        }
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Economy => "Economy",
            Self::Standard => "Standard",
            Self::Advanced => "Advanced",
            Self::Premium => "Premium",
        };
        write!(f, "{}", s)
    }
}

// ── ModelMetadata ─────────────────────────────────────────────────────────────

/// Rich metadata for a single LLM model.
#[derive(Debug, Clone)]
pub struct ModelMetadata {
    /// Unique model identifier (e.g. "gpt-4o").
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Provider name (e.g. "OpenAI").
    pub provider: String,
    /// Quality/price tier.
    pub tier: ModelTier,
    /// Maximum context window in tokens.
    pub context_window: usize,
    /// Maximum tokens the model can output in one call.
    pub max_output_tokens: usize,
    /// Supported capabilities.
    pub capabilities: Vec<ModelCapability>,
    /// Cost per 1 000 input tokens in USD.
    pub cost_per_1k_input_tokens: f64,
    /// Cost per 1 000 output tokens in USD.
    pub cost_per_1k_output_tokens: f64,
    /// API rate limit: requests per minute.
    pub requests_per_minute: u32,
    /// API rate limit: tokens per minute.
    pub tokens_per_minute: u32,
    /// Whether the model supports streaming responses.
    pub supports_streaming: bool,
    /// Whether the model accepts a system prompt.
    pub supports_system_prompt: bool,
    /// Whether this model is deprecated.
    pub deprecated: bool,
    /// ID of the recommended successor if deprecated.
    pub successor: Option<String>,
}

// ── RoutingHint ───────────────────────────────────────────────────────────────

/// Routing suggestion produced by [`ModelRegistry::suggest_routing`].
#[derive(Debug, Clone)]
pub struct RoutingHint {
    /// Recommended model ID.
    pub preferred_model: String,
    /// Ordered list of fallback model IDs.
    pub fallback_models: Vec<String>,
    /// Human-readable rationale.
    pub reason: String,
    /// Confidence in [0.0, 1.0].
    pub confidence: f64,
}

// ── ModelRegistry ─────────────────────────────────────────────────────────────

/// Concurrent registry of [`ModelMetadata`] keyed by model ID.
pub struct ModelRegistry {
    models: DashMap<String, ModelMetadata>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self {
            models: DashMap::new(),
        }
    }
}

impl ModelRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register or replace a model's metadata.
    pub fn register(&self, model: ModelMetadata) {
        self.models.insert(model.id.clone(), model);
    }

    /// Look up a model by ID.
    pub fn get(&self, model_id: &str) -> Option<ModelMetadata> {
        self.models.get(model_id).map(|r| r.clone())
    }

    /// Return all registered models.
    pub fn all_models(&self) -> Vec<ModelMetadata> {
        self.models.iter().map(|r| r.clone()).collect()
    }

    /// Return non-deprecated models that have `cap`, sorted by cheapest input cost first.
    pub fn models_with_capability(&self, cap: &ModelCapability) -> Vec<ModelMetadata> {
        let mut models: Vec<ModelMetadata> = self
            .models
            .iter()
            .filter(|r| !r.deprecated && r.capabilities.contains(cap))
            .map(|r| r.clone())
            .collect();
        models.sort_by(|a, b| {
            a.cost_per_1k_input_tokens
                .partial_cmp(&b.cost_per_1k_input_tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        models
    }

    /// Return the cheapest model that has `cap` and fits within `min_context` tokens.
    pub fn cheapest_for_capability(
        &self,
        cap: &ModelCapability,
        min_context: usize,
    ) -> Option<ModelMetadata> {
        self.models_with_capability(cap)
            .into_iter()
            .find(|m| m.context_window >= min_context)
    }

    /// Return the highest-tier model that has `cap` and fits within `min_context` tokens.
    pub fn best_for_capability(
        &self,
        cap: &ModelCapability,
        min_context: usize,
    ) -> Option<ModelMetadata> {
        let mut candidates: Vec<ModelMetadata> = self
            .models_with_capability(cap)
            .into_iter()
            .filter(|m| m.context_window >= min_context)
            .collect();
        candidates.sort_by(|a, b| b.tier.cmp(&a.tier));
        candidates.into_iter().next()
    }

    /// Suggest a routing strategy given required capabilities, context size, and optional budget.
    pub fn suggest_routing(
        &self,
        caps: &[ModelCapability],
        context_size: usize,
        budget_usd: Option<f64>,
    ) -> RoutingHint {
        // Collect candidates satisfying all caps.
        let mut candidates: Vec<ModelMetadata> = self
            .models
            .iter()
            .filter(|r| {
                !r.deprecated
                    && r.context_window >= context_size
                    && caps.iter().all(|c| r.capabilities.contains(c))
            })
            .map(|r| r.clone())
            .collect();

        if candidates.is_empty() {
            return RoutingHint {
                preferred_model: String::new(),
                fallback_models: vec![],
                reason: "No models satisfy all requested capabilities".to_string(),
                confidence: 0.0,
            };
        }

        // Apply budget filter if provided.
        if let Some(budget) = budget_usd {
            let budget_filtered: Vec<ModelMetadata> = candidates
                .iter()
                .filter(|m| m.cost_per_1k_input_tokens * (context_size as f64 / 1000.0) <= budget)
                .cloned()
                .collect();
            if !budget_filtered.is_empty() {
                candidates = budget_filtered;
            }
        }

        // Sort: cheapest first for economy routing; best tier for quality routing.
        candidates.sort_by(|a, b| {
            a.cost_per_1k_input_tokens
                .partial_cmp(&b.cost_per_1k_input_tokens)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let preferred = candidates.remove(0);
        let fallback_models: Vec<String> = candidates.iter().take(3).map(|m| m.id.clone()).collect();
        let reason = format!(
            "Selected {} (tier={}, cost=${:.4}/1k input) for capabilities: {}",
            preferred.id,
            preferred.tier,
            preferred.cost_per_1k_input_tokens,
            caps.iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let confidence = if fallback_models.is_empty() { 0.7 } else { 0.9 };

        RoutingHint {
            preferred_model: preferred.id,
            fallback_models,
            reason,
            confidence,
        }
    }

    /// Build a registry pre-populated with well-known models.
    pub fn built_in_registry() -> Self {
        let registry = Self::new();

        // GPT-4o
        registry.register(ModelMetadata {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            provider: "OpenAI".to_string(),
            tier: ModelTier::Premium,
            context_window: 128_000,
            max_output_tokens: 4_096,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::FunctionCalling,
                ModelCapability::ImageUnderstanding,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.0025,
            cost_per_1k_output_tokens: 0.01,
            requests_per_minute: 10_000,
            tokens_per_minute: 2_000_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // GPT-4o-mini
        registry.register(ModelMetadata {
            id: "gpt-4o-mini".to_string(),
            name: "GPT-4o Mini".to_string(),
            provider: "OpenAI".to_string(),
            tier: ModelTier::Economy,
            context_window: 128_000,
            max_output_tokens: 16_384,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::FunctionCalling,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.000150,
            cost_per_1k_output_tokens: 0.000600,
            requests_per_minute: 30_000,
            tokens_per_minute: 10_000_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // Claude 3.5 Sonnet
        registry.register(ModelMetadata {
            id: "claude-3-5-sonnet".to_string(),
            name: "Claude 3.5 Sonnet".to_string(),
            provider: "Anthropic".to_string(),
            tier: ModelTier::Advanced,
            context_window: 200_000,
            max_output_tokens: 8_192,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::FunctionCalling,
                ModelCapability::ImageUnderstanding,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.003,
            cost_per_1k_output_tokens: 0.015,
            requests_per_minute: 4_000,
            tokens_per_minute: 800_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // Claude 3 Haiku
        registry.register(ModelMetadata {
            id: "claude-3-haiku".to_string(),
            name: "Claude 3 Haiku".to_string(),
            provider: "Anthropic".to_string(),
            tier: ModelTier::Economy,
            context_window: 200_000,
            max_output_tokens: 4_096,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.00025,
            cost_per_1k_output_tokens: 0.00125,
            requests_per_minute: 4_000,
            tokens_per_minute: 800_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // Gemini 1.5 Pro
        registry.register(ModelMetadata {
            id: "gemini-1.5-pro".to_string(),
            name: "Gemini 1.5 Pro".to_string(),
            provider: "Google".to_string(),
            tier: ModelTier::Premium,
            context_window: 1_048_576,
            max_output_tokens: 8_192,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::FunctionCalling,
                ModelCapability::ImageUnderstanding,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.00125,
            cost_per_1k_output_tokens: 0.005,
            requests_per_minute: 360,
            tokens_per_minute: 4_000_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // Gemini 1.5 Flash
        registry.register(ModelMetadata {
            id: "gemini-1.5-flash".to_string(),
            name: "Gemini 1.5 Flash".to_string(),
            provider: "Google".to_string(),
            tier: ModelTier::Standard,
            context_window: 1_048_576,
            max_output_tokens: 8_192,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::ImageUnderstanding,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.000075,
            cost_per_1k_output_tokens: 0.0003,
            requests_per_minute: 1_000,
            tokens_per_minute: 4_000_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // Llama 3 70B
        registry.register(ModelMetadata {
            id: "llama-3-70b".to_string(),
            name: "Llama 3 70B".to_string(),
            provider: "Meta".to_string(),
            tier: ModelTier::Advanced,
            context_window: 8_192,
            max_output_tokens: 4_096,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::Summarization,
                ModelCapability::Translation,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.00059,
            cost_per_1k_output_tokens: 0.00079,
            requests_per_minute: 6_000,
            tokens_per_minute: 800_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        // Llama 3 8B
        registry.register(ModelMetadata {
            id: "llama-3-8b".to_string(),
            name: "Llama 3 8B".to_string(),
            provider: "Meta".to_string(),
            tier: ModelTier::Economy,
            context_window: 8_192,
            max_output_tokens: 4_096,
            capabilities: vec![
                ModelCapability::TextGeneration,
                ModelCapability::CodeGeneration,
                ModelCapability::Summarization,
                ModelCapability::Classification,
            ],
            cost_per_1k_input_tokens: 0.00010,
            cost_per_1k_output_tokens: 0.00010,
            requests_per_minute: 6_000,
            tokens_per_minute: 800_000,
            supports_streaming: true,
            supports_system_prompt: true,
            deprecated: false,
            successor: None,
        });

        registry
    }

    /// Check whether a model is within its rate limits.
    ///
    /// Returns `true` if the model can accept more requests/tokens, `false` if a limit is exceeded.
    pub fn rate_limit_check(
        &self,
        model_id: &str,
        requests_last_minute: u32,
        tokens_last_minute: u32,
    ) -> bool {
        match self.get(model_id) {
            Some(m) => {
                requests_last_minute < m.requests_per_minute
                    && tokens_last_minute < m.tokens_per_minute
            }
            None => false,
        }
    }

    /// Return deprecation warning strings for all deprecated models.
    pub fn deprecation_warnings(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|r| r.deprecated)
            .map(|r| {
                let successor_hint = r
                    .successor
                    .as_deref()
                    .map(|s| format!(" Please migrate to '{}'.", s))
                    .unwrap_or_default();
                format!(
                    "Model '{}' ({}) is deprecated.{}",
                    r.id, r.name, successor_hint
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_has_gpt4o() {
        let r = ModelRegistry::built_in_registry();
        assert!(r.get("gpt-4o").is_some());
    }

    #[test]
    fn models_with_capability_sorted_by_cost() {
        let r = ModelRegistry::built_in_registry();
        let models = r.models_with_capability(&ModelCapability::TextGeneration);
        assert!(!models.is_empty());
        for i in 1..models.len() {
            assert!(
                models[i].cost_per_1k_input_tokens >= models[i - 1].cost_per_1k_input_tokens
            );
        }
    }

    #[test]
    fn cheapest_for_capability_returns_some() {
        let r = ModelRegistry::built_in_registry();
        let m = r.cheapest_for_capability(&ModelCapability::FunctionCalling, 0);
        assert!(m.is_some());
    }

    #[test]
    fn best_for_capability_returns_highest_tier() {
        let r = ModelRegistry::built_in_registry();
        let m = r.best_for_capability(&ModelCapability::ImageUnderstanding, 0);
        assert!(m.is_some());
        let m = m.unwrap();
        assert!(m.tier == ModelTier::Premium || m.tier == ModelTier::Advanced);
    }

    #[test]
    fn rate_limit_check_within_limit() {
        let r = ModelRegistry::built_in_registry();
        assert!(r.rate_limit_check("gpt-4o", 100, 10_000));
    }

    #[test]
    fn rate_limit_check_exceeds_limit() {
        let r = ModelRegistry::built_in_registry();
        assert!(!r.rate_limit_check("gpt-4o", 999_999, 999_999_999));
    }

    #[test]
    fn no_deprecation_warnings_in_built_in() {
        let r = ModelRegistry::built_in_registry();
        assert!(r.deprecation_warnings().is_empty());
    }

    #[test]
    fn deprecation_warning_for_deprecated_model() {
        let r = ModelRegistry::new();
        r.register(ModelMetadata {
            id: "old-model".to_string(),
            name: "Old Model".to_string(),
            provider: "Acme".to_string(),
            tier: ModelTier::Standard,
            context_window: 4_096,
            max_output_tokens: 1_024,
            capabilities: vec![ModelCapability::TextGeneration],
            cost_per_1k_input_tokens: 0.01,
            cost_per_1k_output_tokens: 0.02,
            requests_per_minute: 100,
            tokens_per_minute: 50_000,
            supports_streaming: false,
            supports_system_prompt: false,
            deprecated: true,
            successor: Some("new-model".to_string()),
        });
        let warnings = r.deprecation_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("old-model"));
        assert!(warnings[0].contains("new-model"));
    }

    #[test]
    fn suggest_routing_no_cap_match_returns_empty() {
        let r = ModelRegistry::new(); // empty
        let hint = r.suggest_routing(&[ModelCapability::Embedding], 0, None);
        assert!(hint.preferred_model.is_empty());
        assert!((hint.confidence - 0.0).abs() < f64::EPSILON);
    }
}
