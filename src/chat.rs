//! Structured chat requests: what a client asked for, carried to the worker.
//!
//! [`ModelWorker::infer`](crate::ModelWorker::infer) takes one prompt string,
//! which is enough for "answer this text" but loses everything else a chat
//! client sends. [`ChatRequest`] keeps it: the model name, the messages as the
//! client wrote them (including assistant `tool_calls` and `tool` results),
//! and the sampling and output settings (`temperature`, `top_p`,
//! `max_tokens` / `max_completion_tokens`, `stop`, `seed`,
//! `response_format`, `tools`, `tool_choice`, `parallel_tool_calls`).
//!
//! [`ModelWorker::chat`](crate::ModelWorker::chat) answers one with a
//! [`ChatResponse`], which carries the text, any tool calls the model made,
//! the finish reason and the provider's token usage. `OpenAiWorker` sends the
//! request to `/chat/completions` field for field, `AnthropicWorker`
//! translates it to the Messages API, and `LlamaCppWorker` sends it to
//! llama.cpp's OpenAI-compatible `/v1/chat/completions`. Workers that do not
//! override `chat` get a default that flattens the messages into one prompt
//! and calls `infer`, so the settings are ignored there.
//!
//! The OpenAI-compatible HTTP endpoint (`POST /v1/chat/completions`, feature
//! `web-api`) builds a `ChatRequest` for every call and runs it through the
//! pipeline, so the stage's circuit breaker, timeout, retries and
//! concurrency limit apply exactly as they do to plain prompts.
//!
//! # Deduplication
//!
//! [`ChatRequest::dedup_key`] is a SHA-256 over a canonical encoding of
//! everything that changes the answer: the route, the model, the messages,
//! the tools and tool choice, every sampling setting and the response
//! format. Object keys are sorted first, so two clients that send the same
//! request with fields in a different order share one answer, while two
//! requests that differ in any setting never do.
//! [`ChatRequest::is_cacheable`] says whether a completed answer may be
//! reused after the call finishes: not when `temperature > 0` without a
//! `seed`, because the client asked for a sampled answer.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One chat completion request, in OpenAI Chat Completions terms.
///
/// `messages` holds the client's message objects unchanged (`role`,
/// `content`, and for tool use `tool_calls`, `tool_call_id`, `name`). All
/// settings are optional; a worker sends only the ones that are set, and
/// falls back to its own configured defaults for the rest.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use tokio_prompt_orchestrator::ChatRequest;
///
/// let mut req = ChatRequest::new("gpt-4o-mini", vec![json!({"role": "user", "content": "Hi"})]);
/// req.temperature = Some(0.0);
/// req.max_tokens = Some(64);
/// assert!(req.is_cacheable());
/// assert_eq!(req.transcript(), "Hi");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatRequest {
    /// Model to call. Empty means "the worker's own model".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// The conversation, as OpenAI message objects.
    pub messages: Vec<Value>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Nucleus sampling cut-off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Reply length cap (`max_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Reply length cap for reasoning models (`max_completion_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Stop sequence(s): a string or an array of strings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Value>,
    /// Seed for reproducible sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Output format, e.g. `{"type": "json_object"}` or a `json_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    /// Tool (function) definitions the model may call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    /// `"auto"`, `"none"`, `"required"` or `{"type": "function", ...}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    /// Whether the model may call several tools in one turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

impl ChatRequest {
    /// A request for `model` with `messages` and no settings.
    pub fn new(model: impl Into<String>, messages: Vec<Value>) -> Self {
        Self {
            model: model.into(),
            messages,
            ..Self::default()
        }
    }

    /// The reply length cap the client asked for: `max_completion_tokens`,
    /// else `max_tokens`.
    pub fn reply_limit(&self) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens)
    }

    /// `true` when the request defines at least one tool. Without tools,
    /// `tool_choice` and `parallel_tool_calls` mean nothing and are not sent
    /// upstream (OpenAI answers 400 for either one without `tools`).
    pub fn has_tools(&self) -> bool {
        self.tools.as_ref().is_some_and(|t| !t.is_empty())
    }

    /// `stop` as a list (a single string becomes a one-element list).
    pub fn stop_sequences(&self) -> Vec<String> {
        match &self.stop {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `true` when a completed answer may be reused for an identical request
    /// that arrives later. A request with `temperature > 0` and no `seed`
    /// asks for a fresh sample each time, so it is only shared with
    /// identical requests that are still in flight. Without a temperature
    /// (the provider's default) or with a seed, the answer is reusable.
    pub fn is_cacheable(&self) -> bool {
        self.seed.is_some() || self.temperature.is_none_or(|t| t <= 0.0)
    }

    /// The canonical form of everything except the messages, as JSON.
    fn settings_value(&self, route: &str) -> Value {
        serde_json::json!({
            "route": route,
            "model": self.model,
            "temperature": self.temperature,
            "top_p": self.top_p,
            "max_tokens": self.max_tokens,
            "max_completion_tokens": self.max_completion_tokens,
            "stop": self.stop_sequences(),
            "seed": self.seed,
            "response_format": self.response_format,
            "tools": self.tools,
            "tool_choice": self.tool_choice,
            "parallel_tool_calls": self.parallel_tool_calls,
        })
    }

    /// Deduplication key: SHA-256 (hex) over a canonical encoding of `route`,
    /// the model, the messages, the tools and tool choice, every sampling
    /// setting and the response format.
    ///
    /// Object keys are sorted at every level, so field order does not matter;
    /// any difference in content or settings gives a different key. `route`
    /// names the upstream the request goes to, so the same model name served
    /// by two routes never shares an answer.
    ///
    /// # Examples
    ///
    /// ```
    /// use serde_json::json;
    /// use tokio_prompt_orchestrator::ChatRequest;
    ///
    /// let msgs = vec![json!({"role": "user", "content": "Hi"})];
    /// let mut a = ChatRequest::new("gpt-4o", msgs.clone());
    /// let mut b = ChatRequest::new("gpt-4o", msgs);
    /// assert_eq!(a.dedup_key("default"), b.dedup_key("default"));
    /// b.temperature = Some(0.9);
    /// assert_ne!(a.dedup_key("default"), b.dedup_key("default"));
    /// a.temperature = Some(0.9);
    /// assert_eq!(a.dedup_key("default"), b.dedup_key("default"));
    /// ```
    pub fn dedup_key(&self, route: &str) -> String {
        let mut text = String::from("chat-v1|");
        write_canonical(&self.settings_value(route), &mut text);
        text.push('|');
        write_canonical(&Value::Array(self.messages.clone()), &mut text);
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    /// SHA-256 (hex) over the settings alone (everything in
    /// [`dedup_key`](Self::dedup_key) except the messages). Requests with
    /// the same fingerprint differ only in what they say; semantic
    /// deduplication compares prompts only within one fingerprint.
    pub fn settings_fingerprint(&self, route: &str) -> String {
        let mut text = String::from("chat-settings-v1|");
        write_canonical(&self.settings_value(route), &mut text);
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    /// The conversation flattened into one prompt, for workers that take a
    /// single string. A lone user message is returned exactly as written;
    /// anything longer becomes a `Role: text` transcript separated by blank
    /// lines. Only text content is kept.
    pub fn transcript(&self) -> String {
        transcript(&self.messages)
    }

    /// The request as an OpenAI Chat Completions body. `model` fills in an
    /// empty [`model`](Self::model); `temperature` and `max_tokens` are the
    /// worker's own defaults, used only when the request leaves them unset.
    /// Without tools (see [`has_tools`](Self::has_tools)), `tools`,
    /// `tool_choice` and `parallel_tool_calls` are left out.
    pub fn to_openai_body(
        &self,
        model: &str,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
    ) -> Value {
        let mut body = serde_json::to_value(self).unwrap_or_else(|_| Value::Object(Default::default()));
        if let Value::Object(map) = &mut body {
            if self.model.is_empty() {
                map.insert("model".into(), Value::String(model.to_string()));
            }
            if !self.has_tools() {
                for key in ["tools", "tool_choice", "parallel_tool_calls"] {
                    map.remove(key);
                }
            }
            if self.temperature.is_none() {
                if let Some(t) = temperature {
                    map.insert("temperature".into(), serde_json::json!(t));
                }
            }
            if self.reply_limit().is_none() {
                if let Some(n) = max_tokens {
                    map.insert("max_tokens".into(), serde_json::json!(n));
                }
            }
        }
        body
    }
}

/// Label used for a message role in a flattened transcript.
fn role_label(role: &str) -> String {
    match role {
        "system" | "developer" => "System".to_string(),
        "user" => "User".to_string(),
        "assistant" => "Assistant".to_string(),
        "tool" | "function" => "Tool".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => "User".to_string(),
            }
        }
    }
}

/// The text of one message: a string, or the `text` parts of a content array.
pub(crate) fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Flatten OpenAI messages into one prompt (see [`ChatRequest::transcript`]).
pub(crate) fn transcript(messages: &[Value]) -> String {
    let turns: Vec<(String, String)> = messages
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
            (role_label(role), message_text(m))
        })
        .collect();
    if turns.len() == 1 && turns[0].0 == "User" {
        return turns[0].1.clone();
    }
    turns
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(label, text)| format!("{label}: {text}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Write `v` as JSON with object keys sorted at every level.
fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push('{');
            for (i, (k, val)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(val, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Token counts reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatUsage {
    /// Tokens in the request.
    pub prompt_tokens: u64,
    /// Tokens in the reply.
    pub completion_tokens: u64,
}

/// A worker's answer to a [`ChatRequest`].
///
/// # Examples
///
/// ```
/// use tokio_prompt_orchestrator::ChatResponse;
///
/// let r = ChatResponse::from_text("Hello");
/// assert_eq!(r.content.as_deref(), Some("Hello"));
/// assert_eq!(r.finish_reason.as_deref(), Some("stop"));
/// assert!(r.tool_calls.is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChatResponse {
    /// The reply text; `None` when the model only called tools.
    #[serde(default)]
    pub content: Option<String>,
    /// Tool calls in OpenAI's shape:
    /// `{"id", "type": "function", "function": {"name", "arguments"}}`.
    #[serde(default)]
    pub tool_calls: Vec<Value>,
    /// `stop`, `length`, `tool_calls` or `content_filter`.
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Token usage as the provider reported it, if it did.
    #[serde(default)]
    pub usage: Option<ChatUsage>,
}

impl ChatResponse {
    /// A plain text answer with finish reason `stop`.
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            content: Some(text.into()),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_string()),
            usage: None,
        }
    }

    /// The reply text, or `""` when there is none.
    pub fn text(&self) -> &str {
        self.content.as_deref().unwrap_or("")
    }

    /// Parse an OpenAI Chat Completions response body (first choice).
    ///
    /// # Errors
    ///
    /// Returns a description of the problem when the body has no choices.
    pub fn from_openai(body: &Value) -> Result<Self, String> {
        let choice = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .ok_or_else(|| "no choices in the response".to_string())?;
        let message = choice.get("message").cloned().unwrap_or(Value::Null);
        let content = match message.get("content") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(_)) => Some(message_text(&message)),
            _ => None,
        };
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let finish_reason = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let usage = body.get("usage").and_then(|u| {
            Some(ChatUsage {
                prompt_tokens: u.get("prompt_tokens")?.as_u64()?,
                completion_tokens: u.get("completion_tokens")?.as_u64()?,
            })
        });
        Ok(Self {
            content,
            tool_calls,
            finish_reason,
            usage,
        })
    }
}

