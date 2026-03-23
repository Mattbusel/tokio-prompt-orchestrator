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
use std::time::Duration;
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
// PluginContext — passed to pre/post inference hooks
// ============================================================================

/// Metrics snapshot passed to pre- and post-inference hooks.
///
/// Values are best-effort estimates at the time the hook fires.  Hooks must
/// not modify any field — the struct is `Clone` for convenience only.
#[derive(Debug, Clone)]
pub struct HookMetrics {
    /// Elapsed wall-clock time since the request entered the pipeline (ms).
    pub elapsed_ms: u64,
    /// Approximate number of prompt tokens (provider-reported or estimated).
    pub prompt_tokens: Option<u64>,
    /// Approximate number of completion tokens.
    pub completion_tokens: Option<u64>,
    /// Estimated cost of this request in USD (0.0 if not available).
    pub estimated_cost_usd: f64,
}

/// Rich context passed to every pre- and post-inference hook.
///
/// Hooks receive a shared reference to this struct; they cannot modify the
/// pipeline payload directly — they should use the returned [`PluginOutput`]
/// mechanism for that.  The hook context is purely informational.
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// Unique ID for the in-flight request.
    pub request_id: String,
    /// Session the request belongs to.
    pub session_id: String,
    /// The raw prompt/payload as submitted to the pipeline stage.
    pub request_payload: Value,
    /// The model response, if available (only set for post-inference hooks).
    pub response_payload: Option<Value>,
    /// Performance / cost metrics for this request.
    pub metrics: HookMetrics,
    /// Arbitrary metadata forwarded from the original [`crate::PromptRequest`].
    pub metadata: HashMap<String, String>,
}

impl PluginContext {
    /// Build a pre-inference context (no response yet).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn pre_inference(
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        request_payload: Value,
        metadata: HashMap<String, String>,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            session_id: session_id.into(),
            request_payload,
            response_payload: None,
            metrics: HookMetrics {
                elapsed_ms,
                prompt_tokens: None,
                completion_tokens: None,
                estimated_cost_usd: 0.0,
            },
            metadata,
        }
    }

    /// Build a post-inference context (response is available).
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn post_inference(
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        request_payload: Value,
        response_payload: Value,
        metadata: HashMap<String, String>,
        elapsed_ms: u64,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        estimated_cost_usd: f64,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            session_id: session_id.into(),
            request_payload,
            response_payload: Some(response_payload),
            metrics: HookMetrics {
                elapsed_ms,
                prompt_tokens,
                completion_tokens,
                estimated_cost_usd,
            },
            metadata,
        }
    }

    /// Convert this context into a [`PluginInput`] for use with `PluginChain::run`.
    ///
    /// The `request_payload` becomes the chain `payload`; `response_payload`
    /// (if present) is injected into `metadata` under the key `"response_payload_json"`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    #[must_use]
    pub fn into_plugin_input(self) -> PluginInput {
        let mut metadata = self.metadata;
        if let Some(resp) = self.response_payload {
            metadata.insert(
                "response_payload_json".to_string(),
                resp.to_string(),
            );
        }
        metadata.insert(
            "elapsed_ms".to_string(),
            self.metrics.elapsed_ms.to_string(),
        );
        if let Some(pt) = self.metrics.prompt_tokens {
            metadata.insert("prompt_tokens".to_string(), pt.to_string());
        }
        if let Some(ct) = self.metrics.completion_tokens {
            metadata.insert("completion_tokens".to_string(), ct.to_string());
        }
        PluginInput {
            request_id: self.request_id,
            session_id: self.session_id,
            payload: self.request_payload,
            metadata,
        }
    }
}

// ============================================================================
// InferenceHook trait — simpler API for pre/post hooks
// ============================================================================

/// A focused hook trait for code that only needs to intercept inference
/// before or after it happens, without needing the full [`StagePlugin`]
/// position system.
///
/// Hooks are registered via [`PluginRegistry::register_pre_hook`] and
/// [`PluginRegistry::register_post_hook`].  They are called with a rich
/// [`PluginContext`] rather than the generic [`PluginInput`].
#[async_trait]
pub trait InferenceHook: Send + Sync {
    /// Human-readable name for logging/metrics.
    fn name(&self) -> &'static str;

    /// Called with the full inference context.  Return `Ok(())` to continue;
    /// return `Err(msg)` to abort the pipeline and return the error to the caller.
    ///
    /// # Errors
    ///
    /// Return an `Err` string to signal that the hook detected an unrecoverable
    /// problem (e.g., budget exceeded, PII detected).
    async fn call(&self, ctx: &PluginContext) -> Result<(), String>;
}

