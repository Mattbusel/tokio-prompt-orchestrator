//! # Prompt Template Engine
//!
//! Versioned, variable-interpolated prompt templates with built-in A/B testing.
//!
//! ## Overview
//!
//! - **Templates** — named prompt blueprints with `{{variable}}` placeholders,
//!   optional system prompts, and version tags.
//! - **Registry** — hot-reloadable store for all templates; load from TOML.
//! - **A/B Experiments** — route traffic between template variants, collect
//!   per-variant latency and quality metrics, and determine winners via a
//!   two-proportion Z-test.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use tokio_prompt_orchestrator::templates::{PromptTemplate, TemplateRegistry};
//! use std::collections::HashMap;
//!
//! let registry = TemplateRegistry::new();
//!
//! let t = PromptTemplate::builder("summarise")
//!     .version("v2")
//!     .system("You are a concise summariser.")
//!     .body("Summarise the following in {{max_words}} words or fewer:\n\n{{text}}")
//!     .tag("summarisation")
//!     .build();
//!
//! registry.register(t);
//!
//! let mut vars = HashMap::new();
//! vars.insert("max_words", "50");
//! vars.insert("text", "The quick brown fox…");
//!
//! let rendered = registry.render("summarise", &vars).unwrap();
//! println!("{rendered}");
//! ```

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

/// A versioned prompt template with variable placeholders.
///
/// Placeholders use `{{variable_name}}` syntax. Variables are resolved at
/// render time from a caller-supplied map. Missing variables default to the
/// empty string unless a default is configured on the template.
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    /// Unique template name used as the lookup key.
    pub name: String,
    /// Semantic version string (e.g. `"v1"`, `"v2.1"`).
    pub version: String,
    /// Optional system prompt (sent as the first turn or prefixed).
    pub system: Option<String>,
    /// Template body with `{{variable}}` placeholders.
    pub body: String,
    /// Variable names declared in this template with optional defaults.
    pub variables: HashMap<String, Option<String>>,
    /// Arbitrary tags for grouping and filtering.
    pub tags: Vec<String>,
    /// Human-readable description.
    pub description: Option<String>,
}

impl PromptTemplate {
    /// Start building a template with the given name.
    pub fn builder(name: impl Into<String>) -> TemplateBuilder {
        TemplateBuilder::new(name)
    }

    /// Render the template body by substituting `vars` for each `{{key}}`.
    ///
    /// In addition to simple `{{variable}}` interpolation this method handles:
    ///
    /// - **Conditional blocks** `{{#if condition}}...{{/if}}` — the block is
    ///   included if `vars` contains a key equal to `condition` whose value is
    ///   non-empty and not the literal string `"false"` or `"0"`.
    ///
    /// - **Loop blocks** `{{#each items}}...{{/each}}` — the block is repeated
    ///   once for each item in a comma-separated list stored under the key
    ///   `items`.  Inside the block `{{this}}` is replaced with the current
    ///   item and `{{@index}}` with the zero-based index.
    ///
    /// Variables present in the template but absent from `vars` fall back to
    /// the template's own default value. If no default exists the placeholder
    /// is replaced with an empty string.
    pub fn render(&self, vars: &HashMap<&str, &str>) -> String {
        // Step 1 — process {{#each items}}...{{/each}} blocks
        let mut output = render_each_blocks(&self.body, vars);

        // Step 2 — process {{#if condition}}...{{/if}} blocks
        output = render_if_blocks(&output, vars);

        // Step 3 — simple variable substitution
        for (key, default) in &self.variables {
            let placeholder = format!("{{{{{key}}}}}");
            let value = vars
                .get(key.as_str())
                .copied()
                .or(default.as_deref())
                .unwrap_or("");
            output = output.replace(&placeholder, value);
        }
        // Also substitute any vars not pre-declared
        for (key, value) in vars {
            let placeholder = format!("{{{{{key}}}}}");
            if output.contains(&placeholder) {
                output = output.replace(&placeholder, value);
            }
        }
        output
    }