// ============================================================================
// Request-scoped chat jobs
// ============================================================================

/// A chat request travelling through the pipeline under one request id.
///
/// The pipeline's stage types carry a prompt string; the structured request
/// rides alongside, keyed by request id, so the inference stage can call
/// [`ModelWorker::chat`](crate::ModelWorker::chat) and leave the structured
/// answer here for the caller.
pub(crate) struct ChatJob {
    pub(crate) request: ChatRequest,
    response: parking_lot::Mutex<Option<ChatResponse>>,
}

impl ChatJob {
    pub(crate) fn set_response(&self, response: ChatResponse) {
        *self.response.lock() = Some(response);
    }
}

static CHAT_JOBS: LazyLock<DashMap<String, Arc<ChatJob>>> = LazyLock::new(DashMap::new);

/// The chat job registered for `request_id`, if any.
pub(crate) fn job(request_id: &str) -> Option<Arc<ChatJob>> {
    CHAT_JOBS.get(request_id).map(|j| Arc::clone(&j))
}

/// Registration of a chat job; removed again when dropped.
#[cfg_attr(not(feature = "web-api"), allow(dead_code))]
pub(crate) struct ChatJobGuard {
    request_id: String,
    job: Arc<ChatJob>,
}

#[cfg_attr(not(feature = "web-api"), allow(dead_code))]
impl ChatJobGuard {
    /// Register `request` under `request_id` until the guard is dropped.
    pub(crate) fn register(request_id: &str, request: ChatRequest) -> Self {
        let job = Arc::new(ChatJob {
            request,
            response: parking_lot::Mutex::new(None),
        });
        CHAT_JOBS.insert(request_id.to_string(), Arc::clone(&job));
        Self {
            request_id: request_id.to_string(),
            job,
        }
    }