// ============================================================================
// PluginRegistry — extended with pre/post hook support
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
    /// Pre-inference hooks, executed in registration order before inference.
    pre_hooks: Vec<(String, Arc<dyn InferenceHook>)>,
    /// Post-inference hooks, executed in registration order after inference.
    post_hooks: Vec<(String, Arc<dyn InferenceHook>)>,
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

    /// Register a pre-inference hook by name.
    ///
    /// Pre-hooks are called in registration order immediately before inference.
    /// If any hook returns `Err`, the pipeline aborts with that error message.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn register_pre_hook(&mut self, hook: Arc<dyn InferenceHook>) {
        let name = hook.name().to_string();
        debug!(hook = %name, "registered pre-inference hook");
        self.pre_hooks.push((name, hook));
    }

    /// Register a post-inference hook by name.
    ///
    /// Post-hooks are called in registration order immediately after a
    /// successful inference.  Errors are logged but do not abort the response.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn register_post_hook(&mut self, hook: Arc<dyn InferenceHook>) {
        let name = hook.name().to_string();
        debug!(hook = %name, "registered post-inference hook");
        self.post_hooks.push((name, hook));
    }

    /// Unregister a plugin by name from the pre-hook list.
    ///
    /// Removes the first matching hook. No-op if the name is not found.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn unregister_pre_hook(&mut self, name: &str) {
        self.pre_hooks.retain(|(n, _)| n != name);
    }

    /// Unregister a plugin by name from the post-hook list.
    ///
    /// Removes the first matching hook. No-op if the name is not found.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn unregister_post_hook(&mut self, name: &str) {
        self.post_hooks.retain(|(n, _)| n != name);
    }

    /// Run all registered pre-inference hooks in order.
    ///
    /// Returns `Ok(())` if all hooks pass.  Returns `Err(message)` at the
    /// first hook that signals a failure; remaining hooks are not called.
    ///
    /// Each hook is run with a per-hook timeout of `hook_timeout` to prevent
    /// a slow hook from blocking the pipeline indefinitely.
    ///
    /// # Errors
    ///
    /// Returns the error message from the first failing hook.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn run_pre_hooks(
        &self,
        ctx: &PluginContext,
        hook_timeout: Duration,
    ) -> Result<(), String> {
        for (name, hook) in &self.pre_hooks {
            let result = tokio::time::timeout(hook_timeout, hook.call(ctx)).await;
            match result {
                Ok(Ok(())) => {
                    debug!(hook = %name, "pre-inference hook passed");
                }
                Ok(Err(msg)) => {
                    warn!(hook = %name, error = %msg, "pre-inference hook rejected request");
                    return Err(msg);
                }
                Err(_elapsed) => {
                    let msg = format!("pre-inference hook '{name}' timed out");
                    warn!(hook = %name, "pre-inference hook timed out");
                    return Err(msg);
                }
            }
        }
        Ok(())
    }

    /// Run all registered post-inference hooks in order.
    ///
    /// Unlike pre-hooks, post-hook errors are logged but do not abort the
    /// caller — the response has already been produced.  Each hook still
    /// receives the full [`PluginContext`] (including `response_payload`).
    ///
    /// Each hook is run with a per-hook timeout of `hook_timeout`.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub async fn run_post_hooks(&self, ctx: &PluginContext, hook_timeout: Duration) {
        for (name, hook) in &self.post_hooks {
            let result = tokio::time::timeout(hook_timeout, hook.call(ctx)).await;
            match result {
                Ok(Ok(())) => {
                    debug!(hook = %name, "post-inference hook completed");
                }
                Ok(Err(msg)) => {
                    warn!(hook = %name, error = %msg, "post-inference hook reported error (non-fatal)");
                }
                Err(_elapsed) => {
                    warn!(hook = %name, "post-inference hook timed out (non-fatal)");
                }
            }
        }
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

    /// Return the number of registered pre-inference hooks.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn pre_hook_count(&self) -> usize {
        self.pre_hooks.len()
    }

    /// Return the number of registered post-inference hooks.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn post_hook_count(&self) -> usize {
        self.post_hooks.len()
    }
}

// ============================================================================
// Round-7 Plugin system: Plugin trait, PluginV2Chain, PluginV2Registry,
// PluginError, PluginInfo, and built-in plugins.
// ============================================================================

use crate::PromptRequest;

