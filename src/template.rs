//! # Prompt Template Engine
//!
//! Simple `{{variable}}` substitution engine with filter support.
//!
//! ## Variable Syntax
//!
//! - `{{var}}` — plain substitution.
//! - `{{var | upper}}` — apply the `upper` filter.
//! - `{{var | lower}}` — apply the `lower` filter.
//! - `{{var | truncate:100}}` — truncate to 100 characters.
//! - `{{var | default:"fallback"}}` — use `"fallback"` if `var` is missing.
//!
//! ## Example
//!
//! ```rust
//! use tokio_prompt_orchestrator::template::{
//!     PromptTemplate, TemplateContext, TemplateValue,
//! };
//!
//! let t = PromptTemplate::from_str("Hello, {{name | upper}}!").unwrap();
//! let mut ctx = TemplateContext::new();
//! ctx.set("name", TemplateValue::Text("world".to_string()));
//! let rendered = t.render(&ctx).unwrap();
//! assert_eq!(rendered, "Hello, WORLD!");
//! ```

use std::collections::HashMap;
use thiserror::Error;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors that can occur during template parsing or rendering.
#[derive(Debug, Error, PartialEq, Clone)]
pub enum TemplateError {
    /// A `{{variable}}` referenced in the template has no value in the context
    /// and no `default` filter was supplied.
    #[error("missing variable: {0}")]
    MissingVariable(String),

    /// An unrecognised filter name was used (e.g. `{{x | frobnicate}}`).
    #[error("invalid filter: {0}")]
    InvalidFilter(String),

    /// The template text could not be parsed (e.g. an unclosed `{{`).
    #[error("parse error: {0}")]
    ParseError(String),
}

// ── TemplateValue ─────────────────────────────────────────────────────────────

/// A typed value that can be stored in a [`TemplateContext`].
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateValue {
    /// A plain text string.
    Text(String),
    /// A number (rendered as its default decimal representation).
    Number(f64),
    /// A boolean (rendered as `"true"` or `"false"`).
    Bool(bool),
    /// A list of strings (rendered as comma-separated).
    List(Vec<String>),
}

impl TemplateValue {
    /// Convert the value to its string representation.
    pub fn to_string_repr(&self) -> String {
        match self {
            TemplateValue::Text(s) => s.clone(),
            TemplateValue::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15_f64 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            TemplateValue::Bool(b) => b.to_string(),
            TemplateValue::List(items) => items.join(", "),
        }
    }
}

// ── TemplateContext ───────────────────────────────────────────────────────────

/// Variable bindings for template rendering.
#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    values: HashMap<String, TemplateValue>,
}

impl TemplateContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a variable binding.
    pub fn set(&mut self, key: impl Into<String>, value: TemplateValue) {
        self.values.insert(key.into(), value);
    }

    /// Retrieve a variable by name.
    pub fn get(&self, key: &str) -> Option<&TemplateValue> {
        self.values.get(key)
    }
}

// ── Filter ────────────────────────────────────────────────────────────────────

/// A filter applied to a substituted value.
#[derive(Debug, Clone, PartialEq)]
enum Filter {
    Upper,
    Lower,
    Truncate(usize),
    Default(String),
}

impl Filter {
    fn parse(spec: &str) -> Result<Self, TemplateError> {
        let spec = spec.trim();
        if spec == "upper" {
            return Ok(Filter::Upper);
        }
        if spec == "lower" {
            return Ok(Filter::Lower);
        }
        if let Some(arg) = spec.strip_prefix("truncate:") {
            let n: usize = arg.trim().parse().map_err(|_| {
                TemplateError::InvalidFilter(format!("truncate: invalid length '{arg}'"))
            })?;
            return Ok(Filter::Truncate(n));
        }
        if let Some(arg) = spec.strip_prefix("default:") {
            let arg = arg.trim();
            // Strip surrounding quotes if present.
            let value = if (arg.starts_with('"') && arg.ends_with('"'))
                || (arg.starts_with('\'') && arg.ends_with('\''))
            {
                arg[1..arg.len() - 1].to_string()
            } else {
                arg.to_string()
            };
            return Ok(Filter::Default(value));
        }
        Err(TemplateError::InvalidFilter(spec.to_string()))
    }

