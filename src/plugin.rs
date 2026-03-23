//! Custom plugin stage system for the LLM inference pipeline.
//!
//! This module provides a first-class extension point so user code can inject
//! arbitrary async processing logic into the pipeline without forking the
//! library.  Plugins are composable, ordered, and fully type-safe.
//!
//! ## Architecture
//!
//! ```text
//! PromptRequest
//!     |
//!     v
//! [BeforeRag plugins]  -> RAG stage -> [AfterRag plugins]
//!     |
//!     v
//! [BeforeAssemble plugins] -> Assemble stage -> [AfterAssemble plugins]
//!     |
//!     v
//! [BeforeInference plugins] -> Inference stage -> [AfterInference plugins]
//!     |
//!     v
//! [BeforePost plugins] -> Post stage -> [AfterPost plugins]
//!     |
//!     v
//! [BeforeStream plugins] -> Stream stage -> [AfterStream plugins]
//! ```
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use tokio_prompt_orchestrator::plugin::{
//!     StagePlugin, PluginInput, PluginOutput, PluginPosition, PluginRegistry,
//! };
//! use tokio_prompt_orchestrator::PipelineStage;
//! use async_trait::async_trait;
//! use std::sync::Arc;
//!
//! /// A simple logging plugin that records when the inference stage is entered.
//! struct InferenceLogPlugin;
//!
//! #[async_trait]
//! impl StagePlugin for InferenceLogPlugin {
//!     fn name(&self) -> &'static str { "inference-logger" }
//!
//!     async fn process(&self, input: PluginInput) -> PluginOutput {
//!         tracing::info!(request_id = %input.request_id, "entering inference stage");
//!         PluginOutput::passthrough(input)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut registry = PluginRegistry::new();
//!     registry.register(
//!         PluginPosition::Before(PipelineStage::Inference),
//!         Arc::new(InferenceLogPlugin),
//!     );
//!
//!     let chain = registry.chain_for(PluginPosition::Before(PipelineStage::Inference));
//!     let input = PluginInput {
//!         request_id: "req-1".to_string(),
//!         session_id: "session-1".to_string(),
//!         payload: serde_json::json!({"prompt": "hello"}),
//!         metadata: std::collections::HashMap::new(),
//!     };
//!
//!     let output = chain.run(input).await;
//!     println!("Plugin chain produced: {:?}", output.status);
//! }
//! ```

use crate::PipelineStage;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

// ============================================================================
// PluginInput / PluginOutput
// ============================================================================

/// Input passed to every plugin in a [`PluginChain`].
///
/// The `payload` field carries stage-specific data as an untyped JSON value so
/// that the same trait works across all pipeline stages without requiring a
/// separate trait per stage.
///
/// # Cloning
///
/// `PluginInput` is cheap to clone; `payload` clones are backed by `Arc`
/// internally in `serde_json::Value`.
#[derive(Debug, Clone)]
pub struct PluginInput {
    /// Unique request ID for distributed trace correlation.
    pub request_id: String,
    /// Session this request belongs to.
    pub session_id: String,
    /// Stage-specific payload (prompt text, tokens, assembled prompt, …).
    ///
    /// Plugins that do not need to inspect or mutate the payload should pass
    /// it through unchanged via [`PluginOutput::passthrough`].
    pub payload: Value,
    /// Arbitrary key-value metadata forwarded from the originating
    /// [`crate::PromptRequest`].  Plugins may read and extend this map.
    pub metadata: HashMap<String, String>,
}

/// The outcome status of a plugin's `process` call.
///
/// Consumers (typically [`PluginChain::run`]) inspect this to decide whether
/// to continue executing the remaining plugins in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStatus {
    /// Processing may continue normally.
    Continue,
    /// This plugin wants to short-circuit the remaining chain.
    ///
    /// `PluginChain::run` stops executing subsequent plugins and returns this
    /// output immediately.  The pipeline stage itself still runs; use this to
    /// replace or skip preprocessing steps, not to cancel inference entirely.
    Abort,
    /// The plugin detected an error and wants to propagate it.
    ///
    /// Carries a human-readable description.  Like `Abort`, the remaining
    /// chain is skipped.  The caller receives the `Error` status and can
    /// choose to shed the request to the dead-letter queue.
    Error(String),
}

