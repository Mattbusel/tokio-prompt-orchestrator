//! What your LLM logs say you paid for twice.
//!
//! You pay for the same prompt twice. Identical requests go out while the
//! first is still in flight, the same request is sent again a minute after
//! it was answered, a 400 is retried as if the next attempt could pass, and
//! retries during an outage are capped by count while each one bills a full
//! context. None of it shows on a dashboard that sums tokens per day.
//!
//! [`analyze`] replays a request log and prices each of those:
//!
//! | Category | What it is | Why it is waste |
//! |---|---|---|
//! | `in_flight_duplicate` | an identical request started before the first one finished | one call could have served both |
//! | `repeat_within_ttl` | an identical request within the TTL (default 5 min) after a success | the answer was already paid for |
//! | `retry_after_4xx` | the identical request sent again after a non-retryable 4xx | a 400/401/403/404/422 fails the same way again |
//! | `retry` | the identical request sent again after a 408/409/429/5xx/529 | legitimate, but capped by count it has no ceiling in dollars; the report shows what a dollar budget would have refused |
//! | `agent_repeat` | an agent step that repeated the previous action and got the identical observation back | a full-context call that learned nothing |
//!
//! Records are JSON objects, one per line (JSONL) or a JSON array. Field
//! names from common gateways and SDK logs are recognised (OpenAI-style
//! request/response logs, LiteLLM spend logs, Helicone and Langfuse
//! exports): see [`Call::from_value`]. Prices come from the log's own cost
//! field, else from [`llm_prices`] list prices for the model, else the call
//! is counted but left unpriced.
//!
//! [`audit_source`] is the static half: it reads retry code and flags
//! retries with no dollar ceiling, retries on every exception (4xx
//! included) and retry loops that never stop.
//!
//! The `burn` binary (feature `cli`) runs both from the command line.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Options for [`analyze`].
#[derive(Debug, Clone)]
pub struct BurnOptions {
    /// Seconds after a successful answer within which an identical request
    /// counts as a repeat. Default 300.
    pub ttl_secs: f64,
    /// Compare requests per `user` / `session` field instead of across the
    /// whole log. A shared proxy cache (one API key) sees the whole log; a
    /// per-user cache sees one user's requests.
    pub per_user: bool,
    /// Dollar floor of the simulated retry budget per hour. Default 0.50.
    pub retry_budget_usd: f64,
    /// Share of real spend added to the retry budget. Default 0.10.
    pub retry_ratio: f64,
    /// How many of the most repeated requests to list. Default 10.
    pub top: usize,
    /// Prices (USD per 1M input and output tokens) that win over the list
    /// price, by model id: for old snapshots, private deployments, or the
    /// price you actually pay.
    pub prices: HashMap<String, (f64, f64)>,
}

impl BurnOptions {
    /// The cost of `c`: the log's own cost, else a price from
    /// [`prices`](Self::prices), else the list price ([`Call::cost`]).
    pub fn cost_of(&self, c: &Call) -> Option<f64> {
        if c.logged_cost.is_some() {
            return c.logged_cost;
        }
        if let Some((i, o)) = self.prices.get(&c.model) {
            return Some((c.input_tokens as f64 * i + c.output_tokens as f64 * o) / 1_000_000.0);
        }
        c.cost()
    }
}

impl Default for BurnOptions {
    fn default() -> Self {
        Self {
            ttl_secs: 300.0,
            per_user: false,
            retry_budget_usd: 0.50,
            retry_ratio: llm_resilience::DEFAULT_RETRY_RATIO,
            top: 10,
            prices: HashMap::new(),
        }
    }
}

/// One model call read from a log record.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// Start time, Unix seconds.
    pub start: f64,
    /// End time, Unix seconds (`start` when the log has no end or latency).
    pub end: f64,
    /// Identity of the request: a SHA-256 of its canonical body, or the
    /// log's own request hash / cache key.
    pub key: String,
    /// User or session, when the log has one.
    pub user: Option<String>,
    /// Model id.
    pub model: String,
    /// HTTP status, `None` for a success without one. A failure without a
    /// status is `Some(0)`.
    pub status: Option<u16>,
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Token counts were estimated from text length (4 characters a token).
    pub estimated_tokens: bool,
    /// Cost from the log itself.
    pub logged_cost: Option<f64>,
    /// Agent trace id, for `agent_repeat`.
    pub trace: Option<String>,
    /// The agent's action at this step.
    pub action: Option<String>,
    /// What the action returned.
    pub observation: Option<String>,
    /// Start of the last user message, for the report.
    pub preview: String,
}

/// First value found at any of these dotted paths.
fn pick<'a>(v: &'a Value, paths: &[&str]) -> Option<&'a Value> {
    paths.iter().find_map(|p| {
        let mut cur = v;
        for part in p.split('.') {
            cur = cur.get(part)?;
        }
        if cur.is_null() {
            None
        } else {
            Some(cur)
        }
    })
}

