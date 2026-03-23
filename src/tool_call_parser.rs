//! Tool call parsing and schema validation for LLM output.
//!
//! Provides [`ToolCallParser`] which scans raw LLM text for JSON tool-call
//! objects, extracts structured [`ToolCall`] values, and validates them against
//! registered [`ToolSchema`] definitions.

use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Parameter / Schema types
// ---------------------------------------------------------------------------

/// A single parameter in a tool schema.
#[derive(Debug, Clone)]
pub struct ToolParameter {
    /// Parameter name.
    pub name: String,
    /// Parameter type (e.g. `"string"`, `"integer"`).
    pub param_type: String,
    /// Whether this parameter must be present in every call.
    pub required: bool,
    /// Human-readable description for documentation / prompting.
    pub description: String,
}

/// Full schema for a single tool.
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// Canonical tool name matched against `"tool"` in LLM output.
    pub name: String,
    /// Short description of what the tool does.
    pub description: String,
    /// Declared parameters.
    pub parameters: Vec<ToolParameter>,
}

// ---------------------------------------------------------------------------
// ToolCall
// ---------------------------------------------------------------------------

/// A parsed (and optionally validated) tool invocation.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Name of the tool being invoked.
    pub tool_name: String,
    /// Flattened key→value argument map (all values stringified).
    pub arguments: HashMap<String, String>,
    /// The raw JSON text that was parsed.
    pub raw_text: String,
}

// ---------------------------------------------------------------------------
// ParseError
// ---------------------------------------------------------------------------

/// Errors that can occur while parsing or validating a tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// The JSON was structurally invalid.
    MalformedJson,
    /// A required field was absent from the JSON object.
    MissingField(String),
    /// The tool name is not registered with this parser.
    UnknownTool(String),
    /// An argument value has the wrong type.
    InvalidArgType {
        /// Parameter name.
        param: String,
        /// Expected type string.
        expected: String,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::MalformedJson => write!(f, "malformed JSON in tool call"),
            ParseError::MissingField(field) => write!(f, "missing required field: {}", field),
            ParseError::UnknownTool(name) => write!(f, "unknown tool: {}", name),
            ParseError::InvalidArgType { param, expected } => {
                write!(f, "argument '{}' expected type '{}'", param, expected)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ToolCallParser
// ---------------------------------------------------------------------------

/// Parses tool calls from LLM text and validates them against registered schemas.
pub struct ToolCallParser {
    /// Registered schemas keyed by tool name.
    pub schemas: HashMap<String, ToolSchema>,
}

impl ToolCallParser {
    /// Creates a new parser with no registered schemas.
    pub fn new() -> Self {
        ToolCallParser {
            schemas: HashMap::new(),
        }
    }

    /// Registers a tool schema so calls to that tool can be validated.
    pub fn register_tool(&mut self, schema: ToolSchema) {
        self.schemas.insert(schema.name.clone(), schema);
    }

    /// Finds all JSON tool-call objects in `text` and attempts to parse each.
    ///
    /// Matches `{"tool": "...", "arguments": {...}}` patterns.  Arguments are
    /// flattened to `HashMap<String, String>` by converting every value to its
    /// JSON string representation.
    pub fn parse_tool_calls(&self, text: &str) -> Vec<Result<ToolCall, ParseError>> {
        let candidates = self.extract_json_objects(text);
        candidates
            .into_iter()
            .filter_map(|raw| {
                // Only handle objects that look like tool calls
                if !raw.contains("\"tool\"") && !raw.contains("'tool'") {
                    return None;
                }
                Some(self.parse_single(raw))
            })
            .collect()
    }

    fn parse_single(&self, raw: &str) -> Result<ToolCall, ParseError> {
        // Minimal JSON object parser – we only need "tool" and "arguments".
        let obj = parse_json_object(raw).ok_or(ParseError::MalformedJson)?;

        let tool_name = obj
            .get("tool")
            .cloned()
            .ok_or_else(|| ParseError::MissingField("tool".to_string()))?;

        let args_raw = obj
            .get("arguments")
            .cloned()
            .ok_or_else(|| ParseError::MissingField("arguments".to_string()))?;

        // Parse the arguments sub-object
        let arguments = parse_flat_object(&args_raw).ok_or(ParseError::MalformedJson)?;

        Ok(ToolCall {
            tool_name,
            arguments,
            raw_text: raw.to_string(),
        })
    }

    /// Validates a parsed [`ToolCall`] against registered schemas.
    ///
    /// Returns `Ok(())` if the tool is known and all required params are present.
    pub fn validate_call(&self, call: &ToolCall) -> Result<(), ParseError> {
        let schema = self
            .schemas
            .get(&call.tool_name)
            .ok_or_else(|| ParseError::UnknownTool(call.tool_name.clone()))?;

        for param in &schema.parameters {
            if param.required && !call.arguments.contains_key(&param.name) {
                return Err(ParseError::MissingField(param.name.clone()));
            }
        }
        Ok(())
    }

    /// Extracts all top-level balanced `{…}` blocks from `text`.
    ///
    /// Handles nested braces correctly; does not cross string boundaries in a
    /// fully general way but is sufficient for typical LLM JSON output.
    pub fn extract_json_objects<'a>(&self, text: &'a str) -> Vec<&'a str> {
        extract_balanced_braces(text)
    }

    /// Formats a tool result in the expected XML-like envelope.
    pub fn format_tool_result(tool_name: &str, result: &str) -> String {
        format!("<tool_result name=\"{}\">{}</tool_result>", tool_name, result)
    }
}

