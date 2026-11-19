//! Anthropic Messages API: `POST /v1/messages` and
//! `POST /v1/messages/count_tokens`.
//!
//! Point an Anthropic client at the orchestrator (`ANTHROPIC_BASE_URL=http://<host>:<port>`
//! for the Claude SDKs and Claude Code) and its requests run through the
//! same routing, pipeline, dedup, circuit breaker and spend cap as
//! `/v1/chat/completions`.
//!
//! The request is translated to a [`ChatRequest`] (OpenAI shape, which every
//! worker understands) and the answer back to Anthropic's shape:
//!
//! - `system` (a string or text blocks) becomes the system turn.
//! - `text` blocks become message text; an assistant's `tool_use` blocks
//!   become `tool_calls`, and a user's `tool_result` blocks become `tool`
//!   messages (an `is_error` result is prefixed with `Error: `).
//! - `tools` (`name`, `description`, `input_schema`) become function tools;
//!   `tool_choice` `auto`, `any`, `tool` and `none` map to `auto`,
//!   `required`, a named function and `none`, and
//!   `disable_parallel_tool_use` to `parallel_tool_calls: false`.
//! - `max_tokens`, `temperature`, `top_p` and `stop_sequences` are passed
//!   through. `thinking`, `top_k`, `cache_control` and `anthropic-beta`
//!   features are accepted and ignored.
//! - The model's tool calls come back as `tool_use` blocks with
//!   `stop_reason: "tool_use"`; when streaming, as `content_block_start` /
//!   `input_json_delta` / `content_block_stop` events.
//!
//! Content is text only: `image` and `document` blocks (also inside a
//! `tool_result`), and Anthropic's server tools (`web_search`, `bash`,
//! `text_editor`, ...), are answered 400.

use super::*;

/// `true` for the Anthropic-compatible paths (auth errors use Anthropic's shape).
pub(in crate::web_api) fn is_anthropic_path(path: &str) -> bool {
    path == "/v1/messages" || path == "/v1/messages/count_tokens"
}

/// The 401 an Anthropic client expects for a missing or wrong key.
pub(in crate::web_api) fn anthropic_unauthorized() -> Response {
    anthropic_error(&ApiError::new(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "invalid_api_key",
        "Missing or incorrect API key. Send the orchestrator's key (ORCHESTRATOR_API_KEY) \
         as 'x-api-key: <key>' or 'Authorization: Bearer <key>'.",
    ))
}

/// Render an error in Anthropic's shape:
/// `{"type": "error", "error": {"type", "message"}}`.
fn anthropic_error(err: &ApiError) -> Response {
    let kind = match err.status.as_u16() {
        400 | 413 | 422 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    };
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": kind, "message": err.message},
    });
    let mut resp = (err.status, Json(body)).into_response();
    err.add_headers(&mut resp);
    resp
}

#[derive(Debug, Deserialize)]
struct MessagesRequest {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<Value>,
    /// A string or an array of text blocks.
    #[serde(default)]
    system: Option<Value>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    metadata: Option<MessagesMetadata>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    tools: Option<Vec<Value>>,
    #[serde(default)]
    tool_choice: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct MessagesMetadata {
    #[serde(default)]
    user_id: Option<String>,
}

fn invalid_messages(message: impl Into<String>) -> ApiError {
    ApiError::invalid(Some("messages"), message)
}

/// Text of a `system` value: a string or an array of text blocks.
fn system_text(system: &Value) -> Result<String, ApiError> {
    match system {
        Value::String(s) => Ok(s.clone()),
        Value::Array(blocks) => {
            let mut out = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => out.push(b.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                    other => {
                        return Err(ApiError::invalid(
                            Some("system"),
                            format!("System block type '{}' is not supported; only text is.", other.unwrap_or("unknown")),
                        ))
                    }
                }
            }
            Ok(out.join("\n"))
        }
        _ => Err(ApiError::invalid(Some("system"), "'system' must be a string or an array of text blocks.")),
    }
}

fn unsupported_block(kind: &str) -> ApiError {
    invalid_messages(format!(
        "Content block type '{kind}' is not supported; only text, tool_use and tool_result are."
    ))
}