fn pick_str(v: &Value, paths: &[&str]) -> Option<String> {
    pick(v, paths).and_then(|x| match x {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn pick_f64(v: &Value, paths: &[&str]) -> Option<f64> {
    pick(v, paths).and_then(|x| match x {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

/// Unix seconds from a number (seconds, or milliseconds when over 1e11) or
/// an RFC 3339 / `YYYY-MM-DD HH:MM:SS` string (UTC when no offset).
fn parse_time(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => {
            let x = n.as_f64()?;
            Some(if x > 1e11 { x / 1000.0 } else { x })
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(x) = s.parse::<f64>() {
                return Some(if x > 1e11 { x / 1000.0 } else { x });
            }
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(t.timestamp_micros() as f64 / 1e6);
            }
            for fmt in [
                "%Y-%m-%d %H:%M:%S%.f",
                "%Y-%m-%dT%H:%M:%S%.f",
                "%Y-%m-%d %H:%M:%S%.f%#z",
            ] {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    return Some(t.and_utc().timestamp_micros() as f64 / 1e6);
                }
            }
            None
        }
        _ => None,
    }
}

/// Characters of every string inside `v`.
fn text_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.iter().map(text_len).sum(),
        Value::Object(m) => m.values().map(text_len).sum(),
        _ => 0,
    }
}

/// Fields of a request body that change on every call without changing the
/// answer, left out of the identity.
const VOLATILE: &[&str] = &[
    "user",
    "metadata",
    "stream",
    "stream_options",
    "request_id",
    "id",
    "store",
];

/// The request body of a record, as the identity is computed from it.
fn request_body(v: &Value) -> Option<Value> {
    if let Some(b) = pick(v, &["request", "request_body", "body", "request.body"]) {
        let b = match b {
            Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| b.clone()),
            other => other.clone(),
        };
        if let Value::Object(mut m) = b {
            for k in VOLATILE {
                m.remove(*k);
            }
            return Some(Value::Object(m));
        }
        return Some(b);
    }
    let mut m = serde_json::Map::new();
    for k in [
        "model",
        "messages",
        "prompt",
        "input",
        "contents",
        "system",
        "temperature",
        "top_p",
        "max_tokens",
        "max_completion_tokens",
        "tools",
        "tool_choice",
        "seed",
        "response_format",
        "stop",
    ] {
        if let Some(x) = v.get(k).filter(|x| !x.is_null()) {
            m.insert(k.to_string(), x.clone());
        }
    }
    if m.keys()
        .any(|k| matches!(k.as_str(), "messages" | "prompt" | "input" | "contents"))
    {
        Some(Value::Object(m))
    } else {
        None
    }
}