    fn apply(&self, value: &str) -> String {
        match self {
            Filter::Upper => value.to_uppercase(),
            Filter::Lower => value.to_lowercase(),
            Filter::Truncate(n) => {
                if value.len() <= *n {
                    value.to_string()
                } else {
                    value[..*n].to_string()
                }
            }
            Filter::Default(_) => value.to_string(), // used only when value is missing
        }
    }
}

// ── Token ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Token {
    Literal(String),
    Substitution {
        variable: String,
        filters: Vec<Filter>,
    },
}

// ── PromptTemplate ────────────────────────────────────────────────────────────

/// A compiled prompt template.
///
/// Parse once with [`PromptTemplate::from_str`], render many times with
/// [`PromptTemplate::render`].
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    source: String,
    tokens: Vec<Token>,
}

impl PromptTemplate {
    /// Parse a template string and validate variable names and filters.
    ///
    /// # Errors
    ///
    /// Returns [`TemplateError::ParseError`] if there are unclosed `{{` blocks,
    /// or [`TemplateError::InvalidFilter`] for unrecognised filters.
    pub fn from_str(template: &str) -> Result<Self, TemplateError> {
        let tokens = Self::parse(template)?;
        Ok(Self {
            source: template.to_string(),
            tokens,
        })
    }

    /// Return the original template source string.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Render the template using `ctx`.
    ///
    /// # Errors
    ///
    /// - [`TemplateError::MissingVariable`] — a variable has no binding and no
    ///   `default` filter.
    /// - [`TemplateError::InvalidFilter`] — an unrecognised filter was used.
    pub fn render(&self, ctx: &TemplateContext) -> Result<String, TemplateError> {
        let mut out = String::new();
        for token in &self.tokens {
            match token {
                Token::Literal(s) => out.push_str(s),
                Token::Substitution { variable, filters } => {
                    let raw = ctx.get(variable);
                    // Handle `default` filter when variable is missing.
                    let base = match raw {
                        Some(v) => v.to_string_repr(),
                        None => {
                            // Look for a default filter.
                            let default_val = filters.iter().find_map(|f| {
                                if let Filter::Default(d) = f {
                                    Some(d.clone())
                                } else {
                                    None
                                }
                            });
                            match default_val {
                                Some(d) => d,
                                None => {
                                    return Err(TemplateError::MissingVariable(
                                        variable.clone(),
                                    ))
                                }
                            }
                        }
                    };
                    // Apply non-default filters in order.
                    let result = filters
                        .iter()
                        .filter(|f| !matches!(f, Filter::Default(_)))
                        .fold(base, |acc, f| f.apply(&acc));
                    out.push_str(&result);
                }
            }
        }
        Ok(out)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn parse(template: &str) -> Result<Vec<Token>, TemplateError> {
        let mut tokens = Vec::new();
        let mut remaining = template;

        while !remaining.is_empty() {
            match remaining.find("{{") {
                None => {
                    // No more substitutions.
                    tokens.push(Token::Literal(remaining.to_string()));
                    break;
                }
                Some(start) => {
                    if start > 0 {
                        tokens.push(Token::Literal(remaining[..start].to_string()));
                    }
                    let after_open = &remaining[start + 2..];
                    let end = after_open.find("}}").ok_or_else(|| {
                        TemplateError::ParseError("unclosed '{{' block".to_string())
                    })?;
                    let inner = after_open[..end].trim();
                    let token = Self::parse_substitution(inner)?;
                    tokens.push(token);
                    remaining = &after_open[end + 2..];
                }
            }
        }

        Ok(tokens)
    }

    fn parse_substitution(inner: &str) -> Result<Token, TemplateError> {
        // Split on `|` to separate variable name from filters.
        let parts: Vec<&str> = inner.splitn(2, '|').collect();
        let variable = parts[0].trim().to_string();

        // Validate variable name: must be a valid identifier.
        if variable.is_empty()
            || !variable
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_')
            || variable
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return Err(TemplateError::ParseError(format!(
                "invalid variable name: '{variable}'"
            )));
        }

        let filters = if parts.len() > 1 {
            // Multiple chained filters separated by `|`.
            parts[1]
                .split('|')
                .map(|f| Filter::parse(f.trim()))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        Ok(Token::Substitution { variable, filters })
    }
}

// ── TemplateLibrary ───────────────────────────────────────────────────────────

/// A named store of [`PromptTemplate`]s.
///
/// # Example
///
/// ```rust
/// use tokio_prompt_orchestrator::template::{
///     TemplateContext, TemplateLibrary, TemplateValue,
/// };
///
/// let mut lib = TemplateLibrary::new();
/// lib.register("greeting", "Hello, {{name}}!").unwrap();
///
/// let mut ctx = TemplateContext::new();
/// ctx.set("name", TemplateValue::Text("Alice".to_string()));
///
/// let rendered = lib.render("greeting", &ctx).unwrap();
/// assert_eq!(rendered, "Hello, Alice!");
/// ```
#[derive(Debug, Default)]
pub struct TemplateLibrary {
    templates: HashMap<String, PromptTemplate>,
}

impl TemplateLibrary {
    /// Create an empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a template under `name`.
    ///
    /// # Errors
    ///
    /// Returns a [`TemplateError`] if the template string cannot be parsed.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        template: impl AsRef<str>,
    ) -> Result<(), TemplateError> {
        let t = PromptTemplate::from_str(template.as_ref())?;
        self.templates.insert(name.into(), t);
        Ok(())
    }

