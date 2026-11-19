//! Model routing: which upstream answers which model name.
//!
//! A static `[[models]]` table maps model names, or globs such as `gpt-*`,
//! `claude-*` or `llama-*`, to providers. Each route gets its own pipeline
//! (queue, circuit breaker, timeout, retries, concurrency limit), so a
//! failing provider only opens its own breaker.
//!
//! ```toml
//! # models.toml, or the same tables inside a pipeline TOML
//! default_route = true        # unknown models go to --provider/--model (the default)
//!
//! [[models]]
//! name = "gpt-*"              # glob: * is any run of characters, ? one character
//! provider = "openai"         # any OpenAI-compatible server
//! # base_url = "https://openrouter.ai/api/v1"
//! # api_key_env = "OPENROUTER_API_KEY"
//!
//! [[models]]
//! name = "claude-*"
//! provider = "anthropic"
//! max_tokens = 8192           # sent when the client gives no max_tokens
//!
//! [[models]]
//! name = "llama-3.1-8b"       # an exact name
//! provider = "llama_cpp"
//! base_url = "http://gpu-box:8080"
//! ```
//!
//! Matching is case-insensitive. Exact names are tried first, then globs in
//! table order. A glob route sends the client's model name upstream; set
//! `model` on a route to send a fixed name instead.
//!
//! With `default_route = true` (the default) a model that matches no route,
//! or no model at all, goes to the server's own pipeline: the single
//! `--provider` / `--model` the orchestrator was started with, exactly as
//! before routing existed. With `default_route = false` such a request is
//! answered 400 with the list of configured models.
//!
//! Load a table with [`ModelRoutesConfig::from_file`] (the CLI's
//! `--models <file>`) and start its pipelines with [`ModelRoutes::spawn`];
//! hand the result to the web server through
//! `web_api::ServerConfig::model_routes`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::stages::{spawn_pipeline_with, PipelineHandles, PipelineOptions};
use crate::{
    AnthropicWorker, EchoWorker, LlamaCppWorker, ModelWorker, OpenAiWorker, OrchestratorError,
};

/// The kind of upstream a route talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteProvider {
    /// Any OpenAI-compatible Chat Completions server: OpenAI itself,
    /// OpenRouter, Groq, Together, vLLM, LM Studio, Ollama's `/v1`.
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic's Messages API.
    Anthropic,
    /// A llama.cpp server (its OpenAI-compatible `/v1/chat/completions`).
    LlamaCpp,
    /// Echo worker: answers with the prompt. For tests and offline setups.
    Echo,
}

impl RouteProvider {
    /// The provider's name as written in config and reported by `/v1/models`.
    pub fn as_str(self) -> &'static str {
        match self {
            RouteProvider::OpenAi => "openai",
            RouteProvider::Anthropic => "anthropic",
            RouteProvider::LlamaCpp => "llama_cpp",
            RouteProvider::Echo => "echo",
        }
    }

    /// `true` for providers that do not bill per token (llama.cpp, echo).
    pub fn is_free(self) -> bool {
        matches!(self, RouteProvider::LlamaCpp | RouteProvider::Echo)
    }
}

impl std::fmt::Display for RouteProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RouteProvider {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.to_ascii_lowercase().replace(['-', '.'], "_").as_str() {
            "openai" | "openai_compatible" | "open_ai" => Ok(RouteProvider::OpenAi),
            "anthropic" => Ok(RouteProvider::Anthropic),
            "llama_cpp" | "llama" | "llamacpp" => Ok(RouteProvider::LlamaCpp),
            "echo" => Ok(RouteProvider::Echo),
            other => Err(serde::de::Error::custom(format!(
                "unknown provider {other:?}; expected one of: openai (any OpenAI-compatible server), anthropic, llama_cpp, echo"
            ))),
        }
    }
}