impl Default for ToolCallParser {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ToolCallBuilder
// ---------------------------------------------------------------------------

/// Fluent builder for constructing [`ToolCall`] values programmatically.
pub struct ToolCallBuilder {
    tool_name: String,
    arguments: HashMap<String, String>,
}

impl ToolCallBuilder {
    /// Creates a builder for the named tool.
    pub fn new(tool_name: &str) -> Self {
        ToolCallBuilder {
            tool_name: tool_name.to_string(),
            arguments: HashMap::new(),
        }
    }

    /// Adds a key/value argument and returns `self` for chaining.
    pub fn arg(mut self, key: &str, value: &str) -> Self {
        self.arguments.insert(key.to_string(), value.to_string());
        self
    }

    /// Consumes the builder and produces a [`ToolCall`].
    pub fn build(self) -> ToolCall {
        // Synthesise a minimal raw JSON representation
        let args_json: String = self
            .arguments
            .iter()
            .map(|(k, v)| format!("\"{}\":\"{}\"", k, v))
            .collect::<Vec<_>>()
            .join(",");
        let raw_text = format!(
            "{{\"tool\":\"{}\",\"arguments\":{{{}}}}}",
            self.tool_name, args_json
        );
        ToolCall {
            tool_name: self.tool_name,
            arguments: self.arguments,
            raw_text,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Returns slices of `text` that are balanced `{…}` blocks.
fn extract_balanced_braces(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut results = Vec::new();
    let mut depth: usize = 0;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];

        if escape {
            escape = false;
            i += 1;
            continue;
        }

        if in_string {
            match b {
                b'\\' => escape = true,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(s) = start.take() {
                            results.push(&text[s..=i]);
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    results
}

/// Very small JSON object parser: returns a flat `HashMap<String, String>` where
/// values are their raw JSON representations (string contents are unquoted).
fn parse_json_object(s: &str) -> Option<HashMap<String, String>> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut map = HashMap::new();
    parse_kv_pairs(inner, &mut map);
    Some(map)
}

/// Parse key-value pairs from a JSON object body (no outer braces).
fn parse_kv_pairs(s: &str, map: &mut HashMap<String, String>) {
    let bytes = s.as_bytes();
    let mut i = 0;

    loop {
        // Skip whitespace and commas
        while i < bytes.len() && (bytes[i] == b',' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Expect a quoted key
        if bytes[i] != b'"' {
            break;
        }
        let (key, next) = match read_string(s, i) {
            Some(v) => v,
            None => break,
        };
        i = next;

        // Skip whitespace and colon
        while i < bytes.len() && (bytes[i] == b':' || bytes[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // Read value
        let (value, next) = match read_value(s, i) {
            Some(v) => v,
            None => break,
        };
        i = next;

        map.insert(key, value);
    }
}

/// Reads a JSON string starting at `pos` (which must point at `"`).
/// Returns (unescaped content, index after closing quote).
fn read_string(s: &str, pos: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    debug_assert_eq!(bytes[pos], b'"');
    let mut i = pos + 1;
    let mut result = String::new();
    let mut escape = false;

    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            match b {
                b'"' => result.push('"'),
                b'\\' => result.push('\\'),
                b'n' => result.push('\n'),
                b'r' => result.push('\r'),
                b't' => result.push('\t'),
                _ => {
                    result.push('\\');
                    result.push(b as char);
                }
            }
            escape = false;
        } else if b == b'\\' {
            escape = true;
        } else if b == b'"' {
            return Some((result, i + 1));
        } else {
            result.push(b as char);
        }
        i += 1;
    }
    None
}

/// Reads a JSON value (string, object, array, or primitive) starting at `pos`.
/// Returns (raw string representation, index after value).
fn read_value(s: &str, pos: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if pos >= bytes.len() {
        return None;
    }
    match bytes[pos] {
        b'"' => {
            let (content, next) = read_string(s, pos)?;
            Some((content, next))
        }
        b'{' => {
            let end = find_matching_close(bytes, pos, b'{', b'}')?;
            Some((s[pos..=end].to_string(), end + 1))
        }
        b'[' => {
            let end = find_matching_close(bytes, pos, b'[', b']')?;
            Some((s[pos..=end].to_string(), end + 1))
        }
        _ => {
            // Primitive: read until comma, }, ] or whitespace
            let start = pos;
            let mut i = pos;
            while i < bytes.len() && bytes[i] != b',' && bytes[i] != b'}' && bytes[i] != b']' {
                i += 1;
            }
            Some((s[start..i].trim().to_string(), i))
        }
    }
}

fn find_matching_close(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for i in start..bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            continue;
        }
        if in_str {
            match b {
                b'\\' => escape = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        if b == b'"' {
            in_str = true;
        } else if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
    }
    None
}

/// Parse a JSON object body and return a flat HashMap where nested objects
/// are kept as their raw JSON string.
fn parse_flat_object(s: &str) -> Option<HashMap<String, String>> {
    let s = s.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut map = HashMap::new();
    parse_kv_pairs(inner, &mut map);
    Some(map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_parser() -> ToolCallParser {
        let mut p = ToolCallParser::new();
        p.register_tool(ToolSchema {
            name: "search".to_string(),
            description: "Search the web".to_string(),
            parameters: vec![
                ToolParameter {
                    name: "query".to_string(),
                    param_type: "string".to_string(),
                    required: true,
                    description: "Search query".to_string(),
                },
                ToolParameter {
                    name: "limit".to_string(),
                    param_type: "integer".to_string(),
                    required: false,
                    description: "Max results".to_string(),
                },
            ],
        });
        p
    }

    #[test]
    fn test_parse_valid_call() {
        let parser = make_parser();
        let text = r#"Some preamble {"tool":"search","arguments":{"query":"Rust async"}} tail"#;
        let results = parser.parse_tool_calls(text);
        assert_eq!(results.len(), 1);
        let call = results[0].as_ref().unwrap();
        assert_eq!(call.tool_name, "search");
        assert_eq!(call.arguments.get("query").unwrap(), "Rust async");
    }

    #[test]
    fn test_missing_required_param() {
        let parser = make_parser();
        let call = ToolCallBuilder::new("search").build(); // no "query"
        let err = parser.validate_call(&call).unwrap_err();
        assert_eq!(err, ParseError::MissingField("query".to_string()));
    }

    #[test]
    fn test_unknown_tool() {
        let parser = make_parser();
        let call = ToolCallBuilder::new("nonexistent").arg("x", "y").build();
        let err = parser.validate_call(&call).unwrap_err();
        assert_eq!(err, ParseError::UnknownTool("nonexistent".to_string()));
    }

    #[test]
    fn test_malformed_json_handled() {
        let parser = make_parser();
        // Text contains { but it is not a valid tool call JSON
        let text = r#"{"tool": incomplete"#;
        // extract_json_objects won't find a balanced block, so no results
        let results = parser.parse_tool_calls(text);
        assert!(results.is_empty());
    }

    #[test]
    fn test_extract_json_objects_nested_braces() {
        let parser = ToolCallParser::new();
        let text = r#"before {"a":{"b":1}} middle {"c":2} after"#;
        let objs = parser.extract_json_objects(text);
        assert_eq!(objs.len(), 2);
        assert!(objs[0].contains("\"a\""));
        assert!(objs[1].contains("\"c\""));
    }

    #[test]
    fn test_builder_and_format() {
        let call = ToolCallBuilder::new("calculator")
            .arg("expression", "2+2")
            .build();
        assert_eq!(call.tool_name, "calculator");
        assert_eq!(call.arguments["expression"], "2+2");

        let result = ToolCallParser::format_tool_result("calculator", "4");
        assert_eq!(result, "<tool_result name=\"calculator\">4</tool_result>");
    }
}
