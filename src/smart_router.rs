//! # Cost-Aware Smart Router
//!
//! Routes prompts to the cheapest provider meeting the request's SLA:
//!   - Latency budget (max acceptable latency in ms)
//!   - Quality floor (minimum acceptable quality score)
//!   - Cost ceiling (maximum USD per request)
//!
//! ## Algorithm
//!
//! 1. Filter providers: remove unhealthy and those exceeding the latency budget.
//! 2. Filter providers: remove those below the quality floor.
//! 3. Filter providers: remove those exceeding the cost ceiling.
//! 4. If `prefer_quality` is set, select the highest-quality remaining provider.
//! 5. Otherwise select the cheapest provider that meets all constraints.
//! 6. The remaining providers (sorted cheapest-first) become the fallback list.
//!
//! Pricing is configurable and can be hot-reloaded from a TOML string at
//! runtime via [`SmartRouter::load_pricing_toml`].

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// ModelPricing
// ---------------------------------------------------------------------------

/// Per-token pricing and capability metadata for one provider/model combination.
#[derive(Debug, Clone)]
pub struct ModelPricing {
    /// Stable provider identifier (e.g. `"anthropic"`, `"openai"`).
    pub provider_id: String,
    /// Model name (e.g. `"claude-3-5-sonnet-20241022"`).
    pub model: String,
    /// USD cost per **1 million** input tokens.
    pub input_per_million: f64,
    /// USD cost per **1 million** output tokens.
    pub output_per_million: f64,
    /// Estimated quality score in `[0.0, 1.0]` derived from public benchmarks.
    pub quality_score: f32,
    /// Typical p95 latency for this model in milliseconds.
    pub typical_p95_ms: u64,
}

impl ModelPricing {
    /// Estimate the total cost of a single request in USD.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn estimate_cost(&self, input_tokens: usize, estimated_output_tokens: usize) -> f64 {
        let input_cost = (input_tokens as f64 / 1_000_000.0) * self.input_per_million;
        let output_cost =
            (estimated_output_tokens as f64 / 1_000_000.0) * self.output_per_million;
        input_cost + output_cost
    }
}

// ---------------------------------------------------------------------------
// RoutingRequirements
// ---------------------------------------------------------------------------

/// SLA constraints for a single routing request.
#[derive(Debug, Clone)]
pub struct RoutingRequirements {
    /// Maximum acceptable p95 latency in milliseconds. `None` = no constraint.
    pub max_latency_ms: Option<u64>,
    /// Minimum acceptable quality score. `None` = no constraint.
    pub min_quality: Option<f32>,
    /// Maximum cost per request in USD. `None` = no constraint.
    pub max_cost_usd: Option<f64>,
    /// Estimated number of input tokens (used for cost estimation).
    pub estimated_input_tokens: usize,
    /// Estimated output tokens (rough guess for cost estimation).
    pub estimated_output_tokens: usize,
    /// When `true`, ignore cost constraints and always pick the highest-quality
    /// provider that satisfies latency requirements.
    pub prefer_quality: bool,
}

impl Default for RoutingRequirements {
    /// Construct permissive requirements: no latency/quality/cost constraints,
    /// 512 estimated input tokens, 256 estimated output tokens.
    ///
    /// # Panics
    ///
    /// Never panics.
    fn default() -> Self {
        Self {
            max_latency_ms: None,
            min_quality: None,
            max_cost_usd: None,
            estimated_input_tokens: 512,
            estimated_output_tokens: 256,
            prefer_quality: false,
        }
    }
}

// ---------------------------------------------------------------------------
// RoutingDecision
// ---------------------------------------------------------------------------

/// The result of a routing call: primary provider plus ordered fallbacks.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// Chosen provider identifier.
    pub provider_id: String,
    /// Model name for the chosen provider.
    pub model: String,
    /// Estimated cost for this request in USD.
    pub estimated_cost_usd: f64,
    /// Human-readable explanation of why this provider was selected.
    pub reason: String,
    /// Ordered list of fallback provider IDs to try if the primary fails.
    /// Sorted cheapest-first (or quality-first when `prefer_quality` is set).
    pub fallbacks: Vec<String>,
}

// ---------------------------------------------------------------------------
// SmartRouter
// ---------------------------------------------------------------------------

/// Routes incoming requests to the cheapest (or highest-quality) provider
/// that satisfies all SLA constraints.
pub struct SmartRouter {
    pricing: Vec<ModelPricing>,
    health: crate::provider_health::ProviderHealthMonitor,
}