/// Error returned by [`Plugin`] hooks.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The plugin determined that the request should be rejected outright.
    #[error("request rejected by plugin: {reason}")]
    RequestRejected {
        /// Human-readable explanation of why the request was rejected.
        reason: String,
    },
    /// The plugin modified the response (informational, not fatal).
    #[error("response modified by plugin")]
    ResponseModified,
    /// An unrecoverable error occurred inside the plugin.
    #[error("plugin fatal error: {0}")]
    Fatal(String),
}

/// Snapshot of per-plugin call statistics.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// Plugin name as returned by [`Plugin::name`].
    pub name: String,
    /// Plugin version as returned by [`Plugin::version`].
    pub version: String,
    /// Whether the plugin is currently enabled.
    pub enabled: bool,
    /// Number of times `on_request` was invoked.
    pub request_calls: u64,
    /// Number of times `on_response` was invoked.
    pub response_calls: u64,
    /// Total number of errors returned from either hook.
    pub errors: u64,
}

/// Core trait for request/response plugins.
///
/// Implement this trait and register it with a [`PluginV2Registry`] to
/// intercept requests before inference and responses after inference.
pub trait Plugin: Send + Sync {
    /// Unique human-readable name, e.g. `"profanity-filter"`.
    fn name(&self) -> &str;
    /// SemVer string, e.g. `"1.0.0"`.
    fn version(&self) -> &str;
    /// Inspect and/or mutate the request before it reaches inference.
    ///
    /// Return `Err(PluginError::RequestRejected { .. })` to abort the request.
    fn on_request(&self, req: &mut PromptRequest) -> Result<(), PluginError>;
    /// Inspect and/or mutate the response tokens after inference.
    ///
    /// Return `Err(PluginError::ResponseModified)` or `Err(PluginError::Fatal)`
    /// to signal post-processing issues (pipeline may log and continue).
    fn on_response(&self, resp: &mut Vec<String>) -> Result<(), PluginError>;
}

/// Internal entry in [`PluginV2Registry`].
struct PluginEntry {
    plugin: Box<dyn Plugin>,
    enabled: bool,
    request_calls: u64,
    response_calls: u64,
    errors: u64,
}