/// Start of the last user message (or the prompt), for the report.
fn preview_of(body: &Value) -> String {
    let text = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| {
            msgs.iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .or_else(|| body.get("prompt"))
        .or_else(|| body.get("input"))
        .map(|c| match c {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(80).collect()
}

fn hash(v: &Value) -> String {
    // serde_json's map is ordered by key, so this is canonical.
    let mut h = Sha256::new();
    h.update(v.to_string().as_bytes());
    hex::encode(&h.finalize()[..12])
}

impl Call {
    /// Read one call from a log record, or `None` when the record has no
    /// time or no request identity.
    ///
    /// Recognised fields (first match wins, dotted paths are nested):
    ///
    /// - start: `start_time`, `startTime`, `timestamp`, `ts`, `time`,
    ///   `created_at`, `created`, `request_time`
    /// - end: `end_time`, `endTime`, `completed_at`, `completionStartTime`;
    ///   or start plus `latency_ms` / `duration_ms` (milliseconds) or
    ///   `latency` / `duration` (seconds)
    /// - identity: `request_hash`, `cache_key`, `prompt_hash`; else the
    ///   request body (`request`, `request_body`, `body`, or the record's
    ///   own `model` + `messages` / `prompt` / `input` / `contents` and
    ///   sampling settings), without `user`, `metadata` and `stream`
    /// - status: `status`, `status_code`, `http_status`, `response.status`
    ///   (a number, or `success` / `failure` / `error`), or an `error`
    ///   field
    /// - tokens: `usage.prompt_tokens`, `usage.input_tokens`,
    ///   `prompt_tokens`, `input_tokens` and the output equivalents
    ///   (`response.usage.*` too); else estimated from the text
    /// - cost: `cost_usd`, `cost`, `spend`, `response_cost`, `total_cost`
    /// - user: `user`, `user_id`, `session_id`, `end_user`, `request.user`
    /// - agent steps: `trace_id` with `action` and `observation`
    /// - `estimated_tokens: true` marks token counts as estimates, and
    ///   `preview` gives the report a text when there is no body
    pub fn from_value(v: &Value) -> Option<Call> {
        let start = pick(
            v,
            &[
                "start_time",
                "startTime",
                "timestamp",
                "ts",
                "time",
                "created_at",
                "created",
                "request_time",
            ],
        )
        .and_then(parse_time)?;
        let end = pick(
            v,
            &["end_time", "endTime", "completed_at", "completionStartTime"],
        )
        .and_then(parse_time)
        .or_else(|| pick_f64(v, &["latency_ms", "duration_ms"]).map(|ms| start + ms / 1000.0))
        .or_else(|| pick_f64(v, &["latency", "duration", "response_time"]).map(|s| start + s))
        .unwrap_or(start)
        .max(start);

        let body = request_body(v);
        let key = pick_str(v, &["request_hash", "cache_key", "prompt_hash"])
            .or_else(|| body.as_ref().map(hash))?;
        let model = pick_str(
            v,
            &[
                "model",
                "request.model",
                "body.model",
                "model_id",
                "modelId",
                "response.model",
            ],
        )
        .unwrap_or_default();

        let status = match pick(
            v,
            &[
                "status_code",
                "http_status",
                "status",
                "response.status",
                "response.status_code",
            ],
        ) {
            Some(Value::Number(n)) => {
                let s = n.as_u64().and_then(|s| u16::try_from(s).ok()).unwrap_or(0);
                if (200..300).contains(&s) {
                    None
                } else {
                    Some(s)
                }
            }
            Some(Value::String(s)) => match s.to_ascii_lowercase().as_str() {
                "success" | "ok" | "succeeded" | "completed" => None,
                other => match other.parse::<u16>() {
                    Ok(code) if (200..300).contains(&code) => None,
                    Ok(code) => Some(code),
                    Err(_) => Some(0),
                },
            },
            _ if pick(v, &["error"]).is_some_and(|e| e != &Value::Bool(false)) => Some(0),
            _ => None,
        };

        let input = pick_f64(
            v,
            &[
                "usage.prompt_tokens",
                "usage.input_tokens",
                "prompt_tokens",
                "input_tokens",
                "response.usage.prompt_tokens",
                "response.usage.input_tokens",
                "tokens_in",
            ],
        );
        let output = pick_f64(
            v,
            &[
                "usage.completion_tokens",
                "usage.output_tokens",
                "completion_tokens",
                "output_tokens",
                "response.usage.completion_tokens",
                "response.usage.output_tokens",
                "tokens_out",
            ],
        );
        let estimated = input.is_none() || v.get("estimated_tokens") == Some(&Value::Bool(true));
        let input_tokens = input.map_or_else(
            || body.as_ref().map_or(0, |b| text_len(b) / 4) as u64,
            |x| x.max(0.0) as u64,
        );
        let output_tokens = output.map_or_else(
            || {
                pick(v, &["response", "response_text", "output", "completion"])
                    .map_or(0, |r| text_len(r) / 4) as u64
            },
            |x| x.max(0.0) as u64,
        );
        let logged_cost = pick_f64(
            v,
            &[
                "cost_usd",
                "cost",
                "spend",
                "response_cost",
                "total_cost",
                "usage.cost",
            ],
        );

        Some(Call {
            start,
            end,
            key,
            user: pick_str(
                v,
                &["user", "user_id", "session_id", "end_user", "request.user"],
            ),
            model,
            status,
            input_tokens,
            output_tokens,
            estimated_tokens: estimated,
            logged_cost,
            trace: pick_str(v, &["trace_id", "traceId"]),
            action: pick_str(v, &["action"]),
            observation: pick_str(v, &["observation"]),
            preview: body
                .as_ref()
                .map(preview_of)
                .filter(|p| !p.is_empty())
                .or_else(|| {
                    pick_str(v, &["preview"]).map(|p| {
                        p.split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .chars()
                            .take(80)
                            .collect()
                    })
                })
                .unwrap_or_default(),
        })
    }

    /// The call's cost: the logged cost, else the model's list price
    /// ([`llm_prices`]; an old `-0301`-style snapshot id falls back to its
    /// base model's current row).
    pub fn cost(&self) -> Option<f64> {
        self.logged_cost.or_else(|| {
            let price = llm_prices::lookup(&self.model).or_else(|| {
                let (base, tail) = self.model.rsplit_once('-')?;
                if tail.len() == 4 && tail.bytes().all(|b| b.is_ascii_digit()) {
                    llm_prices::lookup(base)
                } else {
                    None
                }
            })?;
            price.cost(self.input_tokens, self.output_tokens)
        })
    }

    fn ok(&self) -> bool {
        self.status.is_none()
    }
}

/// Count and dollars of one category.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Bucket {
    /// Calls in the category.
    pub calls: u64,
    /// Their cost in USD (priced calls only).
    pub usd: f64,
}

impl Bucket {
    fn add(&mut self, cost: Option<f64>) {
        self.calls += 1;
        self.usd += cost.unwrap_or(0.0);
    }
}

/// One of the most repeated requests.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Repeated {
    /// Request identity (hash prefix).
    pub key: String,
    /// Model.
    pub model: String,
    /// Start of the last user message.
    pub preview: String,
    /// Times it was sent.
    pub sent: u64,
    /// Calls that were duplicates or repeats of an earlier one.
    pub wasted_calls: u64,
    /// Their cost.
    pub wasted_usd: f64,
}

/// What [`analyze`] found.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct BurnReport {
    /// Calls read.
    pub calls: u64,
    /// Records skipped (no time or no request identity).
    pub skipped: u64,
    /// Calls with a price (logged or list).
    pub priced_calls: u64,
    /// Calls whose token counts were estimated from text length.
    pub estimated_calls: u64,
    /// Total cost of priced calls.
    pub total_usd: f64,
    /// First log time, Unix seconds.
    pub first: f64,
    /// Last log time, Unix seconds.
    pub last: f64,
    /// Identical request started before the first one finished.
    pub in_flight_duplicate: Bucket,
    /// Identical request within the TTL after a success.
    pub repeat_within_ttl: Bucket,
    /// Identical request after a non-retryable 4xx.
    pub retry_after_4xx: Bucket,
    /// Identical request after a 408/409/429/5xx/529 or a failure
    /// without a status.
    pub retry: Bucket,
    /// Retries a dollar budget (floor plus ratio of spend per hour) would
    /// have refused.
    pub retry_refused_by_budget: Bucket,
    /// Longest run of attempts of one request.
    pub longest_retry_chain: u64,
    /// Agent steps that repeated the previous action and got the identical
    /// observation back.
    pub agent_repeat: Bucket,
    /// Dollars on duplicates, repeats, 4xx retries and agent repeats.
    pub burned_usd: f64,
    /// The most repeated requests.
    pub top_repeated: Vec<Repeated>,
    /// Models with no logged cost and no list price.
    pub unpriced_models: Vec<String>,
}

