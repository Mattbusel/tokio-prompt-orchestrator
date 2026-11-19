//! OpenAI Responses API: `POST /v1/responses`, JSON or SSE.
//!
//! Clients built on the Responses API (the OpenAI SDKs' `responses.create`,
//! the Agents SDK, Codex-style tools) can point their base URL at the
//! orchestrator. Each request becomes a [`ChatRequest`] and runs through the
//! same routing, pipeline, dedup, circuit breaker and spend cap as
//! `/v1/chat/completions`:
//!
//! - `instructions` becomes the system turn; `input` is a string or a list
//!   of items: messages (`input_text` / `output_text` parts),
//!   `function_call` items (an assistant's tool calls) and
//!   `function_call_output` items (tool results). `reasoning` items are
//!   skipped.
//! - `tools` of type `function`, `tool_choice` (`auto`, `none`,
//!   `required`, or a named function), `parallel_tool_calls`,
//!   `temperature`, `top_p`, `max_output_tokens` and `text.format`
//!   (`text`, `json_object`, `json_schema`) are passed through.
//! - The answer comes back as a `response` object with a `message` output
//!   item and one `function_call` item per tool call; `status` is
//!   `incomplete` (`max_output_tokens`) when the reply was cut off.
//! - `"stream": true` returns the Responses event sequence
//!   (`response.created`, `response.output_item.added`,
//!   `response.output_text.delta`, `response.function_call_arguments.delta`,
//!   ..., `response.completed`), each with a `sequence_number`.
//!
//! The orchestrator keeps no conversation state: `previous_response_id`,
//! `conversation` and `background` are answered 400 (send the earlier
//! turns in `input` instead), and `store` is reported as `false`. Built-in
//! tools (`web_search`, `file_search`, `computer_use`, ...) and image or
//! file inputs are answered 400.

use super::*;

#[derive(Debug, Deserialize)]
struct ResponsesRequest {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    instructions: Option<Value>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    tools: Option<Vec<Value>>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    text: Option<Value>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    previous_response_id: Option<Value>,
    #[serde(default)]
    conversation: Option<Value>,
    #[serde(default)]
    background: Option<bool>,
}

fn invalid_input(message: impl Into<String>) -> ApiError {
    ApiError::invalid(Some("input"), message)
}

/// Text of a message item's `content`: a string or text parts.
fn content_text(content: Option<&Value>) -> Result<String, ApiError> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(parts)) => {
            let mut out = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text" | "text") => {
                        out.push(p.get("text").and_then(Value::as_str).unwrap_or("").to_string());
                    }
                    Some("refusal") => out.push(p.get("refusal").and_then(Value::as_str).unwrap_or("").to_string()),
                    other => {
                        return Err(invalid_input(format!(
                            "Content part type '{}' is not supported; only text is.",
                            other.unwrap_or("unknown")
                        )))
                    }
                }
            }
            Ok(out.join("\n"))
        }
        Some(_) => Err(invalid_input("Message content must be a string or an array of content parts.")),
    }
}

/// Append a tool call to the assistant turn that ends `wire`, or start one.
fn push_tool_call(wire: &mut Vec<Value>, call: Value) {
    if let Some(last) = wire.last_mut() {
        if last["role"] == "assistant" {
            match last.get_mut("tool_calls") {
                Some(Value::Array(calls)) => calls.push(call),
                _ => last["tool_calls"] = Value::Array(vec![call]),
            }
            return;
        }
    }
    wire.push(serde_json::json!({"role": "assistant", "content": null, "tool_calls": [call]}));
}