/// Output returned by a [`StagePlugin::process`] call.
#[derive(Debug, Clone)]
pub struct PluginOutput {
    /// The (possibly modified) input to pass to the next plugin or stage.
    pub input: PluginInput,
    /// Status signal for the chain runner.
    pub status: PluginStatus,
}

impl PluginOutput {
    /// Construct a `Continue` output that passes `input` through unchanged.
    ///
    /// This is the most common return value for plugins that only observe
    /// or annotate requests without modifying the payload.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn passthrough(input: PluginInput) -> Self {
        Self {
            input,
            status: PluginStatus::Continue,
        }
    }

    /// Construct a `Continue` output with a mutated payload.
    ///
    /// Use this when a plugin needs to transform the data before it reaches
    /// the next stage (e.g., sanitising PII, injecting system context).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn modified(mut input: PluginInput, payload: Value) -> Self {
        input.payload = payload;
        Self {
            input,
            status: PluginStatus::Continue,
        }
    }

    /// Construct an `Abort` output, stopping the plugin chain.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn abort(input: PluginInput) -> Self {
        Self {
            input,
            status: PluginStatus::Abort,
        }
    }

    /// Construct an `Error` output with a descriptive message.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn error(input: PluginInput, message: impl Into<String>) -> Self {
        Self {
            input,
            status: PluginStatus::Error(message.into()),
        }
    }
}

// ============================================================================
// StagePlugin trait
// ============================================================================

/// Core trait for custom pipeline stage plugins.
///
/// Implementors are inserted at specific points in the pipeline via a
/// [`PluginRegistry`].  Each plugin receives a [`PluginInput`] and must return
/// a [`PluginOutput`] indicating whether processing should continue.
///
/// ## Correctness Requirements
///
/// - Implementations **must not** panic — return [`PluginOutput::error`]
///   instead.
/// - Implementations **must** be `Send + Sync` for safe use across Tokio tasks.
/// - The `process` method **must** complete in bounded time; use
///   [`tokio::time::timeout`] internally if calling external services.
///
/// ## Example
///
/// ```rust
/// use tokio_prompt_orchestrator::plugin::{StagePlugin, PluginInput, PluginOutput};
/// use async_trait::async_trait;
///
/// /// Redacts email addresses from the prompt payload.
/// struct PiiRedactor;
///
/// #[async_trait]
/// impl StagePlugin for PiiRedactor {
///     fn name(&self) -> &'static str { "pii-redactor" }
///
///     async fn process(&self, input: PluginInput) -> PluginOutput {
///         // Inspect and potentially mutate the payload here.
///         PluginOutput::passthrough(input)
///     }
/// }
/// ```
#[async_trait]
pub trait StagePlugin: Send + Sync {
    /// A unique, human-readable name used in logs and metrics.
    ///
    /// Should be `kebab-case`, e.g. `"pii-redactor"` or `"context-injector"`.
    fn name(&self) -> &'static str;

    /// Process `input` and return the (potentially modified) output.
    ///
    /// # Errors
    ///
    /// Return [`PluginOutput::error`] rather than propagating panics or
    /// returning unexpected results.
    ///
    /// # Panics
    ///
    /// Implementations must never panic.
    async fn process(&self, input: PluginInput) -> PluginOutput;
}

// ============================================================================
// PluginPosition
// ============================================================================

/// Specifies where in the pipeline a plugin should be inserted.
///
/// Each variant corresponds to a point immediately before or after one of the
/// five built-in pipeline stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginPosition {
    /// Run the plugin before the named stage.
    Before(PipelineStage),
    /// Run the plugin after the named stage.
    After(PipelineStage),
}

impl PluginPosition {
    /// Return the stage this position is relative to.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn stage(&self) -> PipelineStage {
        match self {
            Self::Before(s) | Self::After(s) => *s,
        }
    }

    /// Return `true` if this position is before a stage.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_before(&self) -> bool {
        matches!(self, Self::Before(_))
    }

    /// Return `true` if this position is after a stage.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_after(&self) -> bool {
        matches!(self, Self::After(_))
    }

    /// Return a human-readable label, e.g. `"before:inference"`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn label(&self) -> String {
        let prefix = if self.is_before() { "before" } else { "after" };
        format!("{}:{}", prefix, self.stage())
    }
}