impl BurnReport {
    /// `burned_usd` as a share of `total_usd`, 0 when nothing was priced.
    pub fn burned_share(&self) -> f64 {
        if self.total_usd > 0.0 {
            self.burned_usd / self.total_usd
        } else {
            0.0
        }
    }
}

struct Seen {
    /// Last call with this key: (end, ok, status).
    last_end: f64,
    last_ok: bool,
    last_status: Option<u16>,
    chain: u64,
    sent: u64,
    wasted_calls: u64,
    wasted_usd: f64,
    model: String,
    preview: String,
}

/// Replay `calls` (in any order; they are sorted by start time) and price
/// every duplicate, repeat and blind retry.
pub fn analyze(mut calls: Vec<Call>, opts: &BurnOptions) -> BurnReport {
    calls.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut r = BurnReport {
        calls: calls.len() as u64,
        first: calls.first().map_or(0.0, |c| c.start),
        last: calls.last().map_or(0.0, |c| c.start),
        ..BurnReport::default()
    };
    // The simulated budget's hour follows the log's clock, not the wall
    // clock: a fresh budget per hour of log time.
    let new_budget = || {
        llm_resilience::RetrySpend::new(opts.retry_budget_usd)
            .with_ratio(opts.retry_ratio)
            .with_window(std::time::Duration::from_secs(u64::MAX / 4))
    };
    let mut budget = new_budget();
    let mut window_start = r.first;
    let mut seen: HashMap<String, Seen> = HashMap::new();
    let mut traces: HashMap<String, (String, String)> = HashMap::new();
    let mut unpriced: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for c in &calls {
        if c.start - window_start >= 3600.0 {
            window_start = c.start;
            budget = new_budget();
        }
        let cost = opts.cost_of(c);
        if cost.is_some() {
            r.priced_calls += 1;
            r.total_usd += cost.unwrap_or(0.0);
        } else if !c.model.is_empty() {
            unpriced.insert(c.model.clone());
        }
        if c.estimated_tokens {
            r.estimated_calls += 1;
        }

        if let (Some(t), Some(a), Some(o)) = (&c.trace, &c.action, &c.observation) {
            if traces.get(t).is_some_and(|(pa, po)| pa == a && po == o) {
                r.agent_repeat.add(cost);
                r.burned_usd += cost.unwrap_or(0.0);
            }
            traces.insert(t.clone(), (a.clone(), o.clone()));
        }

        let key = match (&c.user, opts.per_user) {
            (Some(u), true) => format!("{u}\u{0}{}", c.key),
            _ => c.key.clone(),
        };
        let mut retried = false;
        let mut wasted = false;
        match seen.get_mut(&key) {
            Some(s) => {
                s.sent += 1;
                if !s.last_ok {
                    s.chain += 1;
                    r.longest_retry_chain = r.longest_retry_chain.max(s.chain);
                    let status = s.last_status.unwrap_or(0);
                    if (400..500).contains(&status) && !llm_resilience::retryable_status(status) {
                        r.retry_after_4xx.add(cost);
                        wasted = true;
                    } else {
                        r.retry.add(cost);
                        retried = true;
                        if budget.try_retry(cost.unwrap_or(0.0)).is_err() {
                            r.retry_refused_by_budget.add(cost);
                        }
                    }
                } else if c.start < s.last_end {
                    r.in_flight_duplicate.add(cost);
                    wasted = true;
                } else if c.start - s.last_end <= opts.ttl_secs {
                    r.repeat_within_ttl.add(cost);
                    wasted = true;
                }
                if wasted {
                    s.wasted_calls += 1;
                    s.wasted_usd += cost.unwrap_or(0.0);
                    r.burned_usd += cost.unwrap_or(0.0);
                }
                if c.ok() {
                    s.chain = 0;
                }
                // A duplicate that started while the first was in flight
                // does not move the end of the first answer back.
                s.last_end = s.last_end.max(c.end);
                // A failed duplicate of an answered request does not undo
                // the answer.
                s.last_ok = c.ok() || (s.last_ok && wasted);
                s.last_status = if s.last_ok { None } else { c.status };
            }
            None => {
                seen.insert(
                    key,
                    Seen {
                        last_end: c.end,
                        last_ok: c.ok(),
                        last_status: c.status,
                        chain: u64::from(!c.ok()),
                        sent: 1,
                        wasted_calls: 0,
                        wasted_usd: 0.0,
                        model: c.model.clone(),
                        preview: c.preview.clone(),
                    },
                );
            }
        }
        if !retried {
            budget.record_spend(cost.unwrap_or(0.0));
        }
    }

    let mut top: Vec<Repeated> = seen
        .into_iter()
        .filter(|(_, s)| s.wasted_calls > 0)
        .map(|(k, s)| Repeated {
            key: k
                .rsplit('\u{0}')
                .next()
                .unwrap_or(&k)
                .chars()
                .take(12)
                .collect(),
            model: s.model,
            preview: if s.preview.is_empty() {
                "(empty message)".to_string()
            } else {
                s.preview
            },
            sent: s.sent,
            wasted_calls: s.wasted_calls,
            wasted_usd: s.wasted_usd,
        })
        .collect();
    top.sort_by(|a, b| {
        b.wasted_usd
            .total_cmp(&a.wasted_usd)
            .then(b.wasted_calls.cmp(&a.wasted_calls))
            .then(a.key.cmp(&b.key))
    });
    top.truncate(opts.top);
    r.top_repeated = top;
    r.unpriced_models = unpriced.into_iter().collect();
    r
}