/// One `[[models]]` entry: a model name or glob and the upstream serving it.
///
/// # Examples
///
/// ```
/// use tokio_prompt_orchestrator::{ModelRoute, RouteProvider};
///
/// let r = ModelRoute::new("gpt-*", RouteProvider::OpenAi)
///     .with_base_url("http://localhost:8000/v1");
/// assert!(r.matches("gpt-4o-mini"));
/// assert!(!r.matches("claude-sonnet-4-5"));
/// assert_eq!(r.upstream_model("gpt-4o-mini"), "gpt-4o-mini");
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ModelRoute {
    /// Model name, or a glob (`*` any run of characters, `?` one character).
    pub name: String,
    /// The kind of upstream.
    pub provider: RouteProvider,
    /// Base URL of the upstream. Defaults: `https://api.openai.com/v1`,
    /// `https://api.anthropic.com/v1`, and `LLAMA_CPP_URL` or
    /// `http://localhost:8080` for llama.cpp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Environment variable holding the API key. Defaults to
    /// `OPENAI_API_KEY` for `openai` without a `base_url` and
    /// `ANTHROPIC_API_KEY` for `anthropic`; an OpenAI-compatible route with
    /// a `base_url` and no `api_key_env` sends no key (local servers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Model name sent upstream instead of the client's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reply cap sent when the client sends no `max_tokens`. Anthropic
    /// requires one, so Anthropic routes default to 4096; other routes send
    /// none by default and the model's own limit applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Per-call upstream timeout for this route, in seconds. Defaults to the
    /// pipeline's, and for Anthropic routes to at least
    /// [`ANTHROPIC_DEFAULT_TIMEOUT_SECS`] (see
    /// [`effective_timeout`](Self::effective_timeout)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// Reply cap for Anthropic routes when neither the client nor the route
/// sets one (the Messages API requires a value).
pub const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 4096;

/// Shortest per-call timeout an Anthropic route gets when it sets no
/// `timeout_secs`: Claude answers of [`ANTHROPIC_DEFAULT_MAX_TOKENS`] tokens
/// often take longer than the 30 s used for other providers. The CLI gives
/// `--provider anthropic` the same 60 s.
pub const ANTHROPIC_DEFAULT_TIMEOUT_SECS: u64 = 60;

impl ModelRoute {
    /// A route for `name` (exact or glob) to `provider`, with defaults.
    pub fn new(name: impl Into<String>, provider: RouteProvider) -> Self {
        Self {
            name: name.into(),
            provider,
            base_url: None,
            api_key_env: None,
            model: None,
            max_tokens: None,
            timeout_secs: None,
        }
    }

    /// Set the upstream base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Read the API key from environment variable `var`.
    pub fn with_api_key_env(mut self, var: impl Into<String>) -> Self {
        self.api_key_env = Some(var.into());
        self
    }

    /// Send `model` upstream instead of the client's model name.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Reply cap used when the client sends none.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Per-call upstream timeout for this route, in seconds.
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    /// The per-call timeout this route's pipeline uses when the server's own
    /// pipeline uses `pipeline_default`: the route's `timeout_secs` when set;
    /// otherwise `pipeline_default`, raised to
    /// [`ANTHROPIC_DEFAULT_TIMEOUT_SECS`] for an Anthropic route.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use tokio_prompt_orchestrator::{ModelRoute, RouteProvider};
    ///
    /// let thirty = Duration::from_secs(30);
    /// let claude = ModelRoute::new("claude-*", RouteProvider::Anthropic);
    /// assert_eq!(claude.effective_timeout(thirty), Duration::from_secs(60));
    /// let gpt = ModelRoute::new("gpt-*", RouteProvider::OpenAi);
    /// assert_eq!(gpt.effective_timeout(thirty), thirty);
    /// assert_eq!(gpt.with_timeout_secs(90).effective_timeout(thirty), Duration::from_secs(90));
    /// ```
    pub fn effective_timeout(&self, pipeline_default: Duration) -> Duration {
        match (self.timeout_secs, self.provider) {
            (Some(secs), _) => Duration::from_secs(secs),
            (None, RouteProvider::Anthropic) => {
                pipeline_default.max(Duration::from_secs(ANTHROPIC_DEFAULT_TIMEOUT_SECS))
            }
            (None, _) => pipeline_default,
        }
    }

    /// `true` when `name` contains `*` or `?`.
    pub fn is_glob(&self) -> bool {
        self.name.contains(['*', '?'])
    }

    /// `true` when this route serves `model` (case-insensitive).
    pub fn matches(&self, model: &str) -> bool {
        glob_match(&self.name.to_ascii_lowercase(), &model.to_ascii_lowercase())
    }

    /// The model name sent upstream for a client asking for `requested`.
    pub fn upstream_model(&self, requested: &str) -> String {
        self.model.clone().unwrap_or_else(|| requested.to_string())
    }

    /// The environment variable the key is read from, if any.
    pub fn key_variable(&self) -> Option<String> {
        if let Some(var) = &self.api_key_env {
            return Some(var.clone()).filter(|v| !v.is_empty());
        }
        match self.provider {
            RouteProvider::OpenAi if self.base_url.is_none() => Some("OPENAI_API_KEY".into()),
            RouteProvider::Anthropic => Some("ANTHROPIC_API_KEY".into()),
            _ => None,
        }
    }

    /// Check the entry on its own (name, URL).
    ///
    /// # Errors
    ///
    /// A description of the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("a [[models]] entry has an empty name".into());
        }
        if let Some(url) = &self.base_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "model route '{}': base_url must start with http:// or https://, got '{url}'",
                    self.name
                ));
            }
        }
        if self.timeout_secs == Some(0) {
            return Err(format!("model route '{}': timeout_secs must be at least 1", self.name));
        }
        Ok(())
    }

    /// Build the worker for this route. `timeout` is the per-call HTTP
    /// timeout.
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] when the route's key variable is
    /// unset or empty.
    pub fn build_worker(&self, timeout: Duration) -> Result<Arc<dyn ModelWorker>, OrchestratorError> {
        let key = match self.key_variable() {
            Some(var) => Some(
                std::env::var(&var)
                    .ok()
                    .filter(|k| !k.is_empty())
                    .ok_or_else(|| {
                        OrchestratorError::ConfigError(format!(
                            "model route '{}' needs an API key in the environment variable {var}, which is not set",
                            self.name
                        ))
                    })?,
            ),
            None => None,
        };
        let worker_model = self.model.clone().unwrap_or_else(|| self.name.clone());
        let base = self.base_url.as_deref().map(|u| u.trim_end_matches('/').to_string());
        Ok(match self.provider {
            RouteProvider::OpenAi => {
                let mut w = OpenAiWorker::with_key(worker_model, key.unwrap_or_default())
                    .with_timeout(timeout);
                if let Some(base) = base {
                    w = w.with_base_url(base);
                }
                if let Some(n) = self.max_tokens {
                    w = w.with_max_tokens(n);
                }
                Arc::new(w)
            }
            RouteProvider::Anthropic => {
                let mut w = AnthropicWorker::with_key(worker_model, key.unwrap_or_default())
                    .with_timeout(timeout)
                    .with_max_tokens(self.max_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS));
                if let Some(base) = base {
                    w = w.with_base_url(base);
                }
                Arc::new(w)
            }
            RouteProvider::LlamaCpp => {
                let mut w = LlamaCppWorker::new().with_timeout(timeout);
                if let Some(base) = base {
                    w = w.with_url(base);
                }
                if let Some(n) = self.max_tokens.and_then(|n| i32::try_from(n).ok()) {
                    w = w.with_max_tokens(n);
                }
                Arc::new(w)
            }
            RouteProvider::Echo => Arc::new(EchoWorker::new()),
        })
    }
}

