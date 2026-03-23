//! Jinja-lite template engine for prompt construction.
//!
//! Supports `{{ variable }}` substitution with filters, `{% if/elif/else/endif %}` blocks,
//! `{% for item in list %}...{% endfor %}` loops, and `{# comment #}` removal.

use std::collections::HashMap;
use std::fmt;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors that can occur during template parsing or rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateError {
    /// A block (`{%`, `{{`, `{#`) was opened but never closed.
    UnclosedBlock,
    /// A variable referenced in the template is not present in the context.
    UnknownVariable(String),
    /// The template source contains invalid syntax.
    InvalidSyntax(String),
    /// Template blocks are nested beyond the allowed limit.
    NestedBlockLimit,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::UnclosedBlock => write!(f, "unclosed block in template"),
            TemplateError::UnknownVariable(v) => write!(f, "unknown variable: {v}"),
            TemplateError::InvalidSyntax(msg) => write!(f, "invalid syntax: {msg}"),
            TemplateError::NestedBlockLimit => write!(f, "nested block limit exceeded"),
        }
    }
}

impl std::error::Error for TemplateError {}

// ── TemplateVar ───────────────────────────────────────────────────────────────

/// A dynamically-typed value that can be stored in a [`TemplateContext`].
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateVar {
    /// A UTF-8 string.
    Str(String),
    /// A 64-bit signed integer.
    Int(i64),
    /// A 64-bit floating-point number.
    Float(f64),
    /// A boolean.
    Bool(bool),
    /// An ordered list of [`TemplateVar`] values.
    List(Vec<TemplateVar>),
    /// A string-keyed map of [`TemplateVar`] values.
    Map(HashMap<String, TemplateVar>),
}

impl TemplateVar {
    /// Convert this value to its string representation.
    pub fn as_str(&self) -> String {
        match self {
            TemplateVar::Str(s) => s.clone(),
            TemplateVar::Int(i) => i.to_string(),
            TemplateVar::Float(f) => f.to_string(),
            TemplateVar::Bool(b) => b.to_string(),
            TemplateVar::List(items) => {
                let parts: Vec<String> = items.iter().map(|v| v.as_str()).collect();
                format!("[{}]", parts.join(", "))
            }
            TemplateVar::Map(m) => {
                let parts: Vec<String> = m.iter().map(|(k, v)| format!("{k}: {}", v.as_str())).collect();
                format!("{{{}}}", parts.join(", "))
            }
        }
    }

    /// Convert this value to bool (for conditionals).
    pub fn as_bool(&self) -> bool {
        self.is_truthy()
    }

    /// Whether this value is considered truthy.
    pub fn is_truthy(&self) -> bool {
        match self {
            TemplateVar::Bool(b) => *b,
            TemplateVar::Int(i) => *i != 0,
            TemplateVar::Float(f) => *f != 0.0,
            TemplateVar::Str(s) => !s.is_empty(),
            TemplateVar::List(l) => !l.is_empty(),
            TemplateVar::Map(m) => !m.is_empty(),
        }
    }
}

// ── TemplateContext ───────────────────────────────────────────────────────────

/// A variable binding context passed to [`Template::render`].
#[derive(Debug, Clone, Default)]
pub struct TemplateContext {
    vars: HashMap<String, TemplateVar>,
}

impl TemplateContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert any [`TemplateVar`] under `key`.
    pub fn set(&mut self, key: &str, val: TemplateVar) {
        self.vars.insert(key.to_string(), val);
    }

    /// Retrieve a variable by key.
    pub fn get(&self, key: &str) -> Option<&TemplateVar> {
        self.vars.get(key)
    }

    /// Convenience: insert a string variable.
    pub fn set_str(&mut self, key: &str, val: &str) {
        self.set(key, TemplateVar::Str(val.to_string()));
    }

    /// Convenience: insert an integer variable.
    pub fn set_int(&mut self, key: &str, val: i64) {
        self.set(key, TemplateVar::Int(val));
    }

    /// Convenience: insert a boolean variable.
    pub fn set_bool(&mut self, key: &str, val: bool) {
        self.set(key, TemplateVar::Bool(val));
    }

    /// Convenience: insert a list variable.
    pub fn set_list(&mut self, key: &str, val: Vec<TemplateVar>) {
        self.set(key, TemplateVar::List(val));
    }
}