/// Parse a log: JSONL (one object per line) or one JSON array. Returns the
/// calls and the number of records skipped.
pub fn parse_log(text: &str) -> (Vec<Call>, u64) {
    let trimmed = text.trim_start();
    let values: Vec<Option<Value>> = match trimmed.starts_with('[') {
        true => match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => items.into_iter().map(Some).collect(),
            _ => vec![None],
        },
        false => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str::<Value>(l).ok())
            .collect(),
    };
    let mut calls = Vec::with_capacity(values.len());
    let mut skipped = 0;
    for v in values {
        match v.as_ref().and_then(Call::from_value) {
            Some(c) => calls.push(c),
            None => skipped += 1,
        }
    }
    (calls, skipped)
}

// ===========================================================================
// Static audit of retry code
// ===========================================================================

/// What kind of retry problem a line shows.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// A tenacity `@retry` / `Retrying` with no `stop=`: it never stops.
    RetryForever,
    /// Retries every exception (no `retry=` filter, or `Exception` listed),
    /// so a 400, 401 or context-length error is sent again.
    RetriesEveryException,
    /// A retry loop over `range(...)` that catches every exception.
    LoopRetriesEveryException,
    /// Retries capped by a count only: no dollar or token ceiling.
    CountCappedOnly,
}

impl AuditKind {
    /// One-line explanation.
    pub fn explain(self) -> &'static str {
        match self {
            AuditKind::RetryForever => "retry decorator with no stop condition: it retries until it succeeds",
            AuditKind::RetriesEveryException => {
                "retries every exception, so a 400/401/403/422 (bad request, bad key, context too long) is sent again"
            }
            AuditKind::LoopRetriesEveryException => {
                "retry loop catches every exception and tries again, 4xx included"
            }
            AuditKind::CountCappedOnly => "retries capped by count only; nothing caps what the retries cost",
        }
    }
}