impl SmartRouter {
    /// Create a new router with the given pricing table and health monitor.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn new(
        pricing: Vec<ModelPricing>,
        health: crate::provider_health::ProviderHealthMonitor,
    ) -> Self {
        Self { pricing, health }
    }

    /// Route a request, returning the best [`RoutingDecision`] or `None` if no
    /// suitable provider exists after applying all filters.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn route(&self, req: &RoutingRequirements) -> Option<RoutingDecision> {
        // Step 1 — remove unhealthy providers.
        let healthy = self.filter_by_health(&self.pricing).await;
        // Step 2 — remove providers that exceed the latency budget.
        let within_latency = self.filter_by_latency(healthy, req.max_latency_ms);
        // Step 3 — remove providers below the quality floor.
        let quality_ok = self.filter_by_quality(within_latency, req.min_quality);
        // Step 4 — remove providers that exceed the cost ceiling.
        let cost_ok = self.filter_by_cost(quality_ok, req);

        if cost_ok.is_empty() {
            return None;
        }

        // Select primary provider.
        let primary = if req.prefer_quality {
            self.best_quality(cost_ok.clone())?
        } else {
            self.cheapest(cost_ok.clone(), req)?
        };

        let estimated_cost = primary
            .estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens);

        let reason = if req.prefer_quality {
            format!(
                "quality-preferred routing: {} (quality={:.2})",
                primary.model, primary.quality_score
            )
        } else {
            format!(
                "cost-optimised routing: {} (est. ${:.6})",
                primary.model, estimated_cost
            )
        };

        // Build fallback list: remaining providers sorted cheapest/quality-first.
        let mut fallbacks: Vec<&ModelPricing> = cost_ok
            .iter()
            .filter(|p| p.provider_id != primary.provider_id)
            .copied()
            .collect();

        if req.prefer_quality {
            fallbacks.sort_by(|a, b| {
                b.quality_score
                    .partial_cmp(&a.quality_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            fallbacks.sort_by(|a, b| {
                let ca = a.estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens);
                let cb = b.estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens);
                ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        Some(RoutingDecision {
            provider_id: primary.provider_id.clone(),
            model: primary.model.clone(),
            estimated_cost_usd: estimated_cost,
            reason,
            fallbacks: fallbacks.iter().map(|p| p.provider_id.clone()).collect(),
        })
    }

    /// Return the built-in pricing table for common LLM models.
    ///
    /// Prices are in USD per 1 million tokens as of early 2025.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn default_pricing() -> Vec<ModelPricing> {
        vec![
            // Anthropic
            ModelPricing {
                provider_id: "anthropic".to_string(),
                model: "claude-3-5-sonnet-20241022".to_string(),
                input_per_million: 3.0,
                output_per_million: 15.0,
                quality_score: 0.92,
                typical_p95_ms: 4500,
            },
            ModelPricing {
                provider_id: "anthropic-haiku".to_string(),
                model: "claude-3-haiku-20240307".to_string(),
                input_per_million: 0.25,
                output_per_million: 1.25,
                quality_score: 0.74,
                typical_p95_ms: 1800,
            },
            // OpenAI
            ModelPricing {
                provider_id: "openai-gpt4o".to_string(),
                model: "gpt-4o".to_string(),
                input_per_million: 2.5,
                output_per_million: 10.0,
                quality_score: 0.90,
                typical_p95_ms: 5000,
            },
            ModelPricing {
                provider_id: "openai-gpt4o-mini".to_string(),
                model: "gpt-4o-mini".to_string(),
                input_per_million: 0.15,
                output_per_million: 0.60,
                quality_score: 0.78,
                typical_p95_ms: 2000,
            },
            ModelPricing {
                provider_id: "openai-gpt35".to_string(),
                model: "gpt-3.5-turbo".to_string(),
                input_per_million: 0.50,
                output_per_million: 1.50,
                quality_score: 0.65,
                typical_p95_ms: 2500,
            },
            // Meta / local
            ModelPricing {
                provider_id: "meta-llama".to_string(),
                model: "llama-3.1-8b-instruct".to_string(),
                input_per_million: 0.06,
                output_per_million: 0.06,
                quality_score: 0.60,
                typical_p95_ms: 1200,
            },
        ]
    }

    /// Hot-reload pricing from a TOML string.
    ///
    /// The TOML document must contain a top-level `[[model]]` array, where each
    /// entry has the fields: `provider_id`, `model`, `input_per_million`,
    /// `output_per_million`, `quality_score`, `typical_p95_ms`.
    ///
    /// On success, replaces the current pricing table and returns the number of
    /// models loaded.  On parse failure, the existing table is **not** modified.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` describing the parse failure.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub fn load_pricing_toml(&mut self, toml_str: &str) -> Result<usize, String> {
        #[derive(serde::Deserialize)]
        struct PricingFile {
            model: Vec<ModelPricingToml>,
        }

        #[derive(serde::Deserialize)]
        struct ModelPricingToml {
            provider_id: String,
            model: String,
            input_per_million: f64,
            output_per_million: f64,
            quality_score: f32,
            typical_p95_ms: u64,
        }

        let parsed: PricingFile =
            toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {e}"))?;

        let new_pricing: Vec<ModelPricing> = parsed
            .model
            .into_iter()
            .map(|m| ModelPricing {
                provider_id: m.provider_id,
                model: m.model,
                input_per_million: m.input_per_million,
                output_per_million: m.output_per_million,
                quality_score: m.quality_score,
                typical_p95_ms: m.typical_p95_ms,
            })
            .collect();

        let count = new_pricing.len();
        self.pricing = new_pricing;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // Private filter helpers
    // -----------------------------------------------------------------------

    /// Filter out providers whose health monitor marks them as unusable.
    async fn filter_by_health<'a>(&self, pricing: &'a [ModelPricing]) -> Vec<&'a ModelPricing> {
        let mut result = Vec::with_capacity(pricing.len());
        for p in pricing {
            if self.health.is_usable(&p.provider_id).await {
                result.push(p);
            }
        }
        result
    }

    /// Filter out providers whose `typical_p95_ms` exceeds `max_ms`.
    fn filter_by_latency<'a>(
        &self,
        providers: Vec<&'a ModelPricing>,
        max_ms: Option<u64>,
    ) -> Vec<&'a ModelPricing> {
        match max_ms {
            None => providers,
            Some(limit) => providers
                .into_iter()
                .filter(|p| p.typical_p95_ms <= limit)
                .collect(),
        }
    }

    /// Filter out providers whose `quality_score` is below `min_quality`.
    fn filter_by_quality<'a>(
        &self,
        providers: Vec<&'a ModelPricing>,
        min_quality: Option<f32>,
    ) -> Vec<&'a ModelPricing> {
        match min_quality {
            None => providers,
            Some(floor) => providers
                .into_iter()
                .filter(|p| p.quality_score >= floor)
                .collect(),
        }
    }

    /// Filter out providers whose estimated cost exceeds `req.max_cost_usd`.
    fn filter_by_cost<'a>(
        &self,
        providers: Vec<&'a ModelPricing>,
        req: &RoutingRequirements,
    ) -> Vec<&'a ModelPricing> {
        match req.max_cost_usd {
            None => providers,
            Some(ceiling) => providers
                .into_iter()
                .filter(|p| {
                    p.estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens)
                        <= ceiling
                })
                .collect(),
        }
    }

    /// Return the cheapest provider from `providers` for the given request.
    fn cheapest<'a>(
        &self,
        providers: Vec<&'a ModelPricing>,
        req: &RoutingRequirements,
    ) -> Option<&'a ModelPricing> {
        providers.into_iter().min_by(|a, b| {
            let ca = a.estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens);
            let cb = b.estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Return the highest-quality provider from `providers`.
    fn best_quality<'a>(&self, providers: Vec<&'a ModelPricing>) -> Option<&'a ModelPricing> {
        providers.into_iter().max_by(|a, b| {
            a.quality_score
                .partial_cmp(&b.quality_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_health::ProviderHealthMonitor;

    fn make_router() -> SmartRouter {
        let health = ProviderHealthMonitor::new(20);
        SmartRouter::new(SmartRouter::default_pricing(), health)
    }

    #[test]
    fn test_estimate_cost_zero_tokens() {
        let p = ModelPricing {
            provider_id: "test".into(),
            model: "m".into(),
            input_per_million: 3.0,
            output_per_million: 15.0,
            quality_score: 0.9,
            typical_p95_ms: 2000,
        };
        assert_eq!(p.estimate_cost(0, 0), 0.0);
    }

    #[test]
    fn test_estimate_cost_one_million_tokens() {
        let p = ModelPricing {
            provider_id: "test".into(),
            model: "m".into(),
            input_per_million: 3.0,
            output_per_million: 15.0,
            quality_score: 0.9,
            typical_p95_ms: 2000,
        };
        // 1M input + 1M output = $3 + $15 = $18
        assert!((p.estimate_cost(1_000_000, 1_000_000) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn test_default_pricing_not_empty() {
        let pricing = SmartRouter::default_pricing();
        assert!(!pricing.is_empty(), "default pricing must include at least one model");
        // All quality scores must be in [0, 1].
        for p in &pricing {
            assert!(
                p.quality_score >= 0.0 && p.quality_score <= 1.0,
                "quality_score out of range for {}",
                p.model
            );
            assert!(p.input_per_million >= 0.0);
            assert!(p.output_per_million >= 0.0);
        }
    }

    #[tokio::test]
    async fn test_route_no_constraints_returns_cheapest() {
        let router = make_router();
        let req = RoutingRequirements::default();
        let decision = router.route(&req).await.unwrap();
        // llama-3.1-8b is the cheapest in the default table at $0.06/$0.06.
        assert_eq!(decision.provider_id, "meta-llama");
    }

    #[tokio::test]
    async fn test_route_prefer_quality() {
        let router = make_router();
        let req = RoutingRequirements {
            prefer_quality: true,
            ..Default::default()
        };
        let decision = router.route(&req).await.unwrap();
        // claude-3-5-sonnet has quality_score 0.92, highest in table.
        assert_eq!(decision.provider_id, "anthropic");
    }

    #[tokio::test]
    async fn test_route_latency_filter_excludes_slow_models() {
        let router = make_router();
        let req = RoutingRequirements {
            // Only models with typical_p95_ms <= 2000 pass.
            max_latency_ms: Some(2000),
            ..Default::default()
        };
        let decision = router.route(&req).await.unwrap();
        // Only llama (1200), haiku (1800), gpt4o-mini (2000) qualify.
        // Cheapest of those three is llama.
        assert_eq!(decision.provider_id, "meta-llama");
    }

    #[tokio::test]
    async fn test_route_quality_floor_excludes_low_quality() {
        let router = make_router();
        let req = RoutingRequirements {
            // Only quality >= 0.88 qualifies: sonnet (0.92) and gpt-4o (0.90).
            min_quality: Some(0.88),
            ..Default::default()
        };
        let decision = router.route(&req).await.unwrap();
        // gpt-4o costs $2.5 input / $10 output; sonnet costs $3/$15.
        // gpt-4o is cheaper for the default 512+256 token estimate.
        assert_eq!(decision.provider_id, "openai-gpt4o");
    }

    #[tokio::test]
    async fn test_route_returns_none_when_no_candidates() {
        let router = make_router();
        let req = RoutingRequirements {
            // Impossible: require quality >= 0.99 AND latency <= 100 ms.
            min_quality: Some(0.99),
            max_latency_ms: Some(100),
            ..Default::default()
        };
        assert!(router.route(&req).await.is_none());
    }

    #[tokio::test]
    async fn test_fallbacks_ordered_cheapest_first() {
        let router = make_router();
        let req = RoutingRequirements::default();
        let decision = router.route(&req).await.unwrap();
        // Verify fallbacks exist and that no fallback is the same as primary.
        for fb in &decision.fallbacks {
            assert_ne!(fb, &decision.provider_id);
        }
        // Verify they are in ascending cost order.
        let pricing = SmartRouter::default_pricing();
        let cost_of = |id: &str| {
            pricing
                .iter()
                .find(|p| p.provider_id == id)
                .map(|p| p.estimate_cost(req.estimated_input_tokens, req.estimated_output_tokens))
                .unwrap_or(f64::MAX)
        };
        for window in decision.fallbacks.windows(2) {
            assert!(
                cost_of(&window[0]) <= cost_of(&window[1]),
                "fallbacks should be cheapest-first"
            );
        }
    }

    #[tokio::test]
    async fn test_load_pricing_toml_success() {
        let mut router = make_router();
        let toml_str = r#"
[[model]]
provider_id = "test-provider"
model = "test-model-v1"
input_per_million = 1.0
output_per_million = 2.0
quality_score = 0.8
typical_p95_ms = 1500
"#;
        let count = router.load_pricing_toml(toml_str).unwrap();
        assert_eq!(count, 1);
        let req = RoutingRequirements::default();
        let decision = router.route(&req).await.unwrap();
        assert_eq!(decision.provider_id, "test-provider");
    }

    #[test]
    fn test_load_pricing_toml_invalid_returns_err() {
        let mut router = make_router();
        let bad_toml = "this is not valid toml ][[[";
        assert!(router.load_pricing_toml(bad_toml).is_err());
        // Existing pricing should be unchanged.
        assert!(!router.pricing.is_empty());
    }
}