/// Text of a `tool_result`'s `content`: a string or text blocks.
fn tool_result_text(block: &Value) -> Result<String, ApiError> {
    let text = match block.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => out.push(p.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                    other => return Err(unsupported_block(other.unwrap_or("unknown"))),
                }
            }
            out.join("\n")
        }
        Some(_) => return Err(invalid_messages("A tool_result's content must be a string or an array of blocks.")),
    };
    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
        return Ok(format!("Error: {text}"));
    }
    Ok(text)
}

/// Translate an Anthropic conversation (system prompt plus messages) to
/// OpenAI Chat Completions messages.
fn to_openai_messages(system: Option<&Value>, messages: &[Value]) -> Result<Vec<Value>, ApiError> {
    if messages.is_empty() {
        return Err(invalid_messages("'messages' must contain at least one message."));
    }
    let mut wire = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = system.filter(|s| !s.is_null()) {
        let text = system_text(system)?;
        if !text.is_empty() {
            wire.push(serde_json::json!({"role": "system", "content": text}));
        }
    }
    for m in messages {
        let role = m
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_messages("Every message needs a string 'role'."))?;
        if role != "user" && role != "assistant" {
            return Err(invalid_messages(format!("Message role must be 'user' or 'assistant', got '{role}'.")));
        }
        let mut texts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_results: Vec<Value> = Vec::new();
        match m.get("content") {
            Some(Value::String(s)) => texts.push(s.clone()),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    let kind = b.get("type").and_then(Value::as_str).unwrap_or("unknown");
                    match (role, kind) {
                        (_, "text") => texts.push(b.get("text").and_then(Value::as_str).unwrap_or("").to_string()),
                        // Extended thinking is not forwarded, so earlier
                        // thinking blocks have nothing to go back to.
                        ("assistant", "thinking" | "redacted_thinking") => {}
                        ("assistant", "tool_use") => {
                            let name = b
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid_messages("A tool_use block needs a 'name'."))?;
                            let id = b
                                .get("id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid_messages("A tool_use block needs an 'id'."))?;
                            let input = b.get("input").cloned().unwrap_or_else(|| serde_json::json!({}));
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": input.to_string()},
                            }));
                        }
                        ("user", "tool_result") => {
                            let id = b
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .ok_or_else(|| invalid_messages("A tool_result block needs a 'tool_use_id'."))?;
                            tool_results.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": tool_result_text(b)?,
                            }));
                        }
                        (_, other) => return Err(unsupported_block(other)),
                    }
                }
            }
            None | Some(Value::Null) => {}
            Some(_) => return Err(invalid_messages("Message content must be a string or an array of content blocks.")),
        }
        let text = texts.join("\n");
        if role == "assistant" {
            if text.is_empty() && tool_calls.is_empty() {
                continue;
            }
            let mut msg = serde_json::json!({
                "role": "assistant",
                "content": if text.is_empty() { Value::Null } else { Value::String(text) },
            });
            if !tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(tool_calls);
            }
            wire.push(msg);
        } else {
            // Tool results answer the previous assistant turn, so they go
            // first, then whatever the user wrote alongside them.
            let had_results = !tool_results.is_empty();
            wire.extend(tool_results);
            if !text.is_empty() || !had_results {
                wire.push(serde_json::json!({"role": "user", "content": text}));
            }
        }
    }
    Ok(wire)
}

/// Translate Anthropic `tools` to OpenAI function tools.
fn to_openai_tools(tools: &[Value]) -> Result<Vec<Value>, ApiError> {
    tools
        .iter()
        .map(|t| {
            if let Some(kind) = t.get("type").and_then(Value::as_str) {
                if kind != "custom" {
                    return Err(ApiError::invalid(
                        Some("tools"),
                        format!(
                            "Tool type '{kind}' is an Anthropic server tool, which the orchestrator \
                             cannot run; only client tools (name, description, input_schema) are supported."
                        ),
                    ));
                }
            }
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid(Some("tools"), "Every tool needs a string 'name'."))?;
            let mut function = serde_json::json!({
                "name": name,
                "parameters": t
                    .get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
            });
            if let Some(d) = t.get("description").filter(|d| d.is_string()) {
                function["description"] = d.clone();
            }
            Ok(serde_json::json!({"type": "function", "function": function}))
        })
        .collect()
}