/// Ordered list of [`Plugin`]s executed sequentially on each request/response.
///
/// Plugins run in insertion order.  If any `on_request` hook returns
/// `Err(PluginError::RequestRejected)`, the chain stops and the error is
/// propagated.  `on_response` errors are collected but do not short-circuit
/// the remaining response plugins.
#[derive(Default)]
pub struct PluginV2Chain {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginV2Chain {
    /// Create an empty chain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a plugin to the end of the chain.
    pub fn push(&mut self, plugin: Box<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    /// Run `on_request` for all plugins in order.
    ///
    /// Stops at the first `RequestRejected` error.
    pub fn run_request(&self, req: &mut PromptRequest) -> Result<(), PluginError> {
        for plugin in &self.plugins {
            plugin.on_request(req)?;
        }
        Ok(())
    }

    /// Run `on_response` for all plugins in order.
    ///
    /// Continues even if individual plugins return an error (errors are
    /// returned at the end as the last encountered error).
    pub fn run_response(&self, resp: &mut Vec<String>) -> Result<(), PluginError> {
        let mut last_err: Option<PluginError> = None;
        for plugin in &self.plugins {
            if let Err(e) = plugin.on_response(resp) {
                last_err = Some(e);
            }
        }
        match last_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    /// Return the number of plugins in this chain.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Return `true` if no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Runtime registry of [`Plugin`] instances with enable/disable support and
/// per-plugin call statistics.
#[derive(Default)]
pub struct PluginV2Registry {
    entries: Vec<PluginEntry>,
}

impl PluginV2Registry {
    /// Create a new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a plugin.  Plugins execute in registration order.
    pub fn register(&mut self, plugin: Box<dyn Plugin>) {
        self.entries.push(PluginEntry {
            plugin,
            enabled: true,
            request_calls: 0,
            response_calls: 0,
            errors: 0,
        });
    }

    /// Disable a plugin by name.  Disabled plugins are skipped during
    /// `run_request_hooks` and `run_response_hooks` but remain registered.
    ///
    /// No-op if the name is not found.
    pub fn disable(&mut self, name: &str) {
        for entry in &mut self.entries {
            if entry.plugin.name() == name {
                entry.enabled = false;
            }
        }
    }

    /// Re-enable a plugin that was previously disabled.
    ///
    /// No-op if the name is not found.
    pub fn enable(&mut self, name: &str) {
        for entry in &mut self.entries {
            if entry.plugin.name() == name {
                entry.enabled = true;
            }
        }
    }

    /// List all registered plugins with their current statistics.
    pub fn list(&self) -> Vec<PluginInfo> {
        self.entries
            .iter()
            .map(|e| PluginInfo {
                name: e.plugin.name().to_string(),
                version: e.plugin.version().to_string(),
                enabled: e.enabled,
                request_calls: e.request_calls,
                response_calls: e.response_calls,
                errors: e.errors,
            })
            .collect()
    }

    /// Run `on_request` for all enabled plugins in registration order.
    ///
    /// Stops at the first `RequestRejected` error, records the error in stats.
    pub fn run_request_hooks(&mut self, req: &mut PromptRequest) -> Result<(), PluginError> {
        for entry in &mut self.entries {
            if !entry.enabled {
                continue;
            }
            entry.request_calls += 1;
            if let Err(e) = entry.plugin.on_request(req) {
                entry.errors += 1;
                return Err(e);
            }
        }
        Ok(())
    }

    /// Run `on_response` for all enabled plugins in registration order.
    ///
    /// Does not short-circuit on error — all enabled plugins run.  The last
    /// error encountered is returned, if any.
    pub fn run_response_hooks(&mut self, resp: &mut Vec<String>) -> Result<(), PluginError> {
        let mut last_err: Option<PluginError> = None;
        for entry in &mut self.entries {
            if !entry.enabled {
                continue;
            }
            entry.response_calls += 1;
            if let Err(e) = entry.plugin.on_response(resp) {
                entry.errors += 1;
                last_err = Some(e);
            }
        }
        match last_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }
}

// ============================================================================
// Built-in plugins
// ============================================================================

/// A plugin that blocks requests containing any word from a configurable list.
///
/// The check is case-insensitive.
pub struct ProfanityFilterPlugin {
    /// Words that, if present in the prompt, cause the request to be rejected.
    pub word_list: Vec<String>,
}

impl ProfanityFilterPlugin {
    /// Create a new filter with the given list of blocked words.
    pub fn new(words: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            word_list: words.into_iter().map(Into::into).collect(),
        }
    }
}

impl Plugin for ProfanityFilterPlugin {
    fn name(&self) -> &str {
        "profanity-filter"
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn on_request(&self, req: &mut PromptRequest) -> Result<(), PluginError> {
        let lower = req.input.to_lowercase();
        for word in &self.word_list {
            if lower.contains(word.to_lowercase().as_str()) {
                return Err(PluginError::RequestRejected {
                    reason: format!("prompt contains blocked word: {word}"),
                });
            }
        }
        Ok(())
    }
    fn on_response(&self, _resp: &mut Vec<String>) -> Result<(), PluginError> {
        Ok(())
    }
}

/// A plugin that truncates response token lists to at most `max_tokens` items.
pub struct ResponseLengthCapPlugin {
    /// Maximum number of token strings to keep in the response.
    pub max_tokens: usize,
}

impl ResponseLengthCapPlugin {
    /// Create a new cap plugin.
    pub fn new(max_tokens: usize) -> Self {
        Self { max_tokens }
    }
}

impl Plugin for ResponseLengthCapPlugin {
    fn name(&self) -> &str {
        "response-length-cap"
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn on_request(&self, _req: &mut PromptRequest) -> Result<(), PluginError> {
        Ok(())
    }
    fn on_response(&self, resp: &mut Vec<String>) -> Result<(), PluginError> {
        if resp.len() > self.max_tokens {
            resp.truncate(self.max_tokens);
            return Err(PluginError::ResponseModified);
        }
        Ok(())
    }
}

/// A plugin that records per-request start/end timestamps (milliseconds since
/// Unix epoch) into a shared latency log.
///
/// Call [`LatencyLoggerPlugin::record_start`] before inference and
/// [`LatencyLoggerPlugin::record_end`] after inference to append the elapsed
/// duration to the log.
pub struct LatencyLoggerPlugin {
    /// Accumulated latency samples in milliseconds.
    pub log: std::sync::Mutex<Vec<u64>>,
}

impl LatencyLoggerPlugin {
    /// Create a new logger with an empty latency log.
    pub fn new() -> Self {
        Self {
            log: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Record a completed request's latency in milliseconds.
    pub fn record_latency(&self, latency_ms: u64) {
        if let Ok(mut guard) = self.log.lock() {
            guard.push(latency_ms);
        }
    }

    /// Return a copy of all recorded latencies.
    pub fn samples(&self) -> Vec<u64> {
        self.log.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl Default for LatencyLoggerPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for LatencyLoggerPlugin {
    fn name(&self) -> &str {
        "latency-logger"
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn on_request(&self, _req: &mut PromptRequest) -> Result<(), PluginError> {
        // Latency is recorded externally via record_latency().
        Ok(())
    }
    fn on_response(&self, _resp: &mut Vec<String>) -> Result<(), PluginError> {
        Ok(())
    }
}

// ============================================================================
// Tests — Round 7 Plugin system (20+ unit tests)
// ============================================================================

#[cfg(test)]
mod plugin_v2_tests {
    use super::*;
    use std::collections::HashMap;

    fn make_req(input: &str) -> PromptRequest {
        PromptRequest {
            session: crate::SessionId::new("s1"),
            request_id: "r1".to_string(),
            input: input.to_string(),
            meta: HashMap::new(),
            deadline: None,
        }
    }

    // ---- ProfanityFilterPlugin ----

    #[test]
    fn profanity_filter_rejects_blocked_word() {
        let p = ProfanityFilterPlugin::new(["badword"]);
        let mut req = make_req("this has badword in it");
        assert!(matches!(
            p.on_request(&mut req),
            Err(PluginError::RequestRejected { .. })
        ));
    }

    #[test]
    fn profanity_filter_case_insensitive() {
        let p = ProfanityFilterPlugin::new(["BadWord"]);
        let mut req = make_req("BADWORD is uppercase");
        assert!(matches!(
            p.on_request(&mut req),
            Err(PluginError::RequestRejected { .. })
        ));
    }

    #[test]
    fn profanity_filter_passes_clean_request() {
        let p = ProfanityFilterPlugin::new(["blocked"]);
        let mut req = make_req("this is totally fine");
        assert!(p.on_request(&mut req).is_ok());
    }

    #[test]
    fn profanity_filter_empty_word_list_passes() {
        let p = ProfanityFilterPlugin::new([] as [&str; 0]);
        let mut req = make_req("anything goes");
        assert!(p.on_request(&mut req).is_ok());
    }

    #[test]
    fn profanity_filter_on_response_is_noop() {
        let p = ProfanityFilterPlugin::new(["x"]);
        let mut resp = vec!["a".to_string()];
        assert!(p.on_response(&mut resp).is_ok());
    }

    // ---- ResponseLengthCapPlugin ----

    #[test]
    fn length_cap_truncates_over_limit() {
        let p = ResponseLengthCapPlugin::new(3);
        let mut resp: Vec<String> = (0..5).map(|i| i.to_string()).collect();
        let result = p.on_response(&mut resp);
        assert!(matches!(result, Err(PluginError::ResponseModified)));
        assert_eq!(resp.len(), 3);
    }

    #[test]
    fn length_cap_passes_exact_limit() {
        let p = ResponseLengthCapPlugin::new(3);
        let mut resp: Vec<String> = (0..3).map(|i| i.to_string()).collect();
        assert!(p.on_response(&mut resp).is_ok());
        assert_eq!(resp.len(), 3);
    }

    #[test]
    fn length_cap_passes_under_limit() {
        let p = ResponseLengthCapPlugin::new(10);
        let mut resp = vec!["a".to_string(), "b".to_string()];
        assert!(p.on_response(&mut resp).is_ok());
    }

    #[test]
    fn length_cap_on_request_is_noop() {
        let p = ResponseLengthCapPlugin::new(1);
        let mut req = make_req("hello");
        assert!(p.on_request(&mut req).is_ok());
    }

    // ---- LatencyLoggerPlugin ----

    #[test]
    fn latency_logger_records_samples() {
        let p = LatencyLoggerPlugin::new();
        p.record_latency(42);
        p.record_latency(99);
        let samples = p.samples();
        assert_eq!(samples, vec![42, 99]);
    }

    #[test]
    fn latency_logger_on_request_ok() {
        let p = LatencyLoggerPlugin::new();
        let mut req = make_req("hello");
        assert!(p.on_request(&mut req).is_ok());
    }

    #[test]
    fn latency_logger_on_response_ok() {
        let p = LatencyLoggerPlugin::new();
        let mut resp = vec!["tok".to_string()];
        assert!(p.on_response(&mut resp).is_ok());
    }

    // ---- PluginV2Registry ----

    #[test]
    fn registry_list_returns_all() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ProfanityFilterPlugin::new(["x"])));
        reg.register(Box::new(ResponseLengthCapPlugin::new(10)));
        let info = reg.list();
        assert_eq!(info.len(), 2);
        assert_eq!(info[0].name, "profanity-filter");
        assert_eq!(info[1].name, "response-length-cap");
    }

    #[test]
    fn registry_disable_skips_plugin() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ProfanityFilterPlugin::new(["bad"])));
        reg.disable("profanity-filter");
        let mut req = make_req("bad content here");
        // Disabled plugin should not reject.
        assert!(reg.run_request_hooks(&mut req).is_ok());
    }

    #[test]
    fn registry_enable_reenables_plugin() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ProfanityFilterPlugin::new(["bad"])));
        reg.disable("profanity-filter");
        reg.enable("profanity-filter");
        let mut req = make_req("bad content");
        assert!(matches!(
            reg.run_request_hooks(&mut req),
            Err(PluginError::RequestRejected { .. })
        ));
    }