// ── Internal AST ─────────────────────────────────────────────────────────────

/// Internal pre-parsed node.
#[derive(Debug, Clone)]
enum Node {
    /// Literal text, emitted verbatim.
    Text(String),
    /// `{{ expr }}` — variable substitution, possibly with a filter chain.
    Expr(String),
    /// `{% if cond %}` block. Contains (condition, body, optional elif/else chain).
    If {
        branches: Vec<(String, Vec<Node>)>, // (condition, nodes)
        else_branch: Option<Vec<Node>>,
    },
    /// `{% for item in list %}` loop.
    For {
        item: String,
        list: String,
        body: Vec<Node>,
    },
}

// ── Template ──────────────────────────────────────────────────────────────────

/// A pre-parsed template that can be rendered repeatedly with different contexts.
#[derive(Debug, Clone)]
pub struct Template {
    source: String,
    nodes: Vec<Node>,
}

const MAX_NEST: usize = 32;

impl Template {
    /// Parse `source` into a [`Template`].
    pub fn new(source: &str) -> Result<Self, TemplateError> {
        let nodes = parse(source)?;
        Ok(Self { source: source.to_string(), nodes })
    }

    /// Render this template with the given context.
    pub fn render(&self, ctx: &TemplateContext) -> Result<String, TemplateError> {
        render_nodes(&self.nodes, ctx)
    }

    /// Return all variable names referenced in `{{ ... }}` expressions.
    pub fn variables(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_variables(&self.nodes, &mut out);
        out.sort();
        out.dedup();
        out
    }

    /// Validate that all blocks are properly paired without rendering.
    pub fn validate(&self) -> Result<(), TemplateError> {
        // Parsing already validates block structure; re-parse to confirm.
        parse(&self.source).map(|_| ())
    }
}

