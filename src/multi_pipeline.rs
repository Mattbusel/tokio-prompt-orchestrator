//! Multi-pipeline routing with prompt classification.
//!
//! Manages a fleet of named pipeline instances and routes each incoming
//! [`PromptRequest`] to the best-fit pipeline based on prompt characteristics:
//! length, complexity signals, explicit priority hints, and learned latency.
//!
//! ## Concept
//!
//! A single LLM orchestrator often needs to handle very different workloads
//! simultaneously:
//!
//! - **Fast path** — short FAQ-style questions, single-turn, low latency required
//! - **Reasoning path** — multi-step problems, chain-of-thought, higher quality
//! - **Code path** — specialised coding model, longer context window
//! - **Batch path** — offline background jobs, throughput over latency
//!
//! Rather than forcing all traffic through one pipeline, `MultiPipelineRouter`
//! dispatches to the right pipeline and falls back gracefully when a pipeline
//! is at capacity.
//!
//! ## Example
//!
//! ```no_run
//! use std::sync::Arc;
//! use tokio_prompt_orchestrator::{EchoWorker, PromptRequest, SessionId};
//! use tokio_prompt_orchestrator::multi_pipeline::{
//!     MultiPipelineRouter, PipelineDescriptor, PromptClass,
//! };
//! use std::collections::HashMap;
//!
//! # async fn example() {
//! let router = MultiPipelineRouter::builder()
//!     .add_pipeline(PipelineDescriptor::new("fast", PromptClass::Faq, Arc::new(EchoWorker::new())))
//!     .add_pipeline(PipelineDescriptor::new("reasoning", PromptClass::Reasoning, Arc::new(EchoWorker::new())))
//!     .build();
//!
//! let request = PromptRequest {
//!     session: SessionId::new("s1"),
//!     request_id: "r1".to_string(),
//!     input: "What is 2+2?".to_string(),
//!     meta: HashMap::new(),
//!     deadline: None,
//! };
//!
//! let class = router.classify(&request);
//! router.route(request).await.unwrap();
//! # }
//! ```

use crate::{
    stages::{spawn_pipeline_with_config, PipelineHandles},
    worker::ModelWorker,
    OrchestratorError, PromptRequest,
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Broad classification of a prompt's intent and resource requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptClass {
    /// Short, simple questions with a known answer pattern.
    /// Routes to the fast pipeline for minimal latency.
    Faq,
    /// Multi-step reasoning, analysis, or planning tasks.
    /// Routes to the reasoning pipeline for quality.
    Reasoning,
    /// Code generation, review, or debugging requests.
    /// Routes to the code-specialised pipeline.
    Code,
    /// Long-form document processing (summarisation, translation, extraction).
    /// Routes to the batch pipeline for throughput.
    Document,
    /// Background / offline work — latency-insensitive.
    Batch,
    /// Catch-all: no strong classification signal.
    General,
}

impl std::fmt::Display for PromptClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Faq => "faq",
            Self::Reasoning => "reasoning",
            Self::Code => "code",
            Self::Document => "document",
            Self::Batch => "batch",
            Self::General => "general",
        };
        write!(f, "{s}")
    }
}

/// Heuristic prompt classifier.
///
/// Uses lightweight, allocation-free checks to classify a prompt without
/// calling any external model. For production use, swap in an embedding-based
/// classifier via [`PromptClassifier`] trait implementations.
pub struct HeuristicClassifier {
    /// Minimum token-estimate for a prompt to be treated as a Document.
    pub document_token_threshold: usize,
    /// Minimum token-estimate for reasoning (above FAQ, below document).
    pub reasoning_token_threshold: usize,
}

impl Default for HeuristicClassifier {
    fn default() -> Self {
        Self {
            document_token_threshold: 800,
            reasoning_token_threshold: 100,
        }
    }
}