/// Translate `instructions` and `input` to OpenAI chat messages.
fn to_openai_messages(instructions: Option<&Value>, input: &Value) -> Result<Vec<Value>, ApiError> {
    let mut wire = Vec::new();
    match instructions {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.is_empty() => {}
        Some(Value::String(s)) => wire.push(serde_json::json!({"role": "system", "content": s})),
        Some(_) => return Err(ApiError::invalid(Some("instructions"), "'instructions' must be a string.")),
    }
    let items = match input {
        Value::String(s) => {
            if s.trim().is_empty() {
                return Err(invalid_input("'input' is empty."));
            }
            wire.push(serde_json::json!({"role": "user", "content": s}));
            return Ok(wire);
        }
        Value::Array(items) if !items.is_empty() => items,
        Value::Array(_) => return Err(invalid_input("'input' must contain at least one item.")),
        _ => return Err(invalid_input("'input' must be a string or an array of input items.")),
    };
    for item in items {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(if item.get("role").is_some() { "message" } else { "unknown" });
        match kind {
            "message" => {
                let role = item
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("Every message item needs a string 'role'."))?;
                let role = match role {
                    "user" | "assistant" | "system" => role,
                    "developer" => "system",
                    other => return Err(invalid_input(format!("Unknown message role '{other}'."))),
                };
                let text = content_text(item.get("content"))?;
                wire.push(serde_json::json!({"role": role, "content": text}));
            }
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("A function_call item needs a 'call_id'."))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("A function_call item needs a 'name'."))?;
                let arguments = match item.get("arguments") {
                    Some(Value::String(a)) => a.clone(),
                    None | Some(Value::Null) => "{}".to_string(),
                    Some(other) => other.to_string(),
                };
                push_tool_call(
                    &mut wire,
                    serde_json::json!({
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }),
                );
            }
            "function_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_input("A function_call_output item needs a 'call_id'."))?;
                let output = match item.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    other => content_text(other)?,
                };
                wire.push(serde_json::json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
            // Reasoning is not forwarded upstream, so there is nothing to
            // return it to.
            "reasoning" => {}
            "item_reference" => {
                return Err(invalid_input(
                    "item_reference needs stored responses, which this orchestrator does not keep. \
                     Send the referenced items themselves.",
                ))
            }
            other => {
                return Err(invalid_input(format!(
                    "Input item type '{other}' is not supported; use message, function_call and \
                     function_call_output items."
                )))
            }
        }
    }
    if wire.iter().all(|m| m["role"] == "system") {
        return Err(invalid_input("'input' has no user, assistant or tool turn."));
    }
    Ok(wire)
}

/// Translate Responses `tools` to Chat Completions function tools.
fn to_openai_tools(tools: &[Value]) -> Result<Vec<Value>, ApiError> {
    tools
        .iter()
        .map(|t| {
            let kind = t.get("type").and_then(Value::as_str).unwrap_or("function");
            if kind != "function" {
                return Err(ApiError::invalid(
                    Some("tools"),
                    format!(
                        "Built-in tool '{kind}' is not supported by the orchestrator; only function tools are."
                    ),
                ));
            }
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid(Some("tools"), "Every function tool needs a string 'name'."))?;
            let mut function = serde_json::json!({"name": name});
            for key in ["description", "parameters", "strict"] {
                if let Some(v) = t.get(key).filter(|v| !v.is_null()) {
                    function[key] = v.clone();
                }
            }
            Ok(serde_json::json!({"type": "function", "function": function}))
        })
        .collect()
}

/// Translate a Responses `tool_choice`.
fn to_openai_tool_choice(choice: &Value) -> Result<Value, ApiError> {
    match choice {
        Value::String(s) if matches!(s.as_str(), "auto" | "none" | "required") => Ok(choice.clone()),
        Value::Object(_) if choice.get("type").and_then(Value::as_str) == Some("function") => {
            let name = choice
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::invalid(Some("tool_choice"), "A function tool_choice needs a 'name'."))?;
            Ok(serde_json::json!({"type": "function", "function": {"name": name}}))
        }
        _ => Err(ApiError::invalid(
            Some("tool_choice"),
            "'tool_choice' must be auto, none, required or {\"type\": \"function\", \"name\": ...}.",
        )),
    }
}

/// Translate `text.format` to a Chat Completions `response_format`.
fn to_response_format(text: &Value) -> Result<Option<Value>, ApiError> {
    let Some(format) = text.get("format").filter(|f| !f.is_null()) else {
        return Ok(None);
    };
    let bad = |m: &str| ApiError::invalid(Some("text"), m.to_string());
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(None),
        Some("json_object") => Ok(Some(serde_json::json!({"type": "json_object"}))),
        Some("json_schema") => {
            let mut schema = serde_json::json!({
                "name": format.get("name").cloned().unwrap_or_else(|| Value::from("response")),
                "schema": format.get("schema").cloned().ok_or_else(|| bad("A json_schema format needs a 'schema'."))?,
            });
            for key in ["strict", "description"] {
                if let Some(v) = format.get(key).filter(|v| !v.is_null()) {
                    schema[key] = v.clone();
                }
            }
            Ok(Some(serde_json::json!({"type": "json_schema", "json_schema": schema})))
        }
        _ => Err(bad("'text.format.type' must be text, json_object or json_schema.")),
    }
}