/// Translate an Anthropic `tool_choice`: the OpenAI `tool_choice` and
/// whether parallel tool calls were turned off.
fn to_openai_tool_choice(choice: &Value) -> Result<(Option<Value>, Option<bool>), ApiError> {
    let bad = || ApiError::invalid(Some("tool_choice"), "'tool_choice' must be an object with type auto, any, tool or none.");
    let kind = choice.get("type").and_then(Value::as_str).ok_or_else(bad)?;
    let mapped = match kind {
        "auto" => Value::from("auto"),
        "any" => Value::from("required"),
        "none" => Value::from("none"),
        "tool" => {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid(Some("tool_choice"), "tool_choice of type 'tool' needs a 'name'."))?;
            serde_json::json!({"type": "function", "function": {"name": name}})
        }
        _ => return Err(bad()),
    };
    let parallel = (choice.get("disable_parallel_tool_use").and_then(Value::as_bool) == Some(true)).then_some(false);
    Ok((Some(mapped), parallel))
}

/// The orchestrator's view of one Messages API request.
struct Translated {
    chat: ChatRequest,
    prompt: String,
    target: Target,
}

fn translate(state: &AppState, req: &MessagesRequest) -> Result<Translated, ApiError> {
    let wire = to_openai_messages(req.system.as_ref(), &req.messages)?;
    if req.temperature.is_some_and(|t| !t.is_finite() || t < 0.0) {
        return Err(ApiError::invalid(Some("temperature"), "'temperature' must be a number of at least 0."));
    }
    if req.top_p.is_some_and(|p| !p.is_finite() || p < 0.0) {
        return Err(ApiError::invalid(Some("top_p"), "'top_p' must be a number of at least 0."));
    }
    let target = resolve_target(state, req.model.as_deref())?;
    let mut chat = ChatRequest::new(target.upstream_model.clone(), wire);
    chat.max_tokens = req.max_tokens;
    chat.temperature = req.temperature;
    chat.top_p = req.top_p;
    chat.stop = req
        .stop_sequences
        .clone()
        .filter(|s| !s.is_empty())
        .map(|s| Value::from(s.into_iter().map(Value::from).collect::<Vec<_>>()));
    if let Some(tools) = req.tools.as_deref().filter(|t| !t.is_empty()) {
        chat.tools = Some(to_openai_tools(tools)?);
        if let Some(choice) = req.tool_choice.as_ref().filter(|c| !c.is_null()) {
            let (mapped, parallel) = to_openai_tool_choice(choice)?;
            chat.tool_choice = mapped;
            chat.parallel_tool_calls = parallel;
        }
    }
    let mut prompt = chat.transcript();
    if prompt.trim().is_empty() {
        if chat.messages.iter().all(|m| m.get("tool_calls").is_none() && m["role"] != "tool") {
            return Err(invalid_messages("All messages are empty."));
        }
        // Only tool traffic: key semantic dedup and token estimates on it.
        prompt = Value::Array(chat.messages.clone()).to_string();
    }
    Ok(Translated { chat, prompt, target })
}

/// Anthropic content blocks for an answer: a text block (when there is
/// text, or nothing else), then one `tool_use` block per tool call.
fn content_blocks(answer: &ChatResponse) -> Vec<Value> {
    let mut blocks = Vec::new();
    if !answer.text().is_empty() || answer.tool_calls.is_empty() {
        blocks.push(serde_json::json!({"type": "text", "text": answer.text()}));
    }
    for call in &answer.tool_calls {
        let function = call.get("function").cloned().unwrap_or(Value::Null);
        let input = function
            .get("arguments")
            .and_then(Value::as_str)
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("toolu_{}", Uuid::new_v4().simple()));
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": function.get("name").cloned().unwrap_or(Value::Null),
            "input": input,
        }));
    }
    blocks
}