    #[test]
    fn registry_tracks_request_calls() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ProfanityFilterPlugin::new([] as [&str; 0])));
        let mut req = make_req("hello");
        reg.run_request_hooks(&mut req).ok();
        reg.run_request_hooks(&mut req).ok();
        let info = reg.list();
        assert_eq!(info[0].request_calls, 2);
    }

    #[test]
    fn registry_tracks_response_calls() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ResponseLengthCapPlugin::new(100)));
        let mut resp = vec!["tok".to_string()];
        reg.run_response_hooks(&mut resp).ok();
        let info = reg.list();
        assert_eq!(info[0].response_calls, 1);
    }

    #[test]
    fn registry_tracks_errors() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ProfanityFilterPlugin::new(["bad"])));
        let mut req = make_req("bad");
        reg.run_request_hooks(&mut req).ok();
        let info = reg.list();
        assert_eq!(info[0].errors, 1);
    }

    #[test]
    fn registry_run_response_hooks_all_plugins_run_despite_error() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(ResponseLengthCapPlugin::new(0)));
        reg.register(Box::new(ResponseLengthCapPlugin::new(100)));
        let mut resp = vec!["tok1".to_string(), "tok2".to_string()];
        // First plugin truncates and returns Err; second should still run.
        let _ = reg.run_response_hooks(&mut resp);
        let info = reg.list();
        assert_eq!(info[0].response_calls, 1);
        assert_eq!(info[1].response_calls, 1);
    }

    #[test]
    fn registry_empty_runs_ok() {
        let mut reg = PluginV2Registry::new();
        let mut req = make_req("hello");
        assert!(reg.run_request_hooks(&mut req).is_ok());
        let mut resp = vec![];
        assert!(reg.run_response_hooks(&mut resp).is_ok());
    }

    #[test]
    fn plugin_info_enabled_flag() {
        let mut reg = PluginV2Registry::new();
        reg.register(Box::new(LatencyLoggerPlugin::new()));
        reg.disable("latency-logger");
        let info = reg.list();
        assert!(!info[0].enabled);
        reg.enable("latency-logger");
        let info = reg.list();
        assert!(info[0].enabled);
    }

    #[test]
    fn plugin_chain_v2_run_request_all_pass() {
        let mut chain = PluginV2Chain::new();
        chain.push(Box::new(ProfanityFilterPlugin::new([] as [&str; 0])));
        chain.push(Box::new(ResponseLengthCapPlugin::new(100)));
        let mut req = make_req("clean request");
        assert!(chain.run_request(&mut req).is_ok());
    }

    #[test]
    fn plugin_chain_v2_run_request_stops_on_rejection() {
        let mut chain = PluginV2Chain::new();
        chain.push(Box::new(ProfanityFilterPlugin::new(["bad"])));
        chain.push(Box::new(ProfanityFilterPlugin::new(["other"])));
        let mut req = make_req("bad content");
        assert!(matches!(
            chain.run_request(&mut req),
            Err(PluginError::RequestRejected { .. })
        ));
    }

    #[test]
    fn plugin_chain_v2_len_is_empty() {
        let chain = PluginV2Chain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
    }

    #[test]
    fn plugin_error_display() {
        let e = PluginError::RequestRejected {
            reason: "bad".to_string(),
        };
        assert!(e.to_string().contains("bad"));
        let e2 = PluginError::ResponseModified;
        assert!(e2.to_string().contains("modified"));
        let e3 = PluginError::Fatal("crash".to_string());
        assert!(e3.to_string().contains("crash"));
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