fn collect_variables(nodes: &[Node], out: &mut Vec<String>) {
    for node in nodes {
        match node {
            Node::Text(_) => {}
            Node::Expr(expr) => {
                let var = expr.split('|').next().unwrap_or("").trim().to_string();
                // handle dot-access: take root
                let root = var.split('.').next().unwrap_or("").trim().to_string();
                if !root.is_empty() && !root.starts_with("loop.") {
                    out.push(root);
                }
            }
            Node::If { branches, else_branch } => {
                for (cond, body) in branches {
                    let root = cond.trim().split('.').next().unwrap_or("").trim().to_string();
                    if !root.is_empty() {
                        out.push(root);
                    }
                    collect_variables(body, out);
                }
                if let Some(eb) = else_branch {
                    collect_variables(eb, out);
                }
            }
            Node::For { list, body, .. } => {
                out.push(list.trim().to_string());
                collect_variables(body, out);
            }
        }
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Token produced by the lexer.
#[derive(Debug, Clone)]
enum Token {
    Text(String),
    Expr(String),   // {{ ... }}
    Tag(String),    // {% ... %}
    Comment,        // {# ... #}
}

fn lex(source: &str) -> Result<Vec<Token>, TemplateError> {
    let mut tokens = Vec::new();
    let mut chars: &str = source;

    while !chars.is_empty() {
        if chars.starts_with("{{") {
            let end = chars.find("}}").ok_or(TemplateError::UnclosedBlock)?;
            let inner = chars[2..end].trim().to_string();
            tokens.push(Token::Expr(inner));
            chars = &chars[end + 2..];
        } else if chars.starts_with("{%") {
            let end = chars.find("%}").ok_or(TemplateError::UnclosedBlock)?;
            let inner = chars[2..end].trim().to_string();
            tokens.push(Token::Tag(inner));
            chars = &chars[end + 2..];
        } else if chars.starts_with("{#") {
            let end = chars.find("#}").ok_or(TemplateError::UnclosedBlock)?;
            tokens.push(Token::Comment);
            chars = &chars[end + 2..];
        } else {
            // Find the next `{{`, `{%`, or `{#`
            let next = chars.find("{%")
                .unwrap_or(chars.len())
                .min(chars.find("{{").unwrap_or(chars.len()))
                .min(chars.find("{#").unwrap_or(chars.len()));
            tokens.push(Token::Text(chars[..next].to_string()));
            chars = &chars[next..];
        }
    }

    Ok(tokens)
}

/// Recursive descent parser — returns a list of top-level nodes.
fn parse(source: &str) -> Result<Vec<Node>, TemplateError> {
    let tokens = lex(source)?;
    let mut pos = 0;
    let nodes = parse_nodes(&tokens, &mut pos, 0)?;
    Ok(nodes)
}

fn parse_nodes(tokens: &[Token], pos: &mut usize, depth: usize) -> Result<Vec<Node>, TemplateError> {
    if depth > MAX_NEST {
        return Err(TemplateError::NestedBlockLimit);
    }
    let mut nodes = Vec::new();

    while *pos < tokens.len() {
        match &tokens[*pos] {
            Token::Text(t) => {
                nodes.push(Node::Text(t.clone()));
                *pos += 1;
            }
            Token::Expr(e) => {
                nodes.push(Node::Expr(e.clone()));
                *pos += 1;
            }
            Token::Comment => {
                *pos += 1;
            }
            Token::Tag(tag) => {
                let t = tag.as_str();
                if t == "endif" || t == "endfor" || t.starts_with("else") || t.starts_with("elif") {
                    // Stop: caller handles these.
                    break;
                } else if t.starts_with("if ") {
                    *pos += 1;
                    let node = parse_if(tokens, pos, t, depth)?;
                    nodes.push(node);
                } else if t.starts_with("for ") {
                    *pos += 1;
                    let node = parse_for(tokens, pos, t, depth)?;
                    nodes.push(node);
                } else {
                    return Err(TemplateError::InvalidSyntax(format!("unknown tag: {t}")));
                }
            }
        }
    }

    Ok(nodes)
}

fn parse_if(tokens: &[Token], pos: &mut usize, first_tag: &str, depth: usize) -> Result<Node, TemplateError> {
    // first_tag = "if <cond>"
    let first_cond = first_tag["if ".len()..].trim().to_string();
    let first_body = parse_nodes(tokens, pos, depth + 1)?;

    let mut branches: Vec<(String, Vec<Node>)> = vec![(first_cond, first_body)];
    let mut else_branch: Option<Vec<Node>> = None;

    loop {
        if *pos >= tokens.len() {
            return Err(TemplateError::UnclosedBlock);
        }
        match &tokens[*pos] {
            Token::Tag(tag) => {
                let t = tag.as_str();
                if t == "endif" {
                    *pos += 1;
                    break;
                } else if t.starts_with("elif ") {
                    let cond = t["elif ".len()..].trim().to_string();
                    *pos += 1;
                    let body = parse_nodes(tokens, pos, depth + 1)?;
                    branches.push((cond, body));
                } else if t == "else" {
                    *pos += 1;
                    let body = parse_nodes(tokens, pos, depth + 1)?;
                    else_branch = Some(body);
                } else {
                    return Err(TemplateError::InvalidSyntax(format!("unexpected tag in if: {t}")));
                }
            }
            _ => return Err(TemplateError::UnclosedBlock),
        }
    }

    Ok(Node::If { branches, else_branch })
}

fn parse_for(tokens: &[Token], pos: &mut usize, tag: &str, depth: usize) -> Result<Node, TemplateError> {
    // tag = "for <item> in <list>"
    let rest = &tag["for ".len()..];
    let parts: Vec<&str> = rest.splitn(3, ' ').collect();
    if parts.len() < 3 || parts[1] != "in" {
        return Err(TemplateError::InvalidSyntax(format!("malformed for tag: {tag}")));
    }
    let item = parts[0].trim().to_string();
    let list = parts[2].trim().to_string();

    let body = parse_nodes(tokens, pos, depth + 1)?;

    if *pos >= tokens.len() {
        return Err(TemplateError::UnclosedBlock);
    }
    match &tokens[*pos] {
        Token::Tag(t) if t == "endfor" => {
            *pos += 1;
        }
        _ => return Err(TemplateError::UnclosedBlock),
    }

    Ok(Node::For { item, list, body })
}

// ── Renderer ──────────────────────────────────────────────────────────────────

fn render_nodes(nodes: &[Node], ctx: &TemplateContext) -> Result<String, TemplateError> {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Expr(expr) => {
                out.push_str(&eval_expr(expr, ctx)?);
            }
            Node::If { branches, else_branch } => {
                let mut matched = false;
                for (cond, body) in branches {
                    if eval_condition(cond, ctx)? {
                        out.push_str(&render_nodes(body, ctx)?);
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    if let Some(eb) = else_branch {
                        out.push_str(&render_nodes(eb, ctx)?);
                    }
                }
            }
            Node::For { item, list, body } => {
                let list_var = ctx.get(list).ok_or_else(|| TemplateError::UnknownVariable(list.clone()))?;
                let items = match list_var {
                    TemplateVar::List(l) => l.clone(),
                    other => vec![other.clone()],
                };
                let len = items.len();
                for (i, val) in items.into_iter().enumerate() {
                    let mut inner_ctx = ctx.clone();
                    inner_ctx.set(item, val);
                    inner_ctx.set("loop.index", TemplateVar::Int((i + 1) as i64));
                    inner_ctx.set("loop.index0", TemplateVar::Int(i as i64));
                    inner_ctx.set("loop.first", TemplateVar::Bool(i == 0));
                    inner_ctx.set("loop.last", TemplateVar::Bool(i == len - 1));
                    out.push_str(&render_nodes(body, &inner_ctx)?);
                }
            }
        }
    }
    Ok(out)
}

/// Evaluate a `{{ expr }}` expression (variable lookup + optional filters).
fn eval_expr(expr: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
    // Handle loop.* pseudo-variables.
    if expr.starts_with("loop.") {
        let key = expr.trim();
        return match ctx.get(key) {
            Some(v) => Ok(v.as_str()),
            None => Ok(String::new()),
        };
    }

    let parts: Vec<&str> = expr.splitn(2, '|').collect();
    let var_name = parts[0].trim();

    // Resolve variable (support dot-path like "obj.field").
    let value = resolve_var(var_name, ctx)?;

    // Apply filters if any.
    if parts.len() == 2 {
        apply_filter(value, parts[1].trim())
    } else {
        Ok(value)
    }
}

fn resolve_var(name: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
    // Support simple dot-path: "a.b" -> ctx["a"] as Map -> "b"
    let segments: Vec<&str> = name.splitn(2, '.').collect();
    let root = segments[0].trim();

    match ctx.get(root) {
        Some(TemplateVar::Map(m)) if segments.len() == 2 => {
            let key = segments[1].trim();
            match m.get(key) {
                Some(v) => Ok(v.as_str()),
                None => Err(TemplateError::UnknownVariable(name.to_string())),
            }
        }
        Some(v) => Ok(v.as_str()),
        None => Err(TemplateError::UnknownVariable(root.to_string())),
    }
}

fn apply_filter(value: String, filter_expr: &str) -> Result<String, TemplateError> {
    // Filters can be chained: `upper | trim`
    let mut result = value;
    for part in filter_expr.split('|') {
        let f = part.trim();
        result = apply_single_filter(result, f)?;
    }
    Ok(result)
}

fn apply_single_filter(value: String, filter: &str) -> Result<String, TemplateError> {
    if filter == "upper" {
        Ok(value.to_uppercase())
    } else if filter == "lower" {
        Ok(value.to_lowercase())
    } else if filter == "trim" {
        Ok(value.trim().to_string())
    } else if let Some(rest) = filter.strip_prefix("truncate:") {
        let n: usize = rest.trim().parse().map_err(|_| TemplateError::InvalidSyntax(format!("truncate: expected number, got {rest}")))?;
        if value.len() > n {
            Ok(format!("{}...", &value[..n]))
        } else {
            Ok(value)
        }
    } else if let Some(rest) = filter.strip_prefix("default:") {
        if value.is_empty() { Ok(rest.trim().to_string()) } else { Ok(value) }
    } else {
        Err(TemplateError::InvalidSyntax(format!("unknown filter: {filter}")))
    }
}

fn eval_condition(cond: &str, ctx: &TemplateContext) -> Result<bool, TemplateError> {
    let cond = cond.trim();

    // Handle "not X"
    if let Some(inner) = cond.strip_prefix("not ") {
        return eval_condition(inner.trim(), ctx).map(|b| !b);
    }

    // Handle "X == Y"
    if let Some(idx) = cond.find(" == ") {
        let lhs = cond[..idx].trim();
        let rhs = cond[idx + 4..].trim().trim_matches('"');
        let lhs_val = resolve_var(lhs, ctx).unwrap_or_default();
        return Ok(lhs_val == rhs);
    }

    // Handle "X != Y"
    if let Some(idx) = cond.find(" != ") {
        let lhs = cond[..idx].trim();
        let rhs = cond[idx + 4..].trim().trim_matches('"');
        let lhs_val = resolve_var(lhs, ctx).unwrap_or_default();
        return Ok(lhs_val != rhs);
    }

    // Simple truthiness check.
    match ctx.get(cond) {
        Some(v) => Ok(v.is_truthy()),
        None => Ok(false),
    }
}

// ── TemplateLibrary ───────────────────────────────────────────────────────────

/// A named collection of pre-parsed templates.
#[derive(Debug, Default)]
pub struct TemplateLibrary {
    templates: HashMap<String, Template>,
}

impl TemplateLibrary {
    /// Create an empty library.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a template under `name`, parsing it immediately.
    pub fn register(&mut self, name: &str, source: &str) -> Result<(), TemplateError> {
        let tpl = Template::new(source)?;
        self.templates.insert(name.to_string(), tpl);
        Ok(())
    }

    /// Render a named template.
    pub fn render(&self, name: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
        match self.templates.get(name) {
            Some(t) => t.render(ctx),
            None => Err(TemplateError::UnknownVariable(name.to_string())),
        }
    }

    /// Create a library pre-loaded with common prompt templates.
    pub fn built_in_templates() -> Self {
        let mut lib = Self::new();

        lib.register(
            "chat_completion",
            r#"{# chat_completion template #}
{% if system_prompt %}System: {{ system_prompt }}

{% endif %}{% for msg in messages %}{{ msg }}
{% endfor %}Assistant:"#,
        ).ok();

        lib.register(
            "few_shot",
            r#"{# few_shot template #}
{{ instruction }}

{% for example in examples %}Example {{ loop.index }}:
Input: {{ example }}
Output: [see below]

{% endfor %}Now answer:
Input: {{ input }}"#,
        ).ok();

        lib.register(
            "chain_of_thought",
            r#"{# chain_of_thought template #}
{{ problem }}

Let's think step by step.
{% if context %}
Context: {{ context }}
{% endif %}
Answer:"#,
        ).ok();

        lib.register(
            "summarize",
            r#"{# summarize template #}
Please summarize the following text{% if max_words %} in at most {{ max_words }} words{% endif %}:

{{ text }}

Summary:"#,
        ).ok();

        lib.register(
            "translate",
            r#"{# translate template #}
Translate the following text from {{ source_lang | default:English }} to {{ target_lang }}:

{{ text }}

Translation:"#,
        ).ok();

        lib
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_variable() {
        let tpl = Template::new("Hello, {{ name }}!").unwrap();
        let mut ctx = TemplateContext::new();
        ctx.set_str("name", "World");
        assert_eq!(tpl.render(&ctx).unwrap(), "Hello, World!");
    }

    #[test]
    fn test_filter_upper() {
        let tpl = Template::new("{{ name | upper }}").unwrap();
        let mut ctx = TemplateContext::new();
        ctx.set_str("name", "hello");
        assert_eq!(tpl.render(&ctx).unwrap(), "HELLO");
    }

    #[test]
    fn test_if_else() {
        let tpl = Template::new("{% if flag %}yes{% else %}no{% endif %}").unwrap();
        let mut ctx = TemplateContext::new();
        ctx.set_bool("flag", true);
        assert_eq!(tpl.render(&ctx).unwrap(), "yes");
        ctx.set_bool("flag", false);
        assert_eq!(tpl.render(&ctx).unwrap(), "no");
    }

    #[test]
    fn test_for_loop() {
        let tpl = Template::new("{% for item in items %}{{ item }} {% endfor %}").unwrap();
        let mut ctx = TemplateContext::new();
        ctx.set_list("items", vec![
            TemplateVar::Str("a".into()),
            TemplateVar::Str("b".into()),
        ]);
        assert_eq!(tpl.render(&ctx).unwrap(), "a b ");
    }

    #[test]
    fn test_comment_removed() {
        let tpl = Template::new("before{# ignored #}after").unwrap();
        let ctx = TemplateContext::new();
        assert_eq!(tpl.render(&ctx).unwrap(), "beforeafter");
    }

    #[test]
    fn test_built_in_templates() {
        let lib = TemplateLibrary::built_in_templates();
        let mut ctx = TemplateContext::new();
        ctx.set_str("text", "The quick brown fox.");
        ctx.set_str("target_lang", "French");
        let result = lib.render("translate", &ctx);
        assert!(result.is_ok());
    }
}