impl HeuristicClassifier {
    /// Quick token count approximation: split on whitespace.
    fn approx_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    /// Classify a prompt text into a [`PromptClass`].
    ///
    /// Classification is purely heuristic and fast (~50 ns).
    pub fn classify(&self, prompt: &str) -> PromptClass {
        let lower = prompt.to_lowercase();
        let tokens = Self::approx_tokens(prompt);

        // Code signals
        if lower.contains("```")
            || lower.contains("fn ")
            || lower.contains("def ")
            || lower.contains("class ")
            || lower.contains("impl ")
            || lower.contains("function ")
            || lower.contains("// ")
            || lower.contains("# code")
            || lower.contains("write code")
            || lower.contains("debug")
            || lower.contains("compile error")
        {
            return PromptClass::Code;
        }

        // Reasoning signals
        if lower.contains("step by step")
            || lower.contains("explain why")
            || lower.contains("analyse")
            || lower.contains("analyze")
            || lower.contains("compare")
            || lower.contains("pros and cons")
            || lower.contains("reason")
            || lower.contains("chain of thought")
            || lower.contains("think through")
        {
            return PromptClass::Reasoning;
        }

        // Document signals
        if tokens >= self.document_token_threshold
            || lower.contains("summarise")
            || lower.contains("summarize")
            || lower.contains("translate")
            || lower.contains("extract all")
            || lower.contains("the following document")
            || lower.contains("the following text")
        {
            return PromptClass::Document;
        }

        // Batch signals (explicit meta hints checked in router)
        if lower.contains("batch")
            || lower.contains("background")
            || lower.contains("offline")
            || lower.contains("no rush")
        {
            return PromptClass::Batch;
        }

        // FAQ: short and direct
        if tokens < self.reasoning_token_threshold
            && (lower.ends_with('?') || lower.starts_with("what") || lower.starts_with("who")
                || lower.starts_with("when") || lower.starts_with("where")
                || lower.starts_with("how many") || lower.starts_with("is ")
                || lower.starts_with("does ") || lower.starts_with("can "))
        {
            return PromptClass::Faq;
        }

        PromptClass::General
    }
}

/// Descriptor for a single named pipeline instance.
pub struct PipelineDescriptor {
    /// Unique name (e.g. "fast", "reasoning", "code").
    pub name: String,
    /// Primary prompt class this pipeline serves.
    pub target_class: PromptClass,
    /// Additional classes this pipeline can serve (fallback order).
    pub also_serves: Vec<PromptClass>,
    /// The worker for this pipeline.
    pub worker: Arc<dyn ModelWorker>,
    /// Pipeline config override (uses default if None).
    pub config: Option<crate::config::PipelineConfig>,
}

impl PipelineDescriptor {
    /// Create a basic descriptor serving one prompt class.
    pub fn new(
        name: impl Into<String>,
        target_class: PromptClass,
        worker: Arc<dyn ModelWorker>,
    ) -> Self {
        Self {
            name: name.into(),
            target_class,
            also_serves: Vec::new(),
            worker,
            config: None,
        }
    }

    /// Add additional prompt classes this pipeline can handle.
    pub fn also_serving(mut self, classes: Vec<PromptClass>) -> Self {
        self.also_serves = classes;
        self
    }

    /// Override the default pipeline config.
    pub fn with_config(mut self, config: crate::config::PipelineConfig) -> Self {
        self.config = Some(config);
        self
    }
}

/// Per-pipeline runtime stats tracked for routing decisions.
#[derive(Default)]
struct PipelineStats {
    routed: AtomicU64,
    shed: AtomicU64,
    /// EMA of recent latency in milliseconds (×1000 as integer for atomics).
    ema_latency_ms_x1000: AtomicU64,
}

impl PipelineStats {
    fn record_routed(&self) {
        self.routed.fetch_add(1, Ordering::Relaxed);
    }

    fn record_shed(&self) {
        self.shed.fetch_add(1, Ordering::Relaxed);
    }

    fn update_latency(&self, latency_ms: f64) {
        const ALPHA: f64 = 0.1;
        let current = self.ema_latency_ms_x1000.load(Ordering::Relaxed) as f64 / 1000.0;
        let updated = if current == 0.0 {
            latency_ms
        } else {
            ALPHA * latency_ms + (1.0 - ALPHA) * current
        };
        self.ema_latency_ms_x1000
            .store((updated * 1000.0) as u64, Ordering::Relaxed);
    }