// ============================================================================
// PluginChain
// ============================================================================

/// An ordered sequence of [`StagePlugin`]s that run at a single
/// [`PluginPosition`].
///
/// Created by [`PluginRegistry::chain_for`] and executed by calling
/// [`PluginChain::run`].  An empty chain is a no-op: `run` returns
/// `PluginOutput::passthrough(input)` immediately.
///
/// ## Execution Model
///
/// Plugins run serially in insertion order.  If any plugin returns a
/// non-`Continue` status, execution halts and that output is returned
/// immediately without running the remaining plugins.
///
/// This matches the common middleware short-circuit pattern and keeps
/// plugin authors free from reasoning about concurrent modifications.
#[derive(Clone, Default)]
pub struct PluginChain {
    plugins: Vec<Arc<dyn StagePlugin>>,
    position: Option<PluginPosition>,
}

impl PluginChain {
    /// Create a new empty chain.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn new(position: PluginPosition) -> Self {
        Self {
            plugins: Vec::new(),
            position: Some(position),
        }
    }

    /// Append a plugin to the end of the chain.
    ///
    /// Plugins execute in the order they were added.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn push(&mut self, plugin: Arc<dyn StagePlugin>) {
        debug!(
            plugin = plugin.name(),
            position = ?self.position,
            "registered plugin in chain"
        );
        self.plugins.push(plugin);
    }

    /// Return the number of plugins currently in the chain.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Return `true` if no plugins are registered in this chain.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Execute all plugins in order, stopping early on `Abort` or `Error`.
    ///
    /// Returns the final [`PluginOutput`] after all plugins have run (or the
    /// first non-`Continue` output).  If the chain is empty, returns
    /// `PluginOutput::passthrough(input)` immediately.
    ///
    /// # Panics
    ///
    /// This function does not panic (plugin panics are the caller's bug).
    pub async fn run(&self, mut input: PluginInput) -> PluginOutput {
        if self.plugins.is_empty() {
            return PluginOutput::passthrough(input);
        }

        let position_label = self
            .position
            .map(|p| p.label())
            .unwrap_or_else(|| "unknown".to_string());

        for plugin in &self.plugins {
            let name = plugin.name();
            debug!(plugin = name, position = %position_label, "running plugin");

            let output = plugin.process(input).await;

            match &output.status {
                PluginStatus::Continue => {
                    // Propagate (possibly mutated) input to the next plugin.
                    input = output.input;
                }
                PluginStatus::Abort => {
                    warn!(
                        plugin = name,
                        position = %position_label,
                        "plugin aborted chain"
                    );
                    return output;
                }
                PluginStatus::Error(msg) => {
                    warn!(
                        plugin = name,
                        position = %position_label,
                        error = %msg,
                        "plugin returned error, aborting chain"
                    );
                    return output;
                }
            }
        }

        PluginOutput::passthrough(input)
    }
}

// ============================================================================
// PluginRegistry
// ============================================================================

/// Global runtime registry for [`StagePlugin`] instances.
///
/// Stores plugins keyed by [`PluginPosition`] and vends fully-built
/// [`PluginChain`]s on demand.  The registry is designed to be populated once
/// at startup and then used read-only during request processing.
///
/// For dynamic plugin management (add/remove while the pipeline is running),
/// wrap the registry in an `Arc<tokio::sync::RwLock<PluginRegistry>>`.
///
/// ## Example
///
/// ```rust,no_run
/// use tokio_prompt_orchestrator::plugin::{
///     PluginRegistry, PluginPosition, StagePlugin, PluginInput, PluginOutput,
/// };
/// use tokio_prompt_orchestrator::PipelineStage;
/// use async_trait::async_trait;
/// use std::sync::Arc;
///
/// struct Noop;
///
/// #[async_trait]
/// impl StagePlugin for Noop {
///     fn name(&self) -> &'static str { "noop" }
///     async fn process(&self, input: PluginInput) -> PluginOutput {
///         PluginOutput::passthrough(input)
///     }
/// }
///
/// let mut registry = PluginRegistry::new();
/// registry.register(
///     PluginPosition::Before(PipelineStage::Rag),
///     Arc::new(Noop),
/// );
///
/// assert_eq!(registry.plugin_count(PluginPosition::Before(PipelineStage::Rag)), 1);
/// registry.remove_all(PluginPosition::Before(PipelineStage::Rag));
/// assert_eq!(registry.plugin_count(PluginPosition::Before(PipelineStage::Rag)), 0);
/// ```
#[derive(Default)]
pub struct PluginRegistry {
    chains: HashMap<PluginPositionKey, PluginChain>,
}