/// `true` when `text` matches `pattern` (`*` any run, `?` one character).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn default_true() -> bool {
    true
}

/// The `[[models]]` table plus `default_route`, as read from TOML.
///
/// # Examples
///
/// ```
/// use tokio_prompt_orchestrator::ModelRoutesConfig;
///
/// let cfg = ModelRoutesConfig::from_toml_str(r#"
///     default_route = false
///     [[models]]
///     name = "gpt-*"
///     provider = "openai"
///     base_url = "http://localhost:8000/v1"
/// "#).unwrap();
/// assert_eq!(cfg.models.len(), 1);
/// assert!(!cfg.default_route);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRoutesConfig {
    /// Routes, tried exact names first, then globs in this order.
    #[serde(default)]
    pub models: Vec<ModelRoute>,
    /// Send models that match no route to the server's own pipeline (the
    /// single `--provider` / `--model`). Default `true`.
    #[serde(default = "default_true")]
    pub default_route: bool,
}

impl Default for ModelRoutesConfig {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            default_route: true,
        }
    }
}

impl ModelRoutesConfig {
    /// Routes from `models` with the default route on.
    pub fn new(models: Vec<ModelRoute>) -> Self {
        Self {
            models,
            default_route: true,
        }
    }

    /// Read `[[models]]` and `default_route` from TOML. Every other key is
    /// ignored, so a pipeline TOML works as well as a file of its own.
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] for malformed TOML, an unknown
    /// field or provider in a `[[models]]` entry, or a failed
    /// [`validate`](Self::validate).
    pub fn from_toml_str(content: &str) -> Result<Self, OrchestratorError> {
        let table: toml::Table = content
            .parse()
            .map_err(|e| OrchestratorError::ConfigError(format!("model routes: {e}")))?;
        Self::from_table(&table)
    }

    /// Read `[[models]]` and `default_route` from an already parsed table.
    ///
    /// # Errors
    ///
    /// As [`from_toml_str`](Self::from_toml_str).
    pub fn from_table(table: &toml::Table) -> Result<Self, OrchestratorError> {
        let mut own = toml::Table::new();
        for key in ["models", "default_route"] {
            if let Some(v) = table.get(key) {
                own.insert(key.to_string(), v.clone());
            }
        }
        let config: Self = toml::Value::Table(own)
            .try_into()
            .map_err(|e| OrchestratorError::ConfigError(format!("model routes: {e}")))?;
        config.validate().map_err(OrchestratorError::ConfigError)?;
        Ok(config)
    }

    /// Read a TOML file (a models file or a pipeline TOML).
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] when the file cannot be read or
    /// [`from_toml_str`](Self::from_toml_str) fails.
    pub fn from_file(path: &Path) -> Result<Self, OrchestratorError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            OrchestratorError::ConfigError(format!("cannot read {}: {e}", path.display()))
        })?;
        Self::from_toml_str(&content).map_err(|e| match e {
            OrchestratorError::ConfigError(m) => {
                OrchestratorError::ConfigError(format!("{}: {m}", path.display()))
            }
            other => other,
        })
    }

    /// The longest per-call timeout among the routes (each as
    /// [`ModelRoute::effective_timeout`] gives it) and `pipeline_default`,
    /// the server's own pipeline. Size how long the HTTP handler waits for
    /// an answer from this, so a route with a long timeout is not cut off
    /// by the web server first.
    pub fn longest_timeout(&self, pipeline_default: Duration) -> Duration {
        self.models
            .iter()
            .map(|r| r.effective_timeout(pipeline_default))
            .fold(pipeline_default, Duration::max)
    }

    /// Check every entry and that no name appears twice.
    ///
    /// # Errors
    ///
    /// A description of the first problem found.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for route in &self.models {
            route.validate()?;
            if !seen.insert(route.name.to_ascii_lowercase()) {
                return Err(format!("model route '{}' is listed twice", route.name));
            }
        }
        Ok(())
    }
}