/// The output items of an answer: a `message` (when there is text, or
/// nothing else) and one `function_call` per tool call.
fn output_items(answer: &ChatResponse) -> Vec<Value> {
    let mut items = Vec::new();
    if !answer.text().is_empty() || answer.tool_calls.is_empty() {
        items.push(serde_json::json!({
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v4().simple()),
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": answer.text(), "annotations": []}],
        }));
    }
    for call in &answer.tool_calls {
        let function = call.get("function").cloned().unwrap_or(Value::Null);
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{}", Uuid::new_v4().simple()));
        items.push(serde_json::json!({
            "type": "function_call",
            "id": format!("fc_{}", Uuid::new_v4().simple()),
            "call_id": call_id,
            "name": function.get("name").cloned().unwrap_or(Value::Null),
            "arguments": function.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
            "status": "completed",
        }));
    }
    items
}

/// `status` and `incomplete_details` for an answer.
fn completion_status(answer: &ChatResponse) -> (&'static str, Value) {
    match answer.finish_reason.as_deref() {
        Some("length") => ("incomplete", serde_json::json!({"reason": "max_output_tokens"})),
        Some("content_filter") => ("incomplete", serde_json::json!({"reason": "content_filter"})),
        _ => ("completed", Value::Null),
    }
}

/// The parts of the `response` object that echo the request.
struct Echo {
    id: String,
    created_at: u64,
    model: String,
    req: ResponsesRequest,
}

impl Echo {
    fn response(&self, status: &str, output: Vec<Value>, usage: Value, incomplete: Value) -> Value {
        let req = &self.req;
        serde_json::json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "error": null,
            "incomplete_details": incomplete,
            "instructions": req.instructions,
            "max_output_tokens": req.max_output_tokens,
            "model": self.model,
            "output": output,
            "parallel_tool_calls": req.parallel_tool_calls.unwrap_or(true),
            "previous_response_id": null,
            "reasoning": {"effort": null, "summary": null},
            "store": false,
            "temperature": req.temperature,
            "text": req.text.clone().unwrap_or_else(|| serde_json::json!({"format": {"type": "text"}})),
            "tool_choice": req.tool_choice.clone().unwrap_or_else(|| Value::from("auto")),
            "tools": req.tools.clone().unwrap_or_default(),
            "top_p": req.top_p,
            "truncation": "disabled",
            "usage": usage,
            "user": req.user,
            "metadata": req.metadata.clone().unwrap_or_else(|| serde_json::json!({})),
        })
    }
}