/// One finding of [`audit_source`].
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AuditFinding {
    /// 1-based line.
    pub line: usize,
    /// What it is.
    pub kind: AuditKind,
    /// The line, trimmed.
    pub code: String,
}

/// The text of a call starting at `lines[i]`, up to its closing paren (at
/// most 25 lines).
fn call_text(lines: &[&str], i: usize) -> String {
    let mut depth: i32 = 0;
    let mut out = String::new();
    let mut opened = false;
    for line in lines.iter().skip(i).take(25) {
        out.push_str(line);
        out.push('\n');
        for ch in line.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    opened = true;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        if opened && depth <= 0 {
            break;
        }
        if !opened {
            break;
        }
    }
    out
}

/// Scan one source file for retry code that can burn money: tenacity and
/// `backoff` decorators (Python), retry loops that catch every exception,
/// and count-only retry settings. `path` picks the language by extension
/// (`.py`, `.ts`, `.js`, `.mjs`, `.rs`, `.go`); other files give nothing.
///
/// This is a pattern scan, not a type checker: every finding names a line
/// to read, and a reviewer decides. It errs towards fewer, surer findings.
pub fn audit_source(path: &str, text: &str) -> Vec<AuditFinding> {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut push = |line: usize, kind: AuditKind, code: &str| {
        let f = AuditFinding {
            line: line + 1,
            kind,
            code: code.trim().chars().take(160).collect(),
        };
        if !out.contains(&f) {
            out.push(f);
        }
    };
    match ext.as_str() {
        "py" => {
            let tenacity = text.contains("tenacity");
            let backoff = text.contains("import backoff") || text.contains("from backoff");
            for (i, raw) in lines.iter().enumerate() {
                let line = raw.trim();
                if line.starts_with('#') {
                    continue;
                }
                let is_tenacity_call = tenacity
                    && (line.starts_with("@retry(")
                        || line == "@retry"
                        || line.starts_with("@tenacity.retry")
                        || line.contains("Retrying(")
                        || line.contains("AsyncRetrying("));
                if is_tenacity_call {
                    let call = if line == "@retry" {
                        String::new()
                    } else {
                        call_text(&lines, i)
                    };
                    let filters_exceptions = call.contains("retry=") || call.contains("retry =");
                    let filters_everything = call.contains("retry_if_exception_type(Exception")
                        || call.contains("retry_if_exception_type((Exception");
                    if call.contains("(**") {
                        // Settings passed in as `**kwargs`: unknown here.
                    } else if !call.contains("stop=") && !call.contains("stop =") {
                        push(i, AuditKind::RetryForever, line);
                    } else if !filters_exceptions || filters_everything {
                        push(i, AuditKind::RetriesEveryException, line);
                    }
                }
                if backoff && line.contains("on_exception(") {
                    let call = call_text(&lines, i);
                    if call.contains(", Exception")
                        || call.contains("(Exception,")
                        || call.contains("Exception)")
                    {
                        push(i, AuditKind::RetriesEveryException, line);
                    }
                }
                if (line.starts_with("for ") || line.starts_with("while "))
                    && (line.contains("retr") || line.contains("attempt"))
                {
                    let body: Vec<&str> = lines.iter().skip(i + 1).take(30).copied().collect();
                    let indent = raw.len() - raw.trim_start().len();
                    let mut catches_all = None;
                    for (j, b) in body.iter().enumerate() {
                        let bi = b.len() - b.trim_start().len();
                        if !b.trim().is_empty() && bi <= indent {
                            break;
                        }
                        let t = b.trim();
                        if t == "except:"
                            || t.starts_with("except Exception")
                            || t.starts_with("except BaseException")
                        {
                            catches_all = Some(j);
                            break;
                        }
                    }
                    if let Some(j) = catches_all {
                        // Only a catch that always goes around again (no
                        // raise, return or break anywhere in it) retries on
                        // every exception; one with an exit path filters.
                        let except_line = body[j];
                        let ei = except_line.len() - except_line.trim_start().len();
                        let exits = body
                            .iter()
                            .skip(j + 1)
                            .take_while(|b| b.trim().is_empty() || b.len() - b.trim_start().len() > ei)
                            .any(|b| {
                                let t = b.trim();
                                t.starts_with("raise")
                                    || t.starts_with("return")
                                    || t.starts_with("break")
                                    || t.contains(" raise ")
                                    || t.ends_with(" raise")
                            });
                        if !exits {
                            push(i, AuditKind::LoopRetriesEveryException, line);
                        }
                    }
                }
            }
        }
        "ts" | "js" | "mjs" | "cjs" | "tsx" | "rs" | "go" => {
            for (i, raw) in lines.iter().enumerate() {
                let line = raw.trim();
                if line.starts_with("//") || line.starts_with('*') {
                    continue;
                }
                let is_loop = (line.starts_with("for ")
                    || line.starts_with("for(")
                    || line.starts_with("while"))
                    && (line.contains("retr")
                        || line.contains("attempt")
                        || line.contains("Retr")
                        || line.contains("Attempt"));
                if is_loop {
                    let window: Vec<&str> = lines.iter().skip(i + 1).take(40).copied().collect();
                    let catch_at = window.iter().position(|b| {
                        let t = b.trim();
                        t.starts_with("} catch (") || t.starts_with("} catch {") || t.starts_with("catch (")
                    });
                    if let Some(k) = catch_at {
                        // The catch body: from its `{` to the matching `}`.
                        let mut depth = 0i32;
                        let mut body = String::new();
                        let mut started = false;
                        'outer: for b in window.iter().skip(k) {
                            for ch in b.chars() {
                                match ch {
                                    '{' => {
                                        if started || b.contains("catch") {
                                            started = true;
                                        }
                                        depth += 1;
                                    }
                                    '}' => {
                                        depth -= 1;
                                        if started && depth <= 0 && !body.is_empty() {
                                            break 'outer;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            body.push_str(b);
                            body.push('\n');
                        }
                        let exits = body.contains("throw") || body.contains("return") || body.contains("break");
                        if !exits {
                            push(i, AuditKind::LoopRetriesEveryException, line);
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out.sort_by_key(|f| (f.line, f.kind));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(v: Value) -> Call {
        Call::from_value(&v).expect("call")
    }

    fn chat(t: f64, content: &str) -> Value {
        json!({"timestamp": t, "latency_ms": 2000, "model": "gpt-4o",
               "messages": [{"role": "user", "content": content}],
               "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}})
    }

    #[test]
    fn duplicates_repeats_and_fresh_calls_are_told_apart() {
        let calls = vec![
            call(chat(0.0, "a")),    // first: $2.50
            call(chat(1.0, "a")),    // started before the first ended: in flight
            call(chat(100.0, "a")),  // 98 s after: repeat within 5 min
            call(chat(1000.0, "a")), // 15 min later: fresh, not waste
            call(chat(1001.0, "b")), // another prompt
        ];
        let r = analyze(calls, &BurnOptions::default());
        assert_eq!(r.calls, 5);
        assert_eq!(r.in_flight_duplicate.calls, 1);
        assert_eq!(r.repeat_within_ttl.calls, 1);
        assert!((r.total_usd - 12.5).abs() < 1e-9, "{}", r.total_usd);
        assert!((r.burned_usd - 5.0).abs() < 1e-9);
        assert!((r.burned_share() - 0.4).abs() < 1e-9);
        assert_eq!(r.top_repeated.len(), 1);
        assert_eq!(r.top_repeated[0].sent, 4);
        assert_eq!(r.top_repeated[0].preview, "a");
    }

    #[test]
    fn a_retried_400_is_waste_and_a_retried_503_is_budgeted() {
        let mk = |t: f64, status: u16, cost: f64| {
            call(json!({"ts": t, "model": "m", "prompt": "p", "status": status, "cost": cost}))
        };
        let calls = vec![mk(0.0, 400, 0.0), mk(1.0, 400, 0.0), mk(2.0, 400, 0.0)];
        let r = analyze(calls, &BurnOptions::default());
        assert_eq!(r.retry_after_4xx.calls, 2);
        assert_eq!(r.retry.calls, 0);
        assert_eq!(
            r.longest_retry_chain, 3,
            "three attempts of the same request"
        );

        // Three paid 503 retries of $0.40 each; a $0.50 budget pays for one.
        let calls = vec![
            mk(0.0, 503, 0.4),
            mk(1.0, 503, 0.4),
            mk(2.0, 503, 0.4),
            mk(3.0, 200, 0.4),
        ];
        let r = analyze(calls, &BurnOptions::default());
        assert_eq!(r.retry.calls, 3);
        assert_eq!(r.retry_refused_by_budget.calls, 2);
        assert!((r.retry_refused_by_budget.usd - 0.8).abs() < 1e-9);
    }

    #[test]
    fn per_user_scope_only_matches_one_users_requests() {
        let mk = |t: f64, user: &str| {
            call(json!({"ts": t, "model": "gpt-4o", "user": user,
                        "messages": [{"role": "user", "content": "hi"}], "cost": 1.0}))
        };
        let calls = || vec![mk(0.0, "u1"), mk(10.0, "u2"), mk(20.0, "u1")];
        // `user` is volatile, so across the log all three are one request.
        assert_eq!(
            analyze(calls(), &BurnOptions::default())
                .repeat_within_ttl
                .calls,
            2
        );
        let per_user = BurnOptions {
            per_user: true,
            ..BurnOptions::default()
        };
        assert_eq!(analyze(calls(), &per_user).repeat_within_ttl.calls, 1);
    }

    #[test]
    fn agent_repeats_need_the_same_action_and_the_same_result() {
        let mk = |t: f64, action: &str, obs: &str| {
            call(
                json!({"ts": t, "trace_id": "t1", "request_hash": format!("h{t}"), "model": "m",
                        "action": action, "observation": obs, "cost": 0.1}),
            )
        };
        let calls = vec![
            mk(0.0, "ls", "a.py"),
            mk(1.0, "python a.py", "Error: x"),
            mk(2.0, "python a.py", "Error: x"), // same action, same error: waste
            mk(3.0, "python a.py", "ok"),       // same action, new result: not waste
        ];
        let r = analyze(calls, &BurnOptions::default());
        assert_eq!(r.agent_repeat.calls, 1);
        assert!((r.agent_repeat.usd - 0.1).abs() < 1e-9);
    }

    #[test]
    fn fields_from_common_log_shapes_are_read() {
        // LiteLLM spend log.
        let c = call(
            json!({"startTime": "2026-11-01T10:00:00Z", "endTime": "2026-11-01T10:00:03Z",
                            "model": "claude-sonnet-4-5", "spend": 0.02, "status": "success",
                            "messages": [{"role": "user", "content": "x"}], "prompt_tokens": 10, "completion_tokens": 5}),
        );
        assert!((c.end - c.start - 3.0).abs() < 1e-6);
        assert_eq!(c.cost(), Some(0.02));
        assert!(c.ok());
        // OpenAI-style request/response log with a list price.
        let c = call(
            json!({"created_at": 1_762_000_000_000_i64, "status_code": 429,
                            "request": {"model": "gpt-4o", "messages": [], "user": "x"},
                            "response": {"usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}}}),
        );
        assert_eq!(c.status, Some(429));
        assert!((c.start - 1_762_000_000.0).abs() < 1e-6);
        assert_eq!(c.model, "gpt-4o");
        assert_eq!(c.cost(), Some(2.5));
        // No time: skipped.
        assert!(Call::from_value(&json!({"model": "m", "prompt": "p"})).is_none());
        // An old snapshot id is priced at its base model's row; an explicit
        // price wins.
        let old = call(
            json!({"ts": 1, "model": "gpt-3.5-turbo-0301", "prompt": "p",
                              "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}}),
        );
        assert_eq!(
            old.cost(),
            llm_prices::lookup("gpt-3.5-turbo").and_then(|p| p.cost(1_000_000, 0))
        );
        let mut opts = BurnOptions::default();
        opts.prices.insert("gpt-3.5-turbo-0301".into(), (2.0, 2.0));
        assert_eq!(opts.cost_of(&old), Some(2.0));
        let (calls, skipped) =
            parse_log("{\"ts\":1,\"prompt\":\"p\"}\nnot json\n\n{\"prompt\":\"p\"}\n");
        assert_eq!((calls.len(), skipped), (1, 2));
    }

    #[test]
    fn audit_flags_retry_code_that_can_burn_money() {
        let py = r#"
import tenacity
from tenacity import retry, stop_after_attempt

@retry(wait=wait_random_exponential(min=1, max=60))
def forever(): ...

@retry(
    stop=stop_after_attempt(6),
    wait=wait_exponential(),
)
def every_exception(): ...

@retry(stop=stop_after_attempt(3), retry=retry_if_exception_type(RateLimitError))
def fine(): ...

@retry(**config)
def configured_elsewhere(): ...

def loop():
    for attempt in range(5):
        try:
            return call()
        except Exception as e:
            time.sleep(1)
"#;
        let f = audit_source("x.py", py);
        let kinds: Vec<(usize, AuditKind)> = f.iter().map(|f| (f.line, f.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (5, AuditKind::RetryForever),
                (8, AuditKind::RetriesEveryException),
                (21, AuditKind::LoopRetriesEveryException),
            ]
        );
        let ts = "for (let attempt = 0; attempt < 5; attempt++) {\n  try {\n    return await call();\n  } catch (e) {\n    await sleep(1000);\n  }\n}\n";
        assert_eq!(audit_source("x.ts", ts).len(), 1);
        let ts_ok = "for (let attempt = 0; attempt < 5; attempt++) {\n  try {\n    return await call();\n  } catch (e) {\n    if (e.status < 500) throw e;\n  }\n}\n";
        assert!(audit_source("x.ts", ts_ok).is_empty());
        assert!(audit_source("x.md", py).is_empty());
    }
}