/// Route entries with their running pipelines.
struct RouteEntry {
    route: ModelRoute,
    handles: PipelineHandles,
}

/// Configured model routes, each with its own running pipeline.
///
/// Build one with [`spawn`](Self::spawn) (or
/// [`from_pipelines`](Self::from_pipelines) for pipelines you started
/// yourself) and pass it to the web server as
/// `web_api::ServerConfig::model_routes`.
pub struct ModelRoutes {
    entries: Vec<RouteEntry>,
    default_route: bool,
}

impl std::fmt::Debug for ModelRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRoutes")
            .field("routes", &self.entries.iter().map(|e| &e.route).collect::<Vec<_>>())
            .field("default_route", &self.default_route)
            .finish()
    }
}

impl ModelRoutes {
    /// Build every route's worker and start one pipeline per route with
    /// `options`. Each route's inference timeout is
    /// [`ModelRoute::effective_timeout`] of `options.inference_timeout`: its
    /// own `timeout_secs`, else the pipeline's, raised to 60 s for Anthropic.
    /// Must be called inside a Tokio runtime.
    ///
    /// The web server waits `ServerConfig::timeout_seconds` for each answer;
    /// size it from [`ModelRoutesConfig::longest_timeout`] and the retries.
    ///
    /// # Errors
    ///
    /// [`OrchestratorError::ConfigError`] for an invalid table or a missing
    /// API key. Pipelines already started are shut down.
    pub fn spawn(config: &ModelRoutesConfig, options: &PipelineOptions) -> Result<Self, OrchestratorError> {
        config.validate().map_err(OrchestratorError::ConfigError)?;
        let mut workers = Vec::with_capacity(config.models.len());
        for route in &config.models {
            let mut opts = options.clone();
            opts.inference_timeout = route.effective_timeout(options.inference_timeout);
            // Same shared retry budget, priced for this route's model. A glob
            // route without a fixed model is priced by its name pattern,
            // which as an unknown model is priced high, never at $0.
            if let Some(b) = &opts.retry.budget {
                let model = route.model.as_deref().unwrap_or(&route.name);
                opts.retry.budget = Some(crate::enhanced::RetryBudget::for_model(
                    b.spend.clone(),
                    route.provider.as_str(),
                    model,
                ));
            }
            // The pipeline enforces the timeout (and counts it against the
            // breaker); the worker's own HTTP timeout is a backstop.
            let worker = route.build_worker(opts.inference_timeout + Duration::from_secs(1))?;
            workers.push((route.clone(), worker, opts));
        }
        let entries = workers
            .into_iter()
            .map(|(route, worker, opts)| RouteEntry {
                route,
                handles: spawn_pipeline_with(worker, opts),
            })
            .collect();
        Ok(Self {
            entries,
            default_route: config.default_route,
        })
    }