    fn ema_latency_ms(&self) -> f64 {
        self.ema_latency_ms_x1000.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

/// A live, spawned pipeline entry.
struct LivePipeline {
    descriptor_name: String,
    target_class: PromptClass,
    also_serves: Vec<PromptClass>,
    handles: PipelineHandles,
    stats: Arc<PipelineStats>,
}

/// Routes incoming prompts across multiple named pipeline instances.
///
/// Build via [`MultiPipelineRouter::builder()`].
pub struct MultiPipelineRouter {
    pipelines: Vec<LivePipeline>,
    classifier: HeuristicClassifier,
    /// Pipeline name → stats (for metrics/dashboard).
    stats_map: Arc<DashMap<String, Arc<PipelineStats>>>,
}

impl MultiPipelineRouter {
    /// Create a new builder.
    pub fn builder() -> MultiPipelineRouterBuilder {
        MultiPipelineRouterBuilder::new()
    }

    /// Classify a prompt request using the heuristic classifier.
    ///
    /// Respects an optional `"pipeline_class"` key in `request.meta` for
    /// explicit override.
    pub fn classify(&self, request: &PromptRequest) -> PromptClass {
        // Check for explicit override in meta
        if let Some(class_hint) = request.meta.get("pipeline_class") {
            match class_hint.as_str() {
                "faq" => return PromptClass::Faq,
                "reasoning" => return PromptClass::Reasoning,
                "code" => return PromptClass::Code,
                "document" => return PromptClass::Document,
                "batch" => return PromptClass::Batch,
                _ => {}
            }
        }
        self.classifier.classify(&request.input)
    }

    /// Route a request to the best-fit pipeline.
    ///
    /// Routing priority:
    /// 1. Pipeline whose `target_class` matches the classified prompt.
    /// 2. Any pipeline that lists the class in `also_serves`.
    /// 3. First pipeline (default/general fallback).
    ///
    /// # Errors
    ///
    /// Returns [`OrchestratorError::ChannelClosed`] if the selected
    /// pipeline's input channel is closed.
    pub async fn route(&self, request: PromptRequest) -> Result<(), OrchestratorError> {
        let class = self.classify(&request);

        // Find primary match
        let pipeline = self
            .pipelines
            .iter()
            .find(|p| p.target_class == class)
            .or_else(|| {
                self.pipelines
                    .iter()
                    .find(|p| p.also_serves.contains(&class))
            })
            .or_else(|| self.pipelines.first());

        let Some(pipeline) = pipeline else {
            return Err(OrchestratorError::ConfigError(
                "MultiPipelineRouter has no pipelines registered".to_string(),
            ));
        };

        debug!(
            pipeline = %pipeline.descriptor_name,
            class = %class,
            request_id = %request.request_id,
            "routing request"
        );

        pipeline.stats.record_routed();

        pipeline
            .handles
            .input_tx
            .send(request)
            .await
            .map_err(|_| {
                pipeline.stats.record_shed();
                warn!(
                    pipeline = %pipeline.descriptor_name,
                    "pipeline channel closed — request shed"
                );
                OrchestratorError::ChannelClosed
            })
    }

    /// Return a snapshot of routing statistics per pipeline.
    pub fn stats(&self) -> Vec<PipelineRoutingStats> {
        self.pipelines
            .iter()
            .map(|p| PipelineRoutingStats {
                name: p.descriptor_name.clone(),
                target_class: p.target_class,
                routed: p.stats.routed.load(Ordering::Relaxed),
                shed: p.stats.shed.load(Ordering::Relaxed),
                ema_latency_ms: p.stats.ema_latency_ms(),
            })
            .collect()
    }

    /// Record observed latency for a pipeline (called by the pipeline consumer).
    pub fn record_latency(&self, pipeline_name: &str, latency_ms: f64) {
        if let Some(stats) = self.stats_map.get(pipeline_name) {
            stats.update_latency(latency_ms);
        }
    }
}

/// Snapshot of routing stats for one pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineRoutingStats {
    /// Pipeline name.
    pub name: String,
    /// Primary class this pipeline targets.
    pub target_class: PromptClass,
    /// Total requests routed to this pipeline.
    pub routed: u64,
    /// Requests shed (channel full or closed).
    pub shed: u64,
    /// Exponential moving average of observed latency.
    pub ema_latency_ms: f64,
}

/// Builder for [`MultiPipelineRouter`].
pub struct MultiPipelineRouterBuilder {
    descriptors: Vec<PipelineDescriptor>,
}

impl MultiPipelineRouterBuilder {
    fn new() -> Self {
        Self {
            descriptors: Vec::new(),
        }
    }

    /// Register a pipeline.
    pub fn add_pipeline(mut self, descriptor: PipelineDescriptor) -> Self {
        self.descriptors.push(descriptor);
        self
    }