    /// The structured answer the inference stage left, if it left one.
    pub(crate) fn take_response(&self) -> Option<ChatResponse> {
        self.job.response.lock().take()
    }
}

impl Drop for ChatJobGuard {
    fn drop(&mut self) {
        CHAT_JOBS.remove(&self.request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> ChatRequest {
        ChatRequest::new("gpt-4o", vec![json!({"role": "user", "content": "Hi"})])
    }

    #[test]
    fn key_ignores_field_order_but_not_content() {
        let mut a = base();
        a.response_format = Some(json!({"type": "json_schema", "json_schema": {"name": "x", "schema": {"a": 1, "b": 2}}}));
        let mut b = base();
        b.response_format = Some(json!({"json_schema": {"schema": {"b": 2, "a": 1}, "name": "x"}, "type": "json_schema"}));
        assert_eq!(a.dedup_key("r"), b.dedup_key("r"));
        b.response_format = Some(json!({"type": "json_object"}));
        assert_ne!(a.dedup_key("r"), b.dedup_key("r"));
    }

    #[test]
    fn every_setting_changes_the_key() {
        let k = base().dedup_key("r");
        type Change = Box<dyn Fn(&mut ChatRequest)>;
        let variants: Vec<Change> = vec![
            Box::new(|r| r.model = "gpt-4o-mini".into()),
            Box::new(|r| r.messages.push(json!({"role": "user", "content": "more"}))),
            Box::new(|r| r.temperature = Some(0.0)),
            Box::new(|r| r.top_p = Some(0.5)),
            Box::new(|r| r.max_tokens = Some(10)),
            Box::new(|r| r.max_completion_tokens = Some(10)),
            Box::new(|r| r.stop = Some(json!("END"))),
            Box::new(|r| r.seed = Some(7)),
            Box::new(|r| r.response_format = Some(json!({"type": "json_object"}))),
            Box::new(|r| r.tools = Some(vec![json!({"type": "function", "function": {"name": "f"}})])),
            Box::new(|r| r.tool_choice = Some(json!("required"))),
            Box::new(|r| r.parallel_tool_calls = Some(false)),
        ];
        for (i, change) in variants.iter().enumerate() {
            let mut r = base();
            change(&mut r);
            assert_ne!(r.dedup_key("r"), k, "variant {i} must change the key");
        }
        assert_ne!(base().dedup_key("other-route"), k, "route is part of the key");
    }

    #[test]
    fn openai_body_drops_tool_settings_without_tools() {
        let mut r = base();
        r.tool_choice = Some(json!("auto"));
        r.parallel_tool_calls = Some(true);
        for tools in [None, Some(Vec::new())] {
            r.tools = tools;
            assert!(!r.has_tools());
            let body = r.to_openai_body("m", None, None);
            for key in ["tools", "tool_choice", "parallel_tool_calls"] {
                assert!(body.get(key).is_none(), "{key} sent without tools: {body}");
            }
        }
        r.tools = Some(vec![json!({"type": "function", "function": {"name": "f"}})]);
        assert!(r.has_tools());
        let body = r.to_openai_body("m", None, None);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn stop_string_and_one_element_list_are_the_same() {
        let mut a = base();
        a.stop = Some(json!("END"));
        let mut b = base();
        b.stop = Some(json!(["END"]));
        assert_eq!(a.dedup_key("r"), b.dedup_key("r"));
    }

    #[test]
    fn sampled_requests_are_not_cacheable_unless_seeded() {
        let mut r = base();
        assert!(r.is_cacheable(), "no temperature: provider default, cacheable");
        r.temperature = Some(0.0);
        assert!(r.is_cacheable());
        r.temperature = Some(0.7);
        assert!(!r.is_cacheable());
        r.seed = Some(1);
        assert!(r.is_cacheable());
    }

    #[test]
    fn transcript_matches_the_single_prompt_rules() {
        assert_eq!(base().transcript(), "Hi");
        let r = ChatRequest::new(
            "m",
            vec![
                json!({"role": "system", "content": "Be brief."}),
                json!({"role": "user", "content": [{"type": "text", "text": "Hi"}]}),
            ],
        );
        assert_eq!(r.transcript(), "System: Be brief.\n\nUser: Hi");
    }

    #[test]
    fn openai_body_fills_only_unset_defaults() {
        let mut r = base();
        r.model.clear();
        let body = r.to_openai_body("worker-model", Some(0.7), None);
        assert_eq!(body["model"], "worker-model");
        assert_eq!(body["temperature"], 0.7);
        assert!(body.get("max_tokens").is_none());
        r.model = "asked".into();
        r.temperature = Some(0.1);
        r.max_completion_tokens = Some(5);
        let body = r.to_openai_body("worker-model", Some(0.7), Some(99));
        assert_eq!(body["model"], "asked");
        assert_eq!(body["temperature"], 0.1);
        assert_eq!(body["max_completion_tokens"], 5);
        assert!(body.get("max_tokens").is_none(), "the client's own cap wins: {body}");
    }

    #[test]
    fn openai_response_with_tool_calls_parses() {
        let body = json!({
            "choices": [{
                "message": {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
                ]},
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}
        });
        let r = ChatResponse::from_openai(&body).expect("parses");
        assert_eq!(r.content, None);
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(r.usage, Some(ChatUsage { prompt_tokens: 12, completion_tokens: 3 }));
    }

    #[test]
    fn job_guard_registers_and_removes() {
        let id = "chat-job-guard-test";
        {
            let guard = ChatJobGuard::register(id, base());
            let j = job(id).expect("registered");
            j.set_response(ChatResponse::from_text("x"));
            assert_eq!(guard.take_response().map(|r| r.content), Some(Some("x".into())));
        }
        assert!(job(id).is_none(), "removed on drop");
    }
}