    /// Render both system prompt (if any) and body; returns `(system, body)`.
    pub fn render_full(
        &self,
        vars: &HashMap<&str, &str>,
    ) -> (Option<String>, String) {
        let system = self.system.as_ref().map(|s| {
            let mut out = s.clone();
            for (k, v) in vars {
                out = out.replace(&format!("{{{{{k}}}}}"), v);
            }
            out
        });
        (system, self.render(vars))
    }

    /// Extract variable names declared in the body (`{{name}}`).
    pub fn declared_variables(&self) -> Vec<String> {
        extract_placeholders(&self.body)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Fluent builder for [`PromptTemplate`].
#[derive(Default)]
pub struct TemplateBuilder {
    name: String,
    version: String,
    system: Option<String>,
    body: String,
    defaults: HashMap<String, Option<String>>,
    tags: Vec<String>,
    description: Option<String>,
}

impl TemplateBuilder {
    fn new(name: impl Into<String>) -> Self {
        TemplateBuilder {
            name: name.into(),
            version: "v1".into(),
            ..Default::default()
        }
    }

    pub fn version(mut self, v: impl Into<String>) -> Self {
        self.version = v.into();
        self
    }

    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = Some(s.into());
        self
    }

    pub fn body(mut self, b: impl Into<String>) -> Self {
        self.body = b.into();
        self
    }

    /// Declare a required variable (no default).
    pub fn var(mut self, name: impl Into<String>) -> Self {
        self.defaults.insert(name.into(), None);
        self
    }

    /// Declare a variable with a fallback default value.
    pub fn var_default(mut self, name: impl Into<String>, default: impl Into<String>) -> Self {
        self.defaults.insert(name.into(), Some(default.into()));
        self
    }

    pub fn tag(mut self, t: impl Into<String>) -> Self {
        self.tags.push(t.into());
        self
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    /// Finalise and return the [`PromptTemplate`].
    ///
    /// Auto-detects variables from `{{…}}` placeholders in the body if they
    /// haven't been explicitly declared.
    pub fn build(mut self) -> PromptTemplate {
        for v in extract_placeholders(&self.body) {
            self.defaults.entry(v).or_insert(None);
        }
        if let Some(sys) = &self.system {
            for v in extract_placeholders(sys) {
                self.defaults.entry(v).or_insert(None);
            }
        }
        PromptTemplate {
            name: self.name,
            version: self.version,
            system: self.system,
            body: self.body,
            variables: self.defaults,
            tags: self.tags,
            description: self.description,
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Template lookup errors.
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("template '{0}' not found")]
    NotFound(String),
    #[error("missing required variable '{0}'")]
    MissingVariable(String),
}

/// Thread-safe registry of named prompt templates.
///
/// Cheap to clone (Arc-backed). The registry supports hot-reload: call
/// [`register`](Self::register) to add or replace a template at any time.
#[derive(Clone, Default)]
pub struct TemplateRegistry {
    inner: Arc<RwLock<HashMap<String, PromptTemplate>>>,
}

impl TemplateRegistry {
    pub fn new() -> Self {
        TemplateRegistry::default()
    }

    /// Register (or replace) a template.
    pub fn register(&self, template: PromptTemplate) {
        let mut guard = self.inner.write().unwrap();
        guard.insert(template.name.clone(), template);
    }

    /// Remove a template by name.
    pub fn unregister(&self, name: &str) {
        let mut guard = self.inner.write().unwrap();
        guard.remove(name);
    }

    /// Render a named template with the supplied variables.
    pub fn render(
        &self,
        name: &str,
        vars: &HashMap<&str, &str>,
    ) -> Result<String, TemplateError> {
        let guard = self.inner.read().unwrap();
        let tmpl = guard.get(name).ok_or_else(|| TemplateError::NotFound(name.to_owned()))?;

        // Validate required variables
        for (var, default) in &tmpl.variables {
            if default.is_none() && !vars.contains_key(var.as_str()) {
                return Err(TemplateError::MissingVariable(var.clone()));
            }
        }
        Ok(tmpl.render(vars))
    }

    /// Render a template and return `(system_prompt, body)`.
    pub fn render_full(
        &self,
        name: &str,
        vars: &HashMap<&str, &str>,
    ) -> Result<(Option<String>, String), TemplateError> {
        let guard = self.inner.read().unwrap();
        let tmpl = guard.get(name).ok_or_else(|| TemplateError::NotFound(name.to_owned()))?;
        Ok(tmpl.render_full(vars))
    }

    /// Get a snapshot of a template (cloned).
    pub fn get(&self, name: &str) -> Option<PromptTemplate> {
        let guard = self.inner.read().unwrap();
        guard.get(name).cloned()
    }

    /// List all registered template names.
    pub fn names(&self) -> Vec<String> {
        let guard = self.inner.read().unwrap();
        guard.keys().cloned().collect()
    }

    /// Load templates from a TOML string.
    ///
    /// Expected format:
    /// ```toml
    /// [[templates]]
    /// name    = "summarise"
    /// version = "v1"
    /// system  = "You are a summariser."
    /// body    = "Summarise: {{text}}"
    /// tags    = ["nlp"]
    ///
    /// [[templates]]
    /// name = "translate"
    /// body = "Translate to {{lang}}: {{text}}"
    /// ```
    pub fn load_toml(&self, toml_str: &str) -> Result<usize, String> {
        let parsed: toml::Value =
            toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {e}"))?;

        let arr = parsed
            .get("templates")
            .and_then(|v| v.as_array())
            .ok_or("expected [[templates]] array")?;

        let mut count = 0;
        for item in arr {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("template missing 'name'")?
                .to_owned();

            let version = item
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("v1")
                .to_owned();

            let body = item
                .get("body")
                .and_then(|v| v.as_str())
                .ok_or(format!("template '{name}' missing 'body'"))?
                .to_owned();

            let system = item.get("system").and_then(|v| v.as_str()).map(str::to_owned);

            let description = item
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_owned);

            let tags: Vec<String> = item
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();

            let mut builder = PromptTemplate::builder(name)
                .version(version)
                .body(body);

            if let Some(sys) = system {
                builder = builder.system(sys);
            }
            if let Some(desc) = description {
                builder = builder.description(desc);
            }
            for tag in tags {
                builder = builder.tag(tag);
            }

            self.register(builder.build());
            count += 1;
        }
        Ok(count)
    }
}

// ---------------------------------------------------------------------------
// A/B Testing
// ---------------------------------------------------------------------------

/// A single variant in an A/B experiment.
#[derive(Debug, Clone)]
pub struct ExperimentVariant {
    /// Template name in the registry.
    pub template_name: String,
    /// Traffic weight relative to other variants (unnormalized).
    pub weight: f64,
    /// Display label for this variant.
    pub label: String,
}

/// Metrics collected per A/B variant.
#[derive(Debug, Clone, Default)]
pub struct VariantMetrics {
    pub requests: u64,
    pub successes: u64,
    pub total_latency_ms: u64,
    /// Accumulated quality score (caller-reported, 0.0–1.0 per request).
    pub total_quality: f64,
}

impl VariantMetrics {
    pub fn success_rate(&self) -> f64 {
        if self.requests == 0 {
            return 0.0;
        }
        self.successes as f64 / self.requests as f64
    }

    pub fn avg_latency_ms(&self) -> f64 {
        if self.requests == 0 {
            return 0.0;
        }
        self.total_latency_ms as f64 / self.requests as f64
    }

    pub fn avg_quality(&self) -> f64 {
        if self.successes == 0 {
            return 0.0;
        }
        self.total_quality / self.successes as f64
    }
}

/// An A/B experiment that routes traffic between template variants.
pub struct AbExperiment {
    pub name: String,
    pub variants: Vec<ExperimentVariant>,
    metrics: Arc<RwLock<Vec<VariantMetrics>>>,
    cumulative_weights: Vec<f64>,
    total_weight: f64,
}

impl AbExperiment {
    /// Create a new A/B experiment.
    ///
    /// `variants` must not be empty.
    ///
    /// # Panics
    ///
    /// Panics if `variants` is empty.
    pub fn new(name: impl Into<String>, variants: Vec<ExperimentVariant>) -> Self {
        assert!(!variants.is_empty(), "experiment must have at least one variant");
        let n = variants.len();
        let mut cum = 0.0f64;
        let mut cumulative_weights = Vec::with_capacity(n);
        for v in &variants {
            cum += v.weight;
            cumulative_weights.push(cum);
        }
        let metrics = vec![VariantMetrics::default(); n];
        AbExperiment {
            name: name.into(),
            cumulative_weights,
            total_weight: cum,
            variants,
            metrics: Arc::new(RwLock::new(metrics)),
        }
    }

    /// Select a variant index using a [0.0, 1.0) uniform random draw.
    ///
    /// Pass a caller-supplied random value so callers can use any PRNG.
    pub fn pick_variant(&self, rand_0_1: f64) -> usize {
        let target = rand_0_1 * self.total_weight;
        for (i, &w) in self.cumulative_weights.iter().enumerate() {
            if target < w {
                return i;
            }
        }
        self.variants.len() - 1
    }

    /// Record that a request was routed to variant `idx`.
    pub fn record_request(&self, idx: usize) {
        let mut guard = self.metrics.write().unwrap();
        if let Some(m) = guard.get_mut(idx) {
            m.requests += 1;
        }
    }

    /// Record a successful response for variant `idx`.
    pub fn record_success(&self, idx: usize, latency_ms: u64, quality: f64) {
        let mut guard = self.metrics.write().unwrap();
        if let Some(m) = guard.get_mut(idx) {
            m.successes += 1;
            m.total_latency_ms += latency_ms;
            m.total_quality += quality.clamp(0.0, 1.0);
        }
    }

    /// Return a snapshot of all variant metrics.
    pub fn metrics(&self) -> Vec<VariantMetrics> {
        self.metrics.read().unwrap().clone()
    }

    /// Return the index of the leading variant by quality score.
    /// Returns `None` if no successes have been recorded.
    pub fn leading_variant(&self) -> Option<usize> {
        let guard = self.metrics.read().unwrap();
        guard
            .iter()
            .enumerate()
            .filter(|(_, m)| m.successes > 0)
            .max_by(|(_, a), (_, b)| a.avg_quality().partial_cmp(&b.avg_quality()).unwrap())
            .map(|(i, _)| i)
    }

    /// Two-proportion Z-test p-value between variants `a` and `b` (success rate).
    ///
    /// Returns `None` if sample sizes are insufficient (< 30 per variant).
    pub fn significance(&self, a: usize, b: usize) -> Option<f64> {
        let guard = self.metrics.read().unwrap();
        let ma = guard.get(a)?;
        let mb = guard.get(b)?;
        if ma.requests < 30 || mb.requests < 30 {
            return None;
        }
        let pa = ma.success_rate();
        let pb = mb.success_rate();
        let na = ma.requests as f64;
        let nb = mb.requests as f64;
        let p_pool = (ma.successes + mb.successes) as f64 / (na + nb);
        let denom = (p_pool * (1.0 - p_pool) * (1.0 / na + 1.0 / nb)).sqrt();
        if denom < f64::EPSILON {
            return None;
        }
        let z = (pa - pb).abs() / denom;
        // Approximate two-tailed p-value via standard normal CDF
        let p = 2.0 * standard_normal_cdf(-z.abs());
        Some(p)
    }

    /// Summary report for logging / monitoring.
    pub fn report(&self) -> ExperimentReport {
        let metrics = self.metrics();
        let variants = self
            .variants
            .iter()
            .zip(metrics.iter())
            .map(|(v, m)| VariantReport {
                label: v.label.clone(),
                template_name: v.template_name.clone(),
                requests: m.requests,
                success_rate: m.success_rate(),
                avg_latency_ms: m.avg_latency_ms(),
                avg_quality: m.avg_quality(),
            })
            .collect();
        ExperimentReport {
            experiment: self.name.clone(),
            variants,
            leading_variant: self.leading_variant().map(|i| self.variants[i].label.clone()),
        }
    }
}

/// A point-in-time report for an A/B experiment.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExperimentReport {
    pub experiment: String,
    pub variants: Vec<VariantReport>,
    pub leading_variant: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VariantReport {
    pub label: String,
    pub template_name: String,
    pub requests: u64,
    pub success_rate: f64,
    pub avg_latency_ms: f64,
    pub avg_quality: f64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Block rendering helpers
// ---------------------------------------------------------------------------

/// Evaluate a condition value: truthy when non-empty and not `"false"`/`"0"`.
fn is_truthy(value: &str) -> bool {
    !value.is_empty() && value != "false" && value != "0"
}

/// Process all `{{#if condition}}...{{/if}}` blocks in `template`.
///
/// - The block is **included** (without the tags) when the condition is truthy.
/// - The block is **removed** (including the tags) when the condition is falsy.
/// - Nested `{{#if}}` blocks are **not** supported — they are left as-is.
fn render_if_blocks(template: &str, vars: &HashMap<&str, &str>) -> String {
    let mut output = template.to_string();

    loop {
        // Find the next {{#if <condition>}} tag
        let open_start = match output.find("{{#if ") {
            Some(i) => i,
            None => break,
        };
        let tag_end = match output[open_start..].find("}}") {
            Some(j) => open_start + j + 2,
            None => break,
        };

        // Extract the condition name from between "{{#if " and "}}"
        let condition = output[open_start + 6..tag_end - 2].trim().to_string();

        // Locate the matching {{/if}}
        let close_tag = "{{/if}}";
        let close_start = match output[tag_end..].find(close_tag) {
            Some(k) => tag_end + k,
            None => break, // malformed template — stop processing
        };
        let close_end = close_start + close_tag.len();

        // Body is the content between the opening and closing tags
        let body = output[tag_end..close_start].to_string();

        // Decide what to replace the whole block with
        let condition_value = vars.get(condition.as_str()).copied().unwrap_or("");
        let replacement = if is_truthy(condition_value) {
            body
        } else {
            String::new()
        };

        output = format!("{}{}{}", &output[..open_start], replacement, &output[close_end..]);
    }

    output
}

/// Process all `{{#each items}}...{{/each}}` blocks in `template`.
///
/// The value of `items` in `vars` is treated as a **comma-separated list**.
/// Each element is trimmed before use.  Inside the block:
///
/// - `{{this}}` — replaced with the current item's value.
/// - `{{@index}}` — replaced with the zero-based integer index.
///
/// If `items` is absent or empty the entire block is removed.
fn render_each_blocks(template: &str, vars: &HashMap<&str, &str>) -> String {
    let mut output = template.to_string();

    loop {
        // Find the next {{#each <list_key>}} tag
        let open_start = match output.find("{{#each ") {
            Some(i) => i,
            None => break,
        };
        let tag_end = match output[open_start..].find("}}") {
            Some(j) => open_start + j + 2,
            None => break,
        };

        // Extract the list key from between "{{#each " and "}}"
        let list_key = output[open_start + 8..tag_end - 2].trim().to_string();

        // Locate the matching {{/each}}
        let close_tag = "{{/each}}";
        let close_start = match output[tag_end..].find(close_tag) {
            Some(k) => tag_end + k,
            None => break, // malformed — stop
        };
        let close_end = close_start + close_tag.len();

        // The body template for each iteration
        let body_template = output[tag_end..close_start].to_string();

        // Expand
        let items_str = vars.get(list_key.as_str()).copied().unwrap_or("");
        let mut expanded = String::new();

        if !items_str.is_empty() {
            for (idx, item) in items_str.split(',').enumerate() {
                let item = item.trim();
                let iter_body = body_template
                    .replace("{{this}}", item)
                    .replace("{{@index}}", &idx.to_string());
                expanded.push_str(&iter_body);
            }
        }

        output = format!("{}{}{}", &output[..open_start], expanded, &output[close_end..]);
    }

    output
}

/// Extract placeholder names from a `{{…}}` template string.
fn extract_placeholders(s: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '{' {
            if let Some((_, '{')) = chars.peek() {
                chars.next();
                let start = i + 2;
                let mut end = start;
                while let Some(&(j, c2)) = chars.peek() {
                    if c2 == '}' {
                        chars.next();
                        if let Some(&(_, '}')) = chars.peek() {
                            chars.next();
                            end = j;
                            break;
                        }
                    } else {
                        end = j + c2.len_utf8();
                        chars.next();
                    }
                }
                let name = s[start..end].trim().to_owned();
                if !name.is_empty() && !vars.contains(&name) {
                    vars.push(name);
                }
            }
        }
    }
    vars
}

/// Approximation of the standard normal CDF Φ(x).
/// Uses the Abramowitz & Stegun rational approximation (max error ≈ 7.5×10⁻⁸).
fn standard_normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.2316419 * x.abs());
    let poly = t
        * (0.319381530
            + t * (-0.356563782
                + t * (1.781477937
                    + t * (-1.821255978 + t * 1.330274429))));
    let phi = 1.0 - (-(x * x / 2.0)).exp() / (2.0 * std::f64::consts::PI).sqrt() * poly;
    if x >= 0.0 {
        phi
    } else {
        1.0 - phi
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_render() {
        let t = PromptTemplate::builder("t1")
            .body("Hello {{name}}, you are {{age}} years old.")
            .build();
        let mut vars = HashMap::new();
        vars.insert("name", "Alice");
        vars.insert("age", "30");
        assert_eq!(t.render(&vars), "Hello Alice, you are 30 years old.");
    }

    #[test]
    fn test_default_variable() {
        let t = PromptTemplate::builder("t2")
            .body("Format: {{style}}")
            .var_default("style", "markdown")
            .build();
        let vars = HashMap::new();
        assert_eq!(t.render(&vars), "Format: markdown");
    }

    #[test]
    fn test_missing_required_variable_error() {
        let registry = TemplateRegistry::new();
        let t = PromptTemplate::builder("t3")
            .body("Hello {{name}}")
            .var("name")
            .build();
        registry.register(t);
        let vars: HashMap<&str, &str> = HashMap::new();
        assert!(matches!(
            registry.render("t3", &vars),
            Err(TemplateError::MissingVariable(_))
        ));
    }

    #[test]
    fn test_not_found_error() {
        let registry = TemplateRegistry::new();
        let vars: HashMap<&str, &str> = HashMap::new();
        assert!(matches!(
            registry.render("nonexistent", &vars),
            Err(TemplateError::NotFound(_))
        ));
    }

    #[test]
    fn test_extract_placeholders() {
        let vars = extract_placeholders("Say {{greeting}} to {{name}}!");
        assert_eq!(vars, vec!["greeting", "name"]);
    }

    #[test]
    fn test_toml_load() {
        let registry = TemplateRegistry::new();
        let toml = r#"
[[templates]]
name    = "greet"
version = "v1"
body    = "Hello {{name}}!"
tags    = ["greeting"]
"#;
        let count = registry.load_toml(toml).unwrap();
        assert_eq!(count, 1);
        let mut vars = HashMap::new();
        vars.insert("name", "Bob");
        assert_eq!(registry.render("greet", &vars).unwrap(), "Hello Bob!");
    }

    #[test]
    fn test_ab_experiment_routing() {
        let variants = vec![
            ExperimentVariant {
                template_name: "tmpl_a".into(),
                weight: 70.0,
                label: "control".into(),
            },
            ExperimentVariant {
                template_name: "tmpl_b".into(),
                weight: 30.0,
                label: "treatment".into(),
            },
        ];
        let exp = AbExperiment::new("test_exp", variants);

        // 0.0 should always pick first variant (weight 70/100)
        assert_eq!(exp.pick_variant(0.0), 0);
        // 0.99 should pick second variant
        assert_eq!(exp.pick_variant(0.99), 1);
    }

    #[test]
    fn test_ab_metrics() {
        let variants = vec![
            ExperimentVariant {
                template_name: "a".into(),
                weight: 1.0,
                label: "a".into(),
            },
            ExperimentVariant {
                template_name: "b".into(),
                weight: 1.0,
                label: "b".into(),
            },
        ];
        let exp = AbExperiment::new("exp", variants);
        exp.record_request(0);
        exp.record_request(0);
        exp.record_success(0, 100, 0.9);
        exp.record_success(0, 120, 0.85);

        let metrics = exp.metrics();
        assert_eq!(metrics[0].requests, 2);
        assert_eq!(metrics[0].successes, 2);
        assert!((metrics[0].avg_latency_ms() - 110.0).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Block rendering tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_if_block_truthy() {
        let t = PromptTemplate::builder("t_if")
            .body("Prefix. {{#if show_extra}}Extra content.{{/if}} Suffix.")
            .build();
        let mut vars = HashMap::new();
        vars.insert("show_extra", "true");
        assert_eq!(t.render(&vars), "Prefix. Extra content. Suffix.");
    }

    #[test]
    fn test_if_block_falsy() {
        let t = PromptTemplate::builder("t_if_false")
            .body("Prefix. {{#if show_extra}}Extra content.{{/if}} Suffix.")
            .build();
        let mut vars = HashMap::new();
        vars.insert("show_extra", "false");
        assert_eq!(t.render(&vars), "Prefix.  Suffix.");
    }

    #[test]
    fn test_if_block_missing_var_is_falsy() {
        let t = PromptTemplate::builder("t_if_missing")
            .body("A{{#if missing}}B{{/if}}C")
            .build();
        let vars = HashMap::new();
        assert_eq!(t.render(&vars), "AC");
    }

    #[test]
    fn test_if_block_zero_is_falsy() {
        let t = PromptTemplate::builder("t_if_zero")
            .body("{{#if count}}has items{{/if}}")
            .build();
        let mut vars = HashMap::new();
        vars.insert("count", "0");
        assert_eq!(t.render(&vars), "");
    }

    #[test]
    fn test_each_block_basic() {
        let t = PromptTemplate::builder("t_each")
            .body("Items: {{#each fruits}}- {{this}}\n{{/each}}")
            .build();
        let mut vars = HashMap::new();
        vars.insert("fruits", "apple, banana, cherry");
        let result = t.render(&vars);
        assert!(result.contains("- apple"));
        assert!(result.contains("- banana"));
        assert!(result.contains("- cherry"));
    }

    #[test]
    fn test_each_block_index() {
        let t = PromptTemplate::builder("t_each_idx")
            .body("{{#each items}}{{@index}}:{{this}} {{/each}}")
            .build();
        let mut vars = HashMap::new();
        vars.insert("items", "a,b,c");
        let result = t.render(&vars);
        assert!(result.contains("0:a"));
        assert!(result.contains("1:b"));
        assert!(result.contains("2:c"));
    }

    #[test]
    fn test_each_block_empty_list() {
        let t = PromptTemplate::builder("t_each_empty")
            .body("before{{#each items}}{{this}}{{/each}}after")
            .build();
        let mut vars = HashMap::new();
        vars.insert("items", "");
        assert_eq!(t.render(&vars), "beforeafter");
    }

    #[test]
    fn test_if_and_each_combined() {
        let t = PromptTemplate::builder("t_combined")
            .body("{{#if show}}List: {{#each items}}{{this}} {{/each}}{{/if}}")
            .build();
        let mut vars = HashMap::new();
        vars.insert("show", "yes");
        vars.insert("items", "x,y,z");
        let result = t.render(&vars);
        assert!(result.contains("List:"));
        assert!(result.contains("x"));
        assert!(result.contains("y"));
        assert!(result.contains("z"));
    }
}