/// Stable key type used as the `HashMap` key for [`PluginPosition`].
///
/// `PluginPosition` cannot be used directly as a map key because `PartialEq`
/// for the enum variant alone is insufficient when hashing; this wrapper
/// provides `Eq + Hash` using the variant's discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PluginPositionKey(PluginPosition);

impl PluginRegistry {
    /// Create a new, empty registry.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin at the given position.
    ///
    /// Multiple plugins registered at the same position are chained in
    /// insertion order.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn register(&mut self, position: PluginPosition, plugin: Arc<dyn StagePlugin>) {
        let chain = self
            .chains
            .entry(PluginPositionKey(position))
            .or_insert_with(|| PluginChain::new(position));
        chain.push(plugin);
    }

    /// Remove all plugins registered at `position`.
    ///
    /// After this call, [`chain_for`](Self::chain_for) will return an empty
    /// chain for that position.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn remove_all(&mut self, position: PluginPosition) {
        self.chains.remove(&PluginPositionKey(position));
    }

    /// Return the number of plugins registered at `position`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn plugin_count(&self, position: PluginPosition) -> usize {
        self.chains
            .get(&PluginPositionKey(position))
            .map_or(0, |c| c.len())
    }

    /// Return a [`PluginChain`] for the given position.
    ///
    /// If no plugins are registered at `position`, the returned chain is
    /// empty and [`PluginChain::run`] will be a no-op passthrough.
    ///
    /// The returned chain is a **clone** — callers may cache it and call
    /// `run` concurrently without synchronisation.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn chain_for(&self, position: PluginPosition) -> PluginChain {
        self.chains
            .get(&PluginPositionKey(position))
            .cloned()
            .unwrap_or_else(|| PluginChain::new(position))
    }

    /// Return `true` if any plugins are registered at `position`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn has_plugins(&self, position: PluginPosition) -> bool {
        self.plugin_count(position) > 0
    }

    /// Return the total number of plugins across all positions.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn total_plugin_count(&self) -> usize {
        self.chains.values().map(|c| c.len()).sum()
    }

    /// Return a `Vec` of `(position_label, plugin_count)` for every registered
    /// position, sorted by label.  Useful for diagnostics and logging.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn summary(&self) -> Vec<(String, usize)> {
        let mut pairs: Vec<(String, usize)> = self
            .chains
            .iter()
            .map(|(k, c)| (k.0.label(), c.len()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    struct PassthroughPlugin;

    #[async_trait]
    impl StagePlugin for PassthroughPlugin {
        fn name(&self) -> &'static str {
            "passthrough"
        }
        async fn process(&self, input: PluginInput) -> PluginOutput {
            PluginOutput::passthrough(input)
        }
    }

    struct AbortPlugin;

    #[async_trait]
    impl StagePlugin for AbortPlugin {
        fn name(&self) -> &'static str {
            "abort"
        }
        async fn process(&self, input: PluginInput) -> PluginOutput {
            PluginOutput::abort(input)
        }
    }

    struct MetaMutatorPlugin;

    #[async_trait]
    impl StagePlugin for MetaMutatorPlugin {
        fn name(&self) -> &'static str {
            "meta-mutator"
        }
        async fn process(&self, mut input: PluginInput) -> PluginOutput {
            input.metadata.insert("mutated".to_string(), "yes".to_string());
            PluginOutput::passthrough(input)
        }
    }

    fn make_input() -> PluginInput {
        PluginInput {
            request_id: "req-test".to_string(),
            session_id: "session-test".to_string(),
            payload: serde_json::json!({"prompt": "hello"}),
            metadata: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn empty_chain_is_passthrough() {
        let chain = PluginChain::new(PluginPosition::Before(PipelineStage::Inference));
        let input = make_input();
        let output = chain.run(input).await;
        assert_eq!(output.status, PluginStatus::Continue);
    }

    #[tokio::test]
    async fn single_passthrough_plugin() {
        let mut chain = PluginChain::new(PluginPosition::Before(PipelineStage::Rag));
        chain.push(Arc::new(PassthroughPlugin));
        let output = chain.run(make_input()).await;
        assert_eq!(output.status, PluginStatus::Continue);
    }

    #[tokio::test]
    async fn abort_plugin_stops_chain() {
        let mut chain = PluginChain::new(PluginPosition::After(PipelineStage::Inference));
        chain.push(Arc::new(AbortPlugin));
        chain.push(Arc::new(PassthroughPlugin)); // should NOT run
        let output = chain.run(make_input()).await;
        assert_eq!(output.status, PluginStatus::Abort);
    }

    #[tokio::test]
    async fn meta_mutator_modifies_metadata() {
        let mut chain = PluginChain::new(PluginPosition::Before(PipelineStage::Post));
        chain.push(Arc::new(MetaMutatorPlugin));
        let output = chain.run(make_input()).await;
        assert_eq!(
            output.input.metadata.get("mutated").map(String::as_str),
            Some("yes")
        );
    }

    #[tokio::test]
    async fn registry_counts_correctly() {
        let mut registry = PluginRegistry::new();
        let pos = PluginPosition::Before(PipelineStage::Inference);
        registry.register(pos, Arc::new(PassthroughPlugin));
        registry.register(pos, Arc::new(PassthroughPlugin));
        assert_eq!(registry.plugin_count(pos), 2);
        assert_eq!(registry.total_plugin_count(), 2);
    }

    #[tokio::test]
    async fn registry_remove_all_clears_chain() {
        let mut registry = PluginRegistry::new();
        let pos = PluginPosition::After(PipelineStage::Rag);
        registry.register(pos, Arc::new(PassthroughPlugin));
        registry.remove_all(pos);
        assert_eq!(registry.plugin_count(pos), 0);
        // chain_for on an empty position is a no-op passthrough
        let chain = registry.chain_for(pos);
        let output = chain.run(make_input()).await;
        assert_eq!(output.status, PluginStatus::Continue);
    }

    #[tokio::test]
    async fn registry_summary_sorted() {
        let mut registry = PluginRegistry::new();
        registry.register(
            PluginPosition::Before(PipelineStage::Stream),
            Arc::new(PassthroughPlugin),
        );
        registry.register(
            PluginPosition::After(PipelineStage::Rag),
            Arc::new(PassthroughPlugin),
        );
        let summary = registry.summary();
        // Should be sorted alphabetically: "after:rag" < "before:stream"
        assert_eq!(summary[0].0, "after:rag");
        assert_eq!(summary[1].0, "before:stream");
    }

    #[test]
    fn plugin_position_label() {
        assert_eq!(
            PluginPosition::Before(PipelineStage::Inference).label(),
            "before:inference"
        );
        assert_eq!(
            PluginPosition::After(PipelineStage::Post).label(),
            "after:post"
        );
    }

    #[test]
    fn plugin_output_constructors() {
        let input = make_input();
        let out = PluginOutput::passthrough(input.clone());
        assert_eq!(out.status, PluginStatus::Continue);

        let out = PluginOutput::abort(input.clone());
        assert_eq!(out.status, PluginStatus::Abort);

        let out = PluginOutput::error(input.clone(), "oops");
        assert!(matches!(out.status, PluginStatus::Error(ref s) if s == "oops"));

        let modified = PluginOutput::modified(input, serde_json::json!({"x": 1}));
        assert_eq!(modified.input.payload, serde_json::json!({"x": 1}));
        assert_eq!(modified.status, PluginStatus::Continue);
    }
}