    /// Routes served by pipelines you started yourself.
    pub fn from_pipelines(routes: Vec<(ModelRoute, PipelineHandles)>, default_route: bool) -> Self {
        Self {
            entries: routes
                .into_iter()
                .map(|(route, handles)| RouteEntry { route, handles })
                .collect(),
            default_route,
        }
    }

    /// `true` when unmatched models go to the server's own pipeline.
    pub fn has_default_route(&self) -> bool {
        self.default_route
    }

    /// The configured routes, in table order.
    pub fn routes(&self) -> impl Iterator<Item = &ModelRoute> {
        self.entries.iter().map(|e| &e.route)
    }

    /// Number of routes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when there are no routes.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Index of the route serving `model`: an exact name first, then the
    /// first glob that matches.
    pub fn resolve(&self, model: &str) -> Option<usize> {
        let lower = model.to_ascii_lowercase();
        self.entries
            .iter()
            .position(|e| !e.route.is_glob() && e.route.name.to_ascii_lowercase() == lower)
            .or_else(|| {
                self.entries
                    .iter()
                    .position(|e| e.route.is_glob() && e.route.matches(model))
            })
    }

    /// The route at `index`.
    pub fn route(&self, index: usize) -> Option<&ModelRoute> {
        self.entries.get(index).map(|e| &e.route)
    }

    /// The pipeline serving the route at `index`.
    pub fn pipeline(&self, index: usize) -> Option<&PipelineHandles> {
        self.entries.get(index).map(|e| &e.handles)
    }