/// The SSE events of a streamed response, in the Responses API's order.
fn stream_events(echo: &Echo, answer: &ChatResponse, usage: Value) -> Vec<(String, Value)> {
    let mut events: Vec<(String, Value)> = Vec::new();
    let mut push = |kind: &str, mut data: Value| {
        data["type"] = Value::from(kind);
        data["sequence_number"] = Value::from(events.len());
        events.push((kind.to_string(), data));
    };
    let pending = echo.response("in_progress", Vec::new(), Value::Null, Value::Null);
    push("response.created", serde_json::json!({"response": pending}));
    push("response.in_progress", serde_json::json!({"response": pending}));

    let items = output_items(answer);
    for (output_index, item) in items.iter().enumerate() {
        let item_id = item["id"].clone();
        if item["type"] == "message" {
            let text = answer.text();
            let mut started = item.clone();
            started["status"] = Value::from("in_progress");
            started["content"] = serde_json::json!([]);
            push("response.output_item.added", serde_json::json!({"output_index": output_index, "item": started}));
            push(
                "response.content_part.added",
                serde_json::json!({
                    "item_id": item_id, "output_index": output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            );
            for piece in text.split_inclusive(' ') {
                push(
                    "response.output_text.delta",
                    serde_json::json!({
                        "item_id": item_id, "output_index": output_index, "content_index": 0,
                        "delta": piece, "logprobs": [],
                    }),
                );
            }
            push(
                "response.output_text.done",
                serde_json::json!({
                    "item_id": item_id, "output_index": output_index, "content_index": 0,
                    "text": text, "logprobs": [],
                }),
            );
            push(
                "response.content_part.done",
                serde_json::json!({
                    "item_id": item_id, "output_index": output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []},
                }),
            );
        } else {
            let arguments = item["arguments"].clone();
            let mut started = item.clone();
            started["status"] = Value::from("in_progress");
            started["arguments"] = Value::from("");
            push("response.output_item.added", serde_json::json!({"output_index": output_index, "item": started}));
            push(
                "response.function_call_arguments.delta",
                serde_json::json!({"item_id": item_id, "output_index": output_index, "delta": arguments}),
            );
            push(
                "response.function_call_arguments.done",
                serde_json::json!({
                    "item_id": item_id, "output_index": output_index,
                    "name": item["name"], "arguments": arguments,
                }),
            );
        }
        push("response.output_item.done", serde_json::json!({"output_index": output_index, "item": item}));
    }
    let (status, incomplete) = completion_status(answer);
    let done = echo.response(status, items, usage, incomplete);
    let final_kind = if status == "completed" { "response.completed" } else { "response.incomplete" };
    push(final_kind, serde_json::json!({"response": done}));
    events
}

/// `POST /v1/responses`: the OpenAI Responses API, answered by the pipeline
/// with the same routing, dedup, circuit breaker and spend cap as
/// `/v1/chat/completions`. See the module docs for what is translated.
pub(in crate::web_api) async fn responses_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    match responses(state, &headers, body).await {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

async fn responses(
    state: Arc<AppState>,
    headers: &axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    if state.shutting_down.load(AtomicOrdering::Relaxed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "shutting_down",
            "The orchestrator is shutting down.",
        ));
    }
    let req: ResponsesRequest = serde_json::from_slice(&body).map_err(|e| {
        ApiError::invalid(None, format!("Could not parse the request body as a Responses API request: {e}"))
    })?;
    let stateless = |param: &'static str| {
        ApiError::invalid(
            Some(param),
            format!(
                "The orchestrator does not store responses, so '{param}' cannot be used. \
                 Send the earlier turns in 'input' instead."
            ),
        )
    };
    if req.previous_response_id.as_ref().is_some_and(|v| !v.is_null()) {
        return Err(stateless("previous_response_id"));
    }
    if req.conversation.as_ref().is_some_and(|v| !v.is_null()) {
        return Err(stateless("conversation"));
    }
    if req.background == Some(true) {
        return Err(ApiError::invalid(Some("background"), "Background responses are not supported."));
    }
    if req.temperature.is_some_and(|t| !t.is_finite() || t < 0.0) {
        return Err(ApiError::invalid(Some("temperature"), "'temperature' must be a number of at least 0."));
    }
    if req.top_p.is_some_and(|p| !p.is_finite() || p < 0.0) {
        return Err(ApiError::invalid(Some("top_p"), "'top_p' must be a number of at least 0."));
    }
    let input = req.input.as_ref().ok_or_else(|| invalid_input("'input' is required."))?;
    let wire = to_openai_messages(req.instructions.as_ref(), input)?;
    let target = resolve_target(&state, req.model.as_deref())?;

    let mut chat = ChatRequest::new(target.upstream_model.clone(), wire);
    chat.temperature = req.temperature;
    chat.top_p = req.top_p;
    // OpenAI's newer models take only max_completion_tokens; other
    // OpenAI-compatible servers and llama.cpp know max_tokens.
    if target.provider == "openai" {
        chat.max_completion_tokens = req.max_output_tokens;
    } else {
        chat.max_tokens = req.max_output_tokens;
    }
    chat.response_format = match &req.text {
        Some(text) => to_response_format(text)?,
        None => None,
    };
    if let Some(tools) = req.tools.as_deref().filter(|t| !t.is_empty()) {
        chat.tools = Some(to_openai_tools(tools)?);
        if let Some(choice) = req.tool_choice.as_ref().filter(|c| !c.is_null()) {
            chat.tool_choice = Some(to_openai_tool_choice(choice)?);
        }
        chat.parallel_tool_calls = req.parallel_tool_calls;
    }
    check_capabilities(&target, &chat)?;
    check_spend_cap(&state, "insufficient_quota")?;

    let mut prompt = chat.transcript();
    if prompt.trim().is_empty() {
        prompt = Value::Array(chat.messages.clone()).to_string();
    }
    let id = format!("resp_{}", Uuid::new_v4().simple());
    let session = req
        .user
        .clone()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("responses-{id}"));
    debug!(
        request_id = %id,
        requested_model = req.model.as_deref().unwrap_or(""),
        route = %target.label,
        "POST /v1/responses"
    );

    let fresh = wants_fresh(headers);
    let (answer, outcome) =
        complete(&state, &target, &id, session, prompt.clone(), chat, fresh).await?;
    let (input_tokens, output_tokens) = usage_for(&target.reported_model, &prompt, &answer);
    let usage = serde_json::json!({
        "input_tokens": input_tokens,
        "input_tokens_details": {"cached_tokens": 0},
        "output_tokens": output_tokens,
        "output_tokens_details": {"reasoning_tokens": 0},
        "total_tokens": input_tokens + output_tokens,
    });
    let stream_requested = req.stream.unwrap_or(false);
    let echo = Echo {
        id,
        created_at: unix_now(),
        model: target.reported_model.clone(),
        req,
    };

    let mut resp = if stream_requested {
        let frames = stream_events(&echo, &answer, usage)
            .into_iter()
            .map(|(name, data)| Ok::<_, std::convert::Infallible>(Event::default().event(name).data(data.to_string())));
        Sse::new(stream::iter(frames)).into_response()
    } else {
        let (status, incomplete) = completion_status(&answer);
        Json(echo.response(status, output_items(&answer), usage, incomplete)).into_response()
    };
    add_dedup_headers(&mut resp, outcome);
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_items_become_chat_messages() {
        let wire = to_openai_messages(
            Some(&json!("Be brief.")),
            &json!([
                {"role": "user", "content": "Weather in Paris?"},
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Checking."}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "21C"},
                {"role": "developer", "content": [{"type": "input_text", "text": "Use Celsius."}]}
            ]),
        )
        .expect("translates");
        assert_eq!(wire[0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(wire[1], json!({"role": "user", "content": "Weather in Paris?"}));
        assert_eq!(wire[2]["content"], "Checking.");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(wire[3], json!({"role": "tool", "tool_call_id": "call_1", "content": "21C"}));
        assert_eq!(wire[4], json!({"role": "system", "content": "Use Celsius."}));
    }

    #[test]
    fn unsupported_inputs_and_tools_are_rejected() {
        assert!(to_openai_messages(None, &json!([{"role": "user", "content": [{"type": "input_image", "image_url": "x"}]}])).is_err());
        assert!(to_openai_messages(None, &json!([{"type": "item_reference", "id": "x"}])).is_err());
        assert!(to_openai_messages(None, &json!([])).is_err());
        assert!(to_openai_tools(&[json!({"type": "web_search"})]).is_err());
        assert!(to_openai_tool_choice(&json!({"type": "file_search"})).is_err());
    }

    #[test]
    fn text_format_maps_to_response_format() {
        assert_eq!(to_response_format(&json!({"format": {"type": "text"}})).expect("ok"), None);
        let f = to_response_format(&json!({"format": {"type": "json_schema", "name": "w", "schema": {"type": "object"}, "strict": true}}))
            .expect("ok")
            .expect("some");
        assert_eq!(f, json!({"type": "json_schema", "json_schema": {"name": "w", "schema": {"type": "object"}, "strict": true}}));
        assert!(to_response_format(&json!({"format": {"type": "yaml"}})).is_err());
    }

    #[test]
    fn stream_events_are_numbered_and_end_with_completed() {
        let echo = Echo {
            id: "resp_1".into(),
            created_at: 0,
            model: "m".into(),
            req: serde_json::from_value(json!({"input": "hi"})).expect("parses"),
        };
        let mut answer = ChatResponse::from_text("a b");
        answer.tool_calls = vec![json!({"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{}"}})];
        let events = stream_events(&echo, &answer, json!({}));
        for (i, (_, data)) in events.iter().enumerate() {
            assert_eq!(data["sequence_number"], i);
        }
        let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds.first(), Some(&"response.created"));
        assert_eq!(kinds.last(), Some(&"response.completed"));
        assert_eq!(kinds.iter().filter(|k| **k == "response.output_text.delta").count(), 2);
        assert!(kinds.contains(&"response.function_call_arguments.done"));
        let done = &events.last().expect("last").1["response"];
        assert_eq!(done["output"][1]["call_id"], "call_1");
        assert_eq!(done["status"], "completed");
    }
}