    /// Spawn all pipelines and return the router.
    pub fn build(self) -> MultiPipelineRouter {
        let stats_map: Arc<DashMap<String, Arc<PipelineStats>>> = Arc::new(DashMap::new());
        let mut pipelines = Vec::new();

        for desc in self.descriptors {
            let stats = Arc::new(PipelineStats::default());
            stats_map.insert(desc.name.clone(), Arc::clone(&stats));

            let handles = if let Some(ref config) = desc.config {
                spawn_pipeline_with_config(desc.worker, config)
            } else {
                crate::stages::spawn_pipeline(desc.worker)
            };

            info!(
                pipeline = %desc.name,
                class = %desc.target_class,
                "pipeline spawned in multi-pipeline router"
            );

            pipelines.push(LivePipeline {
                descriptor_name: desc.name,
                target_class: desc.target_class,
                also_serves: desc.also_serves,
                handles,
                stats,
            });
        }

        MultiPipelineRouter {
            pipelines,
            classifier: HeuristicClassifier::default(),
            stats_map,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_code() {
        let c = HeuristicClassifier::default();
        assert_eq!(c.classify("```rust\nfn main() {}\n```"), PromptClass::Code);
        assert_eq!(c.classify("debug this compile error"), PromptClass::Code);
        assert_eq!(c.classify("write code to sort a list"), PromptClass::Code);
    }

    #[test]
    fn test_classify_faq() {
        let c = HeuristicClassifier::default();
        assert_eq!(c.classify("What is Rust?"), PromptClass::Faq);
        assert_eq!(c.classify("Who invented TCP/IP?"), PromptClass::Faq);
        assert_eq!(c.classify("Is Tokio async?"), PromptClass::Faq);
    }

    #[test]
    fn test_classify_reasoning() {
        let c = HeuristicClassifier::default();
        assert_eq!(
            c.classify("Explain why Rust is memory safe step by step"),
            PromptClass::Reasoning
        );
        assert_eq!(
            c.classify("analyse the pros and cons of async Rust"),
            PromptClass::Reasoning
        );
    }

    #[test]
    fn test_classify_document() {
        let c = HeuristicClassifier::default();
        // Long text crosses document threshold
        let long = "word ".repeat(900);
        assert_eq!(c.classify(&long), PromptClass::Document);
        assert_eq!(c.classify("summarize the following document"), PromptClass::Document);
    }

    #[test]
    fn test_classify_general_fallback() {
        let c = HeuristicClassifier::default();
        // Non-signal text
        assert_eq!(c.classify("hello"), PromptClass::General);
    }

    #[test]
    fn test_pipeline_stats_ema() {
        let stats = PipelineStats::default();
        stats.update_latency(100.0);
        stats.update_latency(200.0);
        let ema = stats.ema_latency_ms();
        // Should be between 100 and 200
        assert!(ema > 100.0 && ema < 200.0, "ema={ema}");
    }

    #[tokio::test]
    async fn test_router_routes_to_matching_pipeline() {
        use crate::EchoWorker;
        use std::collections::HashMap;

        let router = MultiPipelineRouter::builder()
            .add_pipeline(PipelineDescriptor::new(
                "faq",
                PromptClass::Faq,
                Arc::new(EchoWorker::new()),
            ))
            .add_pipeline(PipelineDescriptor::new(
                "general",
                PromptClass::General,
                Arc::new(EchoWorker::new()),
            ))
            .build();

        let req = PromptRequest {
            session: crate::SessionId::new("s1"),
            request_id: "r1".to_string(),
            input: "What is Tokio?".to_string(),
            meta: HashMap::new(),
            deadline: None,
        };

        let class = router.classify(&req);
        assert_eq!(class, PromptClass::Faq);

        // Route should succeed without error
        router.route(req).await.expect("route should succeed");

        let stats = router.stats();
        let faq_stats = stats.iter().find(|s| s.name == "faq").unwrap();
        assert_eq!(faq_stats.routed, 1);
    }

    #[tokio::test]
    async fn test_meta_override() {
        use crate::EchoWorker;
        use std::collections::HashMap;

        let router = MultiPipelineRouter::builder()
            .add_pipeline(PipelineDescriptor::new(
                "general",
                PromptClass::General,
                Arc::new(EchoWorker::new()),
            ))
            .build();

        let mut meta = HashMap::new();
        meta.insert("pipeline_class".to_string(), "code".to_string());
        let req = PromptRequest {
            session: crate::SessionId::new("s1"),
            request_id: "r1".to_string(),
            input: "hello world".to_string(),
            meta,
            deadline: None,
        };

        assert_eq!(router.classify(&req), PromptClass::Code);
    }
}