    /// Stop every route's pipeline.
    pub fn shutdown(&self) {
        for e in &self.entries {
            e.handles.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_shell_patterns() {
        assert!(glob_match("gpt-*", "gpt-4o"));
        assert!(glob_match("gpt-*", "gpt-"));
        assert!(!glob_match("gpt-*", "chatgpt-4o"));
        assert!(glob_match("*-mini", "gpt-4o-mini"));
        assert!(glob_match("llama-?b", "llama-8b"));
        assert!(!glob_match("llama-?b", "llama-70b"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let r = ModelRoute::new("Claude-*", RouteProvider::Anthropic);
        assert!(r.matches("claude-sonnet-4-5"));
        assert!(r.matches("CLAUDE-OPUS"));
    }

    #[test]
    fn parses_from_a_pipeline_toml_ignoring_other_tables() {
        let cfg = ModelRoutesConfig::from_toml_str(
            r#"
            [pipeline]
            name = "x"
            [[models]]
            name = "gpt-*"
            provider = "openai-compatible"
            base_url = "http://localhost:8000/v1"
            [[models]]
            name = "claude-*"
            provider = "anthropic"
            max_tokens = 8192
            [[models]]
            name = "llama-*"
            provider = "llama.cpp"
            "#,
        )
        .expect("parses");
        assert!(cfg.default_route, "default route is on unless turned off");
        let providers: Vec<_> = cfg.models.iter().map(|m| m.provider).collect();
        assert_eq!(
            providers,
            vec![RouteProvider::OpenAi, RouteProvider::Anthropic, RouteProvider::LlamaCpp]
        );
        assert_eq!(cfg.models[1].max_tokens, Some(8192));
    }

    #[test]
    fn unknown_fields_providers_and_duplicates_are_errors() {
        let typo = ModelRoutesConfig::from_toml_str("[[models]]\nname='a'\nprovider='openai'\nbase='x'\n");
        assert!(typo.is_err());
        let bad = ModelRoutesConfig::from_toml_str("[[models]]\nname='a'\nprovider='bard'\n");
        assert!(format!("{:?}", bad.err()).contains("unknown provider"));
        let dup = ModelRoutesConfig::from_toml_str(
            "[[models]]\nname='a'\nprovider='echo'\n[[models]]\nname='A'\nprovider='echo'\n",
        );
        assert!(format!("{:?}", dup.err()).contains("listed twice"));
        let url = ModelRoutesConfig::from_toml_str("[[models]]\nname='a'\nprovider='openai'\nbase_url='localhost'\n");
        assert!(url.is_err());
    }

    #[test]
    fn shipped_example_table_parses() {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
        let path = Path::new(&dir).join("examples").join("models.example.toml");
        if !path.exists() {
            return;
        }
        let cfg = ModelRoutesConfig::from_file(&path).expect("examples/models.example.toml parses");
        assert!(cfg.default_route);
        assert_eq!(cfg.models.len(), 7);
        assert_eq!(cfg.models[6].model.as_deref(), Some("meta-llama/llama-3.1-8b-instruct"));
        // The reasoning-model globs must not catch other names starting with 'o'.
        let o_routes: Vec<&ModelRoute> =
            cfg.models.iter().filter(|r| r.name.starts_with('o')).collect();
        assert_eq!(o_routes.len(), 3);
        for model in ["o1-mini", "o3", "o4-mini"] {
            assert!(o_routes.iter().any(|r| r.matches(model)), "{model} routes to OpenAI");
        }
        for model in ["olmo-2-13b", "openchat-3.5", "orca-mini", "openai/gpt-oss-20b"] {
            assert!(cfg.models.iter().all(|r| !r.matches(model)), "{model} must not match a route");
        }
    }

    #[test]
    fn key_variables_follow_the_provider() {
        assert_eq!(
            ModelRoute::new("gpt-*", RouteProvider::OpenAi).key_variable().as_deref(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            ModelRoute::new("gpt-*", RouteProvider::OpenAi)
                .with_base_url("http://vllm:8000/v1")
                .key_variable(),
            None,
            "a local OpenAI-compatible server needs no key"
        );
        assert_eq!(
            ModelRoute::new("x", RouteProvider::OpenAi)
                .with_base_url("https://openrouter.ai/api/v1")
                .with_api_key_env("OPENROUTER_API_KEY")
                .key_variable()
                .as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(
            ModelRoute::new("claude-*", RouteProvider::Anthropic).key_variable().as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
        assert_eq!(ModelRoute::new("l", RouteProvider::LlamaCpp).key_variable(), None);
    }

    #[test]
    fn missing_key_is_a_config_error() {
        let r = ModelRoute::new("x", RouteProvider::OpenAi)
            .with_api_key_env("TPO_TEST_ROUTE_KEY_THAT_IS_NEVER_SET");
        let err = r.build_worker(Duration::from_secs(1)).err().expect("error");
        assert!(err.to_string().contains("TPO_TEST_ROUTE_KEY_THAT_IS_NEVER_SET"), "{err}");
    }

    #[tokio::test]
    async fn resolve_prefers_exact_names_then_globs_in_order() {
        let cfg = ModelRoutesConfig::new(vec![
            ModelRoute::new("gpt-*", RouteProvider::Echo),
            ModelRoute::new("*", RouteProvider::Echo),
            ModelRoute::new("gpt-4o", RouteProvider::Echo),
        ]);
        let routes = ModelRoutes::spawn(&cfg, &PipelineOptions::default()).expect("spawns");
        assert_eq!(routes.resolve("gpt-4o"), Some(2), "exact beats an earlier glob");
        assert_eq!(routes.resolve("gpt-4o-mini"), Some(0));
        assert_eq!(routes.resolve("mistral"), Some(1));
        routes.shutdown();
    }
}