/// Anthropic `stop_reason` for an answer.
fn stop_reason(answer: &ChatResponse) -> &'static str {
    if !answer.tool_calls.is_empty() {
        return "tool_use";
    }
    match answer.finish_reason.as_deref() {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("content_filter") => "refusal",
        _ => "end_turn",
    }
}

/// The SSE events of a streamed answer, in Anthropic's order.
fn stream_events(
    id: &str,
    model: &str,
    answer: &ChatResponse,
    input_tokens: usize,
    output_tokens: usize,
) -> Vec<(&'static str, Value)> {
    let mut events: Vec<(&'static str, Value)> = vec![
        (
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                    },
                },
            }),
        ),
        ("ping", serde_json::json!({"type": "ping"})),
    ];
    for (index, block) in content_blocks(answer).into_iter().enumerate() {
        if block["type"] == "tool_use" {
            let input = block["input"].to_string();
            let mut start = block.clone();
            start["input"] = serde_json::json!({});
            events.push((
                "content_block_start",
                serde_json::json!({"type": "content_block_start", "index": index, "content_block": start}),
            ));
            events.push((
                "content_block_delta",
                serde_json::json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": input},
                }),
            ));
        } else {
            events.push((
                "content_block_start",
                serde_json::json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {"type": "text", "text": ""},
                }),
            ));
            for piece in answer.text().split_inclusive(' ') {
                events.push((
                    "content_block_delta",
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {"type": "text_delta", "text": piece},
                    }),
                ));
            }
        }
        events.push((
            "content_block_stop",
            serde_json::json!({"type": "content_block_stop", "index": index}),
        ));
    }
    events.push((
        "message_delta",
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason(answer), "stop_sequence": null},
            "usage": {"output_tokens": output_tokens},
        }),
    ));
    events.push(("message_stop", serde_json::json!({"type": "message_stop"})));
    events
}

/// `POST /v1/messages`: the Anthropic Messages API, answered by the pipeline
/// with the same routing, dedup, circuit breaker and spend cap as the OpenAI
/// endpoint. See the module docs for what is translated.
pub(in crate::web_api) async fn messages_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    match messages(state, &headers, body).await {
        Ok(resp) => resp,
        Err(err) => anthropic_error(&err),
    }
}

async fn messages(
    state: Arc<AppState>,
    headers: &axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    if state.shutting_down.load(AtomicOrdering::Relaxed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "shutting_down",
            "The orchestrator is shutting down.",
        ));
    }
    let req: MessagesRequest = serde_json::from_slice(&body).map_err(|e| {
        ApiError::invalid(None, format!("Could not parse the request body as a Messages API request: {e}"))
    })?;
    let Translated { chat, prompt, target } = translate(&state, &req)?;
    check_capabilities(&target, &chat)?;
    check_spend_cap(&state, "rate_limit_error")?;

    let id = format!("msg_{}", Uuid::new_v4().simple());
    let session = req
        .metadata
        .and_then(|m| m.user_id)
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("anthropic-{id}"));
    debug!(
        request_id = %id,
        requested_model = req.model.as_deref().unwrap_or(""),
        route = %target.label,
        "POST /v1/messages"
    );

    let fresh = wants_fresh(headers);
    let (answer, outcome) =
        complete(&state, &target, &id, session, prompt.clone(), chat, fresh).await?;
    let (input_tokens, output_tokens) = usage_for(&target.reported_model, &prompt, &answer);
    let model = target.reported_model.clone();

    let mut resp = if req.stream.unwrap_or(false) {
        let frames = stream_events(&id, &model, &answer, input_tokens, output_tokens)
            .into_iter()
            .map(|(name, data)| Ok::<_, std::convert::Infallible>(Event::default().event(name).data(data.to_string())));
        Sse::new(stream::iter(frames)).into_response()
    } else {
        Json(serde_json::json!({
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": content_blocks(&answer),
            "stop_reason": stop_reason(&answer),
            "stop_sequence": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            },
        }))
        .into_response()
    };
    add_dedup_headers(&mut resp, outcome);
    Ok(resp)
}