    /// Register a pre-compiled [`PromptTemplate`] under `name`.
    pub fn register_template(&mut self, name: impl Into<String>, template: PromptTemplate) {
        self.templates.insert(name.into(), template);
    }

    /// Render the named template with the given context.
    ///
    /// # Errors
    ///
    /// - [`TemplateError::MissingVariable`] if `name` is not registered.
    /// - Any error from [`PromptTemplate::render`].
    pub fn render(&self, name: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
        let t = self
            .templates
            .get(name)
            .ok_or_else(|| TemplateError::MissingVariable(name.to_string()))?;
        t.render(ctx)
    }

    /// Return the names of all registered templates.
    pub fn list(&self) -> Vec<&str> {
        self.templates.keys().map(|s| s.as_str()).collect()
    }

    /// Return `true` if a template with the given name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.templates.contains_key(name)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ctx(pairs: &[(&str, &str)]) -> TemplateContext {
        let mut c = TemplateContext::new();
        for (k, v) in pairs {
            c.set(*k, TemplateValue::Text(v.to_string()));
        }
        c
    }

    #[test]
    fn test_plain_literal() {
        let t = PromptTemplate::from_str("hello world").unwrap();
        assert_eq!(t.render(&ctx(&[])).unwrap(), "hello world");
    }

    #[test]
    fn test_simple_substitution() {
        let t = PromptTemplate::from_str("Hello, {{name}}!").unwrap();
        assert_eq!(t.render(&ctx(&[("name", "Alice")])).unwrap(), "Hello, Alice!");
    }

    #[test]
    fn test_multiple_variables() {
        let t = PromptTemplate::from_str("{{a}} + {{b}} = {{c}}").unwrap();
        let c = ctx(&[("a", "1"), ("b", "2"), ("c", "3")]);
        assert_eq!(t.render(&c).unwrap(), "1 + 2 = 3");
    }

    #[test]
    fn test_missing_variable_error() {
        let t = PromptTemplate::from_str("{{missing}}").unwrap();
        let err = t.render(&ctx(&[])).unwrap_err();
        assert_eq!(err, TemplateError::MissingVariable("missing".to_string()));
    }

    #[test]
    fn test_filter_upper() {
        let t = PromptTemplate::from_str("{{name | upper}}").unwrap();
        assert_eq!(t.render(&ctx(&[("name", "hello")])).unwrap(), "HELLO");
    }

    #[test]
    fn test_filter_lower() {
        let t = PromptTemplate::from_str("{{name | lower}}").unwrap();
        assert_eq!(t.render(&ctx(&[("name", "HELLO")])).unwrap(), "hello");
    }

    #[test]
    fn test_filter_truncate() {
        let t = PromptTemplate::from_str("{{text | truncate:5}}").unwrap();
        assert_eq!(t.render(&ctx(&[("text", "hello world")])).unwrap(), "hello");
    }

    #[test]
    fn test_filter_truncate_short_string() {
        let t = PromptTemplate::from_str("{{text | truncate:100}}").unwrap();
        assert_eq!(t.render(&ctx(&[("text", "hi")])).unwrap(), "hi");
    }

    #[test]
    fn test_filter_default_present() {
        let t = PromptTemplate::from_str(r#"{{name | default:"anon"}}"#).unwrap();
        assert_eq!(t.render(&ctx(&[("name", "Bob")])).unwrap(), "Bob");
    }

    #[test]
    fn test_filter_default_missing() {
        let t = PromptTemplate::from_str(r#"{{name | default:"anon"}}"#).unwrap();
        assert_eq!(t.render(&ctx(&[])).unwrap(), "anon");
    }

    #[test]
    fn test_invalid_filter_name() {
        let result = PromptTemplate::from_str("{{x | frobnicate}}");
        assert!(matches!(result.unwrap_err(), TemplateError::InvalidFilter(_)));
    }

    #[test]
    fn test_unclosed_brace_parse_error() {
        let result = PromptTemplate::from_str("{{unclosed");
        assert!(matches!(result.unwrap_err(), TemplateError::ParseError(_)));
    }

    #[test]
    fn test_number_value() {
        let t = PromptTemplate::from_str("Count: {{n}}").unwrap();
        let mut c = TemplateContext::new();
        c.set("n", TemplateValue::Number(42.0));
        assert_eq!(t.render(&c).unwrap(), "Count: 42");
    }

    #[test]
    fn test_bool_value() {
        let t = PromptTemplate::from_str("Active: {{flag}}").unwrap();
        let mut c = TemplateContext::new();
        c.set("flag", TemplateValue::Bool(true));
        assert_eq!(t.render(&c).unwrap(), "Active: true");
    }

    #[test]
    fn test_list_value() {
        let t = PromptTemplate::from_str("Items: {{list}}").unwrap();
        let mut c = TemplateContext::new();
        c.set(
            "list",
            TemplateValue::List(vec!["a".to_string(), "b".to_string(), "c".to_string()]),
        );
        assert_eq!(t.render(&c).unwrap(), "Items: a, b, c");
    }

    #[test]
    fn test_library_register_and_render() {
        let mut lib = TemplateLibrary::new();
        lib.register("greet", "Hi {{name}}!").unwrap();
        let rendered = lib.render("greet", &ctx(&[("name", "World")])).unwrap();
        assert_eq!(rendered, "Hi World!");
    }

    #[test]
    fn test_library_list() {
        let mut lib = TemplateLibrary::new();
        lib.register("a", "{{x}}").unwrap();
        lib.register("b", "{{y}}").unwrap();
        let mut names = lib.list();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn test_library_missing_template() {
        let lib = TemplateLibrary::new();
        let err = lib.render("nonexistent", &ctx(&[])).unwrap_err();
        assert!(matches!(err, TemplateError::MissingVariable(_)));
    }

    #[test]
    fn test_library_contains() {
        let mut lib = TemplateLibrary::new();
        lib.register("t", "x").unwrap();
        assert!(lib.contains("t"));
        assert!(!lib.contains("missing"));
    }

    #[test]
    fn test_chained_filters_upper_truncate() {
        let t = PromptTemplate::from_str("{{text | upper | truncate:3}}").unwrap();
        assert_eq!(t.render(&ctx(&[("text", "hello")])).unwrap(), "HEL");
    }

    #[test]
    fn test_no_substitutions() {
        let t = PromptTemplate::from_str("plain text").unwrap();
        assert_eq!(t.render(&ctx(&[])).unwrap(), "plain text");
    }

    #[test]
    fn test_adjacent_substitutions() {
        let t = PromptTemplate::from_str("{{a}}{{b}}").unwrap();
        assert_eq!(t.render(&ctx(&[("a", "foo"), ("b", "bar")])).unwrap(), "foobar");
    }

    #[test]
    fn test_empty_template() {
        let t = PromptTemplate::from_str("").unwrap();
        assert_eq!(t.render(&ctx(&[])).unwrap(), "");
    }

    #[test]
    fn test_source_preserved() {
        let src = "Hello, {{name}}!";
        let t = PromptTemplate::from_str(src).unwrap();
        assert_eq!(t.source(), src);
    }
}