/// `POST /v1/messages/count_tokens`: the input tokens a Messages API
/// request would use, without calling the model. Counted with the model's
/// tokenizer for OpenAI models (feature `tiktoken`), else estimated at about
/// four characters per token; tool definitions are counted as their JSON.
pub(in crate::web_api) async fn count_tokens_handler(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    match count_tokens(&state, &body) {
        Ok(n) => Json(serde_json::json!({"input_tokens": n})).into_response(),
        Err(err) => anthropic_error(&err),
    }
}

fn count_tokens(state: &AppState, body: &[u8]) -> Result<usize, ApiError> {
    let req: MessagesRequest = serde_json::from_slice(body).map_err(|e| {
        ApiError::invalid(None, format!("Could not parse the request body as a count_tokens request: {e}"))
    })?;
    let t = translate(state, &req)?;
    let mut tokens = prompt_tokens(&t.target.reported_model, &t.prompt);
    if let Some(tools) = &t.chat.tools {
        tokens += approx_tokens(&Value::Array(tools.clone()).to_string());
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_use_and_tool_result_become_openai_tool_messages() {
        let wire = to_openai_messages(
            Some(&json!([{"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}])),
            &[
                json!({"role": "user", "content": "Weather in Paris?"}),
                json!({"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "...", "signature": "s"},
                    {"type": "text", "text": "Checking."},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}
                ]}),
                json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "21C"}]},
                    {"type": "text", "text": "Thanks"}
                ]}),
            ],
        )
        .expect("translates");
        assert_eq!(wire[0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(wire[2]["content"], "Checking.");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Paris\"}");
        assert_eq!(wire[3], json!({"role": "tool", "tool_call_id": "toolu_1", "content": "21C"}));
        assert_eq!(wire[4], json!({"role": "user", "content": "Thanks"}));
        assert_eq!(wire.len(), 5);
    }

    #[test]
    fn error_results_are_marked() {
        let wire = to_openai_messages(
            None,
            &[json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "boom", "is_error": true}
            ]})],
        )
        .expect("translates");
        assert_eq!(wire, vec![json!({"role": "tool", "tool_call_id": "t", "content": "Error: boom"})]);
    }

    #[test]
    fn images_and_server_tools_are_rejected() {
        assert!(to_openai_messages(None, &[json!({"role": "user", "content": [{"type": "image", "source": {}}]})]).is_err());
        assert!(to_openai_messages(
            None,
            &[json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": [{"type": "image"}]}]})]
        )
        .is_err());
        assert!(to_openai_tools(&[json!({"type": "web_search_20250305", "name": "web_search"})]).is_err());
        assert!(to_openai_tools(&[json!({"type": "custom", "name": "f", "input_schema": {"type": "object"}})]).is_ok());
    }

    #[test]
    fn tool_choice_maps_to_openai() {
        let (c, p) = to_openai_tool_choice(&json!({"type": "any", "disable_parallel_tool_use": true})).expect("maps");
        assert_eq!(c, Some(json!("required")));
        assert_eq!(p, Some(false));
        let (c, p) = to_openai_tool_choice(&json!({"type": "tool", "name": "f"})).expect("maps");
        assert_eq!(c, Some(json!({"type": "function", "function": {"name": "f"}})));
        assert_eq!(p, None);
        assert!(to_openai_tool_choice(&json!({"type": "sometimes"})).is_err());
    }

    #[test]
    fn answers_with_tool_calls_become_tool_use_blocks() {
        let mut answer = ChatResponse::from_text("");
        answer.content = None;
        answer.tool_calls = vec![json!({"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}})];
        answer.finish_reason = Some("tool_calls".into());
        let blocks = content_blocks(&answer);
        assert_eq!(blocks, vec![json!({"type": "tool_use", "id": "call_1", "name": "f", "input": {"a": 1}})]);
        assert_eq!(stop_reason(&answer), "tool_use");
        let events = stream_events("msg_1", "m", &answer, 3, 4);
        let delta = events.iter().find(|(_, v)| v["delta"]["type"] == "input_json_delta").expect("json delta");
        assert_eq!(delta.1["delta"]["partial_json"], "{\"a\":1}");
        assert_eq!(events.last().map(|e| e.0), Some("message_stop"));
    }
}
