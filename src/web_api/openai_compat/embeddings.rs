//! OpenAI embeddings: `POST /v1/embeddings`, with caching of identical inputs.
//!
//! Vectors come from an [`EmbeddingsBackend`]: an OpenAI-compatible
//! `/embeddings` endpoint (OpenAI, OpenRouter, vLLM, llama.cpp, Ollama,
//! LM Studio, ...) or a local [`Embedder`](crate::embedding::Embedder).
//! A request's `model` picks the backend: a `[[models]]` route of provider
//! `openai` or `llama_cpp` that matches it sends the request to that route's
//! server (its `base_url` and key), anything else goes to
//! [`ServerConfig::embeddings`].
//!
//! Every input is cached on its own, keyed by backend, model, `dimensions`
//! and the input itself, for [`ServerConfig::dedup_window_secs`]: a batch
//! that repeats inputs seen recently sends only the new ones upstream, the
//! same input twice in one batch is embedded once, and identical inputs in
//! concurrent requests share one upstream call. The `x-orchestrator-dedup`
//! header says `miss` (all inputs embedded by this request), `cached`,
//! `joined` or `partial` (a mix), and `x-orchestrator-cached-inputs` counts
//! the inputs that did not need a new upstream call.
//!
//! The spend cap applies (OpenAI embedding prices; local backends are
//! free), and each backend has its own circuit breaker: five upstream
//! failures in a row (5xx, timeouts, connection errors; not 4xx) open it
//! for 30 seconds, during which requests get 503 at once.
//!
//! `encoding_format` `float` and `base64` are both supported (the OpenAI
//! Python SDK asks for `base64` by default); inputs may be strings or token
//! arrays (token arrays need an HTTP backend).

use super::*;

use std::collections::HashMap as StdHashMap;

use crate::embedding::Embedder;
use crate::enhanced::circuit_breaker::CircuitBreakerError;
use crate::enhanced::CircuitBreaker;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

/// Most inputs one request may carry (OpenAI's own limit).
const MAX_INPUTS: usize = 2048;

/// Bytes of vectors the input cache may hold.
const CACHE_BYTES: u64 = 64 * 1024 * 1024;

/// Default per-call timeout of an HTTP backend.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// One embedding vector, shared between the cache and responses.
type Vector = Arc<Vec<f32>>;

/// The outcome of one input, shared with concurrent requests for it.
type Shared = Option<Result<Vector, ApiError>>;

// ============================================================================
// Backend
// ============================================================================

/// Where `POST /v1/embeddings` gets vectors from; set it as
/// [`ServerConfig::embeddings`](crate::web_api::ServerConfig::embeddings).
///
/// Two kinds:
///
/// - [`openai_compatible`](Self::openai_compatible): any server with an
///   OpenAI-style `POST {base_url}/embeddings` (OpenAI, Azure-style proxies,
///   OpenRouter, vLLM, llama.cpp started with `--embeddings`, Ollama's
///   `/v1`, LM Studio). The client's `model`, `dimensions` and `user` are
///   passed through.
/// - [`from_embedder`](Self::from_embedder): a local or custom
///   [`Embedder`], such as `integrations::FastEmbedder`. Text inputs only,
///   no `dimensions`.
///
/// Each backend has its own circuit breaker. Cloning is cheap and clones
/// share the breaker.
///
/// # Examples
///
/// ```
/// use tokio_prompt_orchestrator::web_api::{EmbeddingsBackend, ServerConfig};
///
/// let config = ServerConfig {
///     embeddings: Some(
///         EmbeddingsBackend::openai_compatible("http://localhost:11434/v1", None)
///             .with_default_model("nomic-embed-text")
///             .with_billing(false),
///     ),
///     ..ServerConfig::default()
/// };
/// assert!(config.embeddings.is_some());
/// ```
#[derive(Clone)]
pub struct EmbeddingsBackend {
    kind: BackendKind,
    default_model: Option<String>,
    billed: bool,
    timeout: Duration,
    breaker: CircuitBreaker,
}

#[derive(Clone)]
enum BackendKind {
    Http {
        base_url: String,
        api_key: Option<String>,
        client: reqwest::Client,
    },
    Embedder {
        embedder: Arc<dyn Embedder>,
        model: String,
    },
}

impl std::fmt::Debug for EmbeddingsBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("EmbeddingsBackend");
        match &self.kind {
            BackendKind::Http { base_url, api_key, .. } => {
                d.field("base_url", base_url).field("api_key", &api_key.as_ref().map(|_| "<set>"));
            }
            BackendKind::Embedder { embedder, model } => {
                d.field("embedder", embedder).field("model", model);
            }
        }
        d.field("default_model", &self.default_model)
            .field("billed", &self.billed)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl EmbeddingsBackend {
    fn with_kind(kind: BackendKind, billed: bool) -> Self {
        Self {
            kind,
            default_model: None,
            billed,
            timeout: DEFAULT_TIMEOUT,
            breaker: CircuitBreaker::new(5, 0.8, Duration::from_secs(30)),
        }
    }

    /// An OpenAI-compatible server: requests go to `{base_url}/embeddings`
    /// (for example `https://api.openai.com/v1`), with
    /// `Authorization: Bearer <api_key>` when a key is given. Calls are
    /// counted against the spend cap at OpenAI's embedding prices; use
    /// [`with_billing(false)`](Self::with_billing) for a free local server.
    pub fn openai_compatible(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self::with_kind(
            BackendKind::Http {
                base_url,
                api_key: api_key.filter(|k| !k.is_empty()),
                client: reqwest::Client::new(),
            },
            true,
        )
    }

    /// A local or custom [`Embedder`], reported to clients as `model`. Each
    /// input is embedded with one `embed` call; token-array inputs and
    /// `dimensions` are answered 400. Free (not counted against the spend
    /// cap).
    pub fn from_embedder(embedder: Arc<dyn Embedder>, model: impl Into<String>) -> Self {
        let model = model.into();
        let mut backend = Self::with_kind(
            BackendKind::Embedder {
                embedder,
                model: model.clone(),
            },
            false,
        );
        backend.default_model = Some(model);
        backend
    }

    /// The model used when a request names none (OpenAI requires `model`,
    /// but some clients leave it out).
    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Whether calls count against
    /// [`ServerConfig::max_spend_usd`](crate::web_api::ServerConfig::max_spend_usd).
    /// On by default for [`openai_compatible`](Self::openai_compatible), off
    /// for [`from_embedder`](Self::from_embedder).
    pub fn with_billing(mut self, billed: bool) -> Self {
        self.billed = billed;
        self
    }

    /// Per-call timeout of an HTTP backend (default 60 s). A call that takes
    /// longer is answered 504 and counts against the circuit breaker.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Reject what this backend cannot embed, before any call is made.
    fn check(&self, inputs: &[Value], dimensions: Option<u32>) -> Result<(), ApiError> {
        if let BackendKind::Embedder { .. } = self.kind {
            if dimensions.is_some() {
                return Err(ApiError::invalid(
                    Some("dimensions"),
                    "'dimensions' is not supported by the local embedding model.",
                ));
            }
            if inputs.iter().any(|i| !i.is_string()) {
                return Err(ApiError::invalid(
                    Some("input"),
                    "Token-array input needs an OpenAI-compatible embeddings server; the local \
                     embedding model takes text.",
                ));
            }
        }
        Ok(())
    }

    /// Embed `inputs` with `model`: one vector per input, in order, and the
    /// upstream's prompt token count if it reported one.
    async fn embed(
        &self,
        model: &str,
        inputs: &[Value],
        dimensions: Option<u32>,
        user: Option<&str>,
    ) -> Result<(Vec<Vec<f32>>, Option<u64>), ApiError> {
        let result = self
            .breaker
            .call(|| async {
                // Client errors are the request's fault, not the upstream's:
                // return them as Ok so the breaker does not count them.
                match self.embed_once(model, inputs, dimensions, user).await {
                    Err(e) if e.status.is_client_error() => Ok(Err(e)),
                    other => other.map(Ok),
                }
            })
            .await;
        match result {
            Ok(inner) => inner,
            Err(CircuitBreakerError::Failed(e)) => Err(e),
            Err(CircuitBreakerError::Open) => {
                let mut err = ApiError::from_dlq_reason("circuit_breaker_open");
                if let Some(wait) = self.breaker.retry_after_sync() {
                    err = err.retry_after((wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1));
                }
                Err(err)
            }
        }
    }

    async fn embed_once(
        &self,
        model: &str,
        inputs: &[Value],
        dimensions: Option<u32>,
        user: Option<&str>,
    ) -> Result<(Vec<Vec<f32>>, Option<u64>), ApiError> {
        match &self.kind {
            BackendKind::Embedder { embedder, .. } => {
                let mut out = Vec::with_capacity(inputs.len());
                for input in inputs {
                    let text = input.as_str().unwrap_or_default();
                    let v = embedder.embed(text).await.map_err(|e| {
                        ApiError::from_dlq_reason(&format!("inference_failure:embedding failed: {e}"))
                    })?;
                    out.push(v);
                }
                Ok((out, None))
            }
            BackendKind::Http { base_url, api_key, client } => {
                let mut body = serde_json::json!({
                    "model": model,
                    "input": inputs,
                    "encoding_format": "float",
                });
                if let Some(d) = dimensions {
                    body["dimensions"] = Value::from(d);
                }
                if let Some(u) = user {
                    body["user"] = Value::from(u);
                }
                let mut request = client
                    .post(format!("{base_url}/embeddings"))
                    .timeout(self.timeout)
                    .json(&body);
                if let Some(key) = api_key {
                    request = request.bearer_auth(key);
                }
                let resp = request.send().await.map_err(|e| {
                    if e.is_timeout() {
                        ApiError::from_dlq_reason(&format!("inference_timeout:{}s", self.timeout.as_secs()))
                    } else {
                        ApiError::from_dlq_reason(&format!("inference_failure:{e}"))
                    }
                })?;
                let status = resp.status();
                let text = resp
                    .text()
                    .await
                    .map_err(|e| ApiError::from_dlq_reason(&format!("inference_failure:{e}")))?;
                if !status.is_success() {
                    let message = serde_json::from_str::<Value>(&text)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                        .unwrap_or_else(|| text.chars().take(500).collect());
                    let reason = if status.as_u16() == 429 {
                        format!("inference_failure:rate limited (429): {message}")
                    } else if status.is_client_error() {
                        format!("upstream_rejected:{}:{message}", status.as_u16())
                    } else {
                        format!("inference_failure:{status}: {message}")
                    };
                    return Err(ApiError::from_dlq_reason(&reason));
                }
                parse_openai_embeddings(&text, inputs.len())
            }
        }
    }
}

/// Vectors (in input order) and prompt tokens from an OpenAI embeddings body.
fn parse_openai_embeddings(text: &str, expected: usize) -> Result<(Vec<Vec<f32>>, Option<u64>), ApiError> {
    let bad = |why: &str| ApiError::from_dlq_reason(&format!("inference_failure:unexpected embeddings response: {why}"));
    let body: Value = serde_json::from_str(text).map_err(|_| bad("not JSON"))?;
    let data = body["data"].as_array().ok_or_else(|| bad("no 'data' array"))?;
    let mut out: Vec<Option<Vec<f32>>> = vec![None; expected];
    for (position, item) in data.iter().enumerate() {
        let index = item["index"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(position);
        let vector: Vec<f32> = item["embedding"]
            .as_array()
            .ok_or_else(|| bad("an embedding is not an array of numbers"))?
            .iter()
            .map(|x| x.as_f64().map(|f| f as f32))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("an embedding is not an array of numbers"))?;
        let slot = out.get_mut(index).ok_or_else(|| bad("index out of range"))?;
        *slot = Some(vector);
    }
    let vectors = out
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| bad("fewer embeddings than inputs"))?;
    let tokens = body["usage"]["prompt_tokens"]
        .as_u64()
        .or_else(|| body["usage"]["total_tokens"].as_u64());
    Ok((vectors, tokens))
}

// ============================================================================
// Server state: cache and in-flight table
// ============================================================================

/// Per-server state for `/v1/embeddings`.
pub(in crate::web_api) struct EmbeddingsState {
    /// Completed vectors by input key; `None` when the dedup window is 0.
    cache: Option<moka::sync::Cache<String, Vector>>,
    /// Inputs being embedded right now, so concurrent requests share them.
    in_flight: parking_lot::Mutex<StdHashMap<String, watch::Receiver<Shared>>>,
    /// Backends built for `[[models]]` routes, by route label.
    route_backends: DashMap<String, EmbeddingsBackend>,
}

impl EmbeddingsState {
    pub(in crate::web_api) fn new(window: Duration) -> Self {
        let cache = (!window.is_zero()).then(|| {
            moka::sync::Cache::builder()
                .time_to_live(window)
                .weigher(|k: &String, v: &Vector| {
                    u32::try_from(k.len() + v.len() * 4).unwrap_or(u32::MAX)
                })
                .max_capacity(CACHE_BYTES)
                .build()
        });
        Self {
            cache,
            in_flight: parking_lot::Mutex::new(StdHashMap::new()),
            route_backends: DashMap::new(),
        }
    }

    fn cached(&self, key: &str) -> Option<Vector> {
        self.cache.as_ref().and_then(|c| c.get(key))
    }
}

/// Removes the in-flight entries a leader owns when it finishes (or is
/// dropped), so later requests read the cache or start a new call.
struct LeadGuard {
    state: Arc<AppState>,
    keys: Vec<String>,
    senders: Vec<watch::Sender<Shared>>,
}

impl Drop for LeadGuard {
    fn drop(&mut self) {
        let mut map = self.state.openai.embeddings.in_flight.lock();
        for (key, tx) in self.keys.iter().zip(&self.senders) {
            if map.get(key).is_some_and(|rx| rx.same_channel(&tx.subscribe())) {
                map.remove(key);
            }
        }
    }
}

// ============================================================================
// Request handling
// ============================================================================

#[derive(Debug, Deserialize)]
struct EmbeddingsRequest {
    #[serde(default)]
    model: Option<String>,
    input: Value,
    #[serde(default)]
    encoding_format: Option<String>,
    #[serde(default)]
    dimensions: Option<u32>,
    #[serde(default)]
    user: Option<String>,
}

/// Where one embeddings request goes.
struct Selected {
    backend: EmbeddingsBackend,
    /// Part of every cache key, so backends never share vectors.
    label: String,
    upstream_model: String,
    reported_model: String,
}

/// The base URL (ending in `/v1`) of a route's OpenAI-compatible server.
fn route_base_url(route: &crate::model_routes::ModelRoute) -> Option<String> {
    use crate::model_routes::RouteProvider;
    let configured = route.base_url.as_deref().map(|u| u.trim_end_matches('/').to_string());
    match route.provider {
        RouteProvider::OpenAi => Some(configured.unwrap_or_else(|| "https://api.openai.com/v1".to_string())),
        RouteProvider::LlamaCpp => {
            let base = configured.unwrap_or_else(|| {
                std::env::var("LLAMA_CPP_URL")
                    .unwrap_or_else(|_| "http://localhost:8080".to_string())
                    .trim_end_matches('/')
                    .to_string()
            });
            Some(if base.ends_with("/v1") { base } else { format!("{base}/v1") })
        }
        _ => None,
    }
}

fn select_backend(state: &AppState, requested: Option<&str>) -> Result<Selected, ApiError> {
    let requested = requested.map(str::trim).filter(|m| !m.is_empty());
    if let (Some(model), Some(routes)) = (requested, state.config.model_routes.as_deref()) {
        let route = routes.resolve(model).and_then(|i| routes.route(i));
        if let Some(route) = route {
            if let Some(base_url) = route_base_url(route) {
                let label = format!("route:{}", route.name.to_ascii_lowercase());
                let backend = match state.openai.embeddings.route_backends.get(&label) {
                    Some(b) => b.clone(),
                    None => {
                        let key = route
                            .key_variable()
                            .and_then(|var| std::env::var(var).ok())
                            .filter(|k| !k.is_empty());
                        let backend = EmbeddingsBackend::openai_compatible(base_url, key)
                            .with_billing(!route.provider.is_free());
                        state
                            .openai
                            .embeddings
                            .route_backends
                            .entry(label.clone())
                            .or_insert(backend)
                            .clone()
                    }
                };
                let upstream = route.upstream_model(model);
                return Ok(Selected {
                    backend,
                    label,
                    reported_model: upstream.clone(),
                    upstream_model: upstream,
                });
            }
            if state.config.embeddings.is_none() {
                return Err(ApiError {
                    param: Some("model"),
                    ..ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_request_error",
                        "model_not_found",
                        format!(
                            "The model '{model}' is routed to {}, which has no embeddings API.",
                            route.provider
                        ),
                    )
                });
            }
        }
    }
    let Some(backend) = state.config.embeddings.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "embeddings_not_configured",
            "This orchestrator has no embeddings backend. Start it with --provider openai or \
             --provider llama, add an openai or llama_cpp route for the embedding model, or set \
             ServerConfig::embeddings.",
        ));
    };
    let model = match (&backend.kind, requested) {
        (BackendKind::Embedder { model, .. }, _) => model.clone(),
        (_, Some(m)) => m.to_string(),
        (_, None) => backend
            .default_model
            .clone()
            .ok_or_else(|| ApiError::invalid(Some("model"), "'model' is required."))?,
    };
    Ok(Selected {
        backend,
        label: "default".to_string(),
        reported_model: model.clone(),
        upstream_model: model,
    })
}

/// The request's inputs, one JSON value each (a string or a token array).
fn normalize_input(input: &Value) -> Result<Vec<Value>, ApiError> {
    let bad = |m: &str| ApiError::invalid(Some("input"), m.to_string());
    let is_token = |v: &Value| v.as_u64().is_some();
    let is_token_array = |v: &Value| v.as_array().is_some_and(|a| !a.is_empty() && a.iter().all(is_token));
    let items = match input {
        Value::String(_) => vec![input.clone()],
        Value::Array(items) if items.is_empty() => return Err(bad("'input' must not be empty.")),
        Value::Array(items) if items.iter().all(Value::is_string) => items.clone(),
        Value::Array(items) if items.iter().all(is_token) => vec![input.clone()],
        Value::Array(items) if items.iter().all(is_token_array) => items.clone(),
        _ => {
            return Err(bad(
                "'input' must be a string, an array of strings, an array of tokens or an array of token arrays.",
            ))
        }
    };
    if items.len() > MAX_INPUTS {
        return Err(bad(&format!("At most {MAX_INPUTS} inputs per request.")));
    }
    if items.iter().any(|i| i.as_str().is_some_and(str::is_empty)) {
        return Err(bad("'input' must not contain empty strings."));
    }
    Ok(items)
}

/// Cache key of one input.
fn input_key(label: &str, model: &str, dimensions: Option<u32>, input: &Value) -> String {
    let mut h = Sha256::new();
    h.update(b"embed-v1|");
    for part in [label, model, &dimensions.map(|d| d.to_string()).unwrap_or_default()] {
        h.update(part.as_bytes());
        h.update(b"|");
    }
    h.update(input.to_string().as_bytes());
    hex::encode(h.finalize())
}

/// Tokens of one input: a token array's length, else counted with the
/// model's tokenizer (feature `tiktoken`) or estimated.
fn input_tokens(model: &str, input: &Value) -> usize {
    match input {
        Value::String(s) => crate::token_counter::exact_token_count(model, s).unwrap_or_else(|| approx_tokens(s)),
        Value::Array(tokens) => tokens.len(),
        _ => 0,
    }
}

/// USD per million input tokens for an embedding model. Unknown models are
/// priced like `text-embedding-3-large`, erring high for the spend cap.
fn embedding_price_per_1m(model: &str) -> f64 {
    let m = model.to_ascii_lowercase();
    if m.contains("text-embedding-3-small") {
        0.02
    } else if m.contains("ada-002") {
        0.10
    } else {
        0.13
    }
}

/// Little-endian `f32` bytes, base64-encoded (OpenAI's `encoding_format: "base64"`).
fn base64_f32(v: &[f32]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Run the upstream call for the inputs this request leads, publish the
/// results to everyone waiting on them, and cache them.
async fn lead(
    state: Arc<AppState>,
    selected: Arc<Selected>,
    guard: LeadGuard,
    inputs: Vec<Value>,
    dimensions: Option<u32>,
    user: Option<String>,
) -> Result<(Vec<Vector>, Option<u64>), ApiError> {
    let result = selected
        .backend
        .embed(&selected.upstream_model, &inputs, dimensions, user.as_deref())
        .await;
    match result {
        Ok((vectors, tokens)) => {
            let vectors: Vec<Vector> = vectors.into_iter().map(Arc::new).collect();
            if selected.backend.billed {
                let counted = tokens.map_or_else(
                    || inputs.iter().map(|i| input_tokens(&selected.upstream_model, i)).sum::<usize>() as f64,
                    |t| t as f64,
                );
                let cost = counted / 1_000_000.0 * embedding_price_per_1m(&selected.upstream_model);
                if cost > 0.0 {
                    *state.openai.spent_usd.lock() += cost;
                    crate::metrics::record_inference_cost(cost);
                }
            }
            for ((key, tx), v) in guard.keys.iter().zip(&guard.senders).zip(&vectors) {
                if let Some(cache) = &state.openai.embeddings.cache {
                    cache.insert(key.clone(), Arc::clone(v));
                }
                let _ = tx.send(Some(Ok(Arc::clone(v))));
            }
            drop(guard);
            Ok((vectors, tokens))
        }
        Err(err) => {
            for tx in &guard.senders {
                let _ = tx.send(Some(Err(err.clone())));
            }
            drop(guard);
            Err(err)
        }
    }
}

/// How the inputs of one request were answered.
#[derive(Default)]
struct Sources {
    led: usize,
    joined: usize,
    cached: usize,
}

impl Sources {
    fn header(&self) -> &'static str {
        match (self.led > 0, self.joined > 0, self.cached > 0) {
            (true, false, false) => "miss",
            (false, false, true) => "cached",
            (false, true, false) => "joined",
            _ => "partial",
        }
    }
}

/// `POST /v1/embeddings`: OpenAI embeddings with per-input caching. See the
/// module docs.
pub(in crate::web_api) async fn embeddings_handler(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Response {
    match embeddings(state, body).await {
        Ok(resp) => resp,
        Err(err) => err.into_response(),
    }
}

async fn embeddings(state: Arc<AppState>, body: axum::body::Bytes) -> Result<Response, ApiError> {
    if state.shutting_down.load(AtomicOrdering::Relaxed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server_error",
            "shutting_down",
            "The orchestrator is shutting down.",
        ));
    }
    let req: EmbeddingsRequest = serde_json::from_slice(&body).map_err(|e| {
        ApiError::invalid(None, format!("Could not parse the request body as an embeddings request: {e}"))
    })?;
    let base64 = match req.encoding_format.as_deref() {
        None | Some("float") => false,
        Some("base64") => true,
        Some(other) => {
            return Err(ApiError::invalid(
                Some("encoding_format"),
                format!("'encoding_format' must be float or base64, got '{other}'."),
            ))
        }
    };
    if req.dimensions == Some(0) {
        return Err(ApiError::invalid(Some("dimensions"), "'dimensions' must be at least 1."));
    }
    let inputs = normalize_input(&req.input)?;
    let selected = Arc::new(select_backend(&state, req.model.as_deref())?);
    selected.backend.check(&inputs, req.dimensions)?;
    if selected.backend.billed {
        check_spend_cap(&state, "insufficient_quota")?;
    }

    // Unique inputs, each with the positions it fills.
    let keys: Vec<String> = inputs
        .iter()
        .map(|i| input_key(&selected.label, &selected.upstream_model, req.dimensions, i))
        .collect();
    let mut vectors: Vec<Option<Vector>> = vec![None; inputs.len()];
    let mut unique: Vec<usize> = Vec::new();
    let mut first_of: StdHashMap<&str, usize> = StdHashMap::new();
    for (i, key) in keys.iter().enumerate() {
        first_of.entry(key.as_str()).or_insert_with(|| {
            unique.push(i);
            i
        });
    }

    let mut sources = Sources::default();
    let mut lead_keys = Vec::new();
    let mut lead_senders = Vec::new();
    let mut lead_positions = Vec::new();
    let mut joined: Vec<(usize, watch::Receiver<Shared>)> = Vec::new();
    {
        let mut in_flight = state.openai.embeddings.in_flight.lock();
        for &i in &unique {
            let key = &keys[i];
            // Read the cache under the lock: a leader that just finished
            // cached its vectors before giving up its in-flight entries.
            if let Some(v) = state.openai.embeddings.cached(key) {
                vectors[i] = Some(v);
                sources.cached += 1;
            } else if let Some(rx) = in_flight.get(key).filter(|rx| rx.has_changed().is_ok()) {
                joined.push((i, rx.clone()));
                sources.joined += 1;
            } else {
                let (tx, rx) = watch::channel(None);
                in_flight.insert(key.clone(), rx);
                lead_keys.push(key.clone());
                lead_senders.push(tx);
                lead_positions.push(i);
                sources.led += 1;
            }
        }
    }

    let mut upstream_tokens = None;
    if !lead_positions.is_empty() {
        let guard = LeadGuard {
            state: Arc::clone(&state),
            keys: lead_keys,
            senders: lead_senders,
        };
        let led_inputs: Vec<Value> = lead_positions.iter().map(|&i| inputs[i].clone()).collect();
        // Its own task: a client that disconnects does not cancel the call
        // for the requests that joined it.
        let task = tokio::spawn(lead(
            Arc::clone(&state),
            Arc::clone(&selected),
            guard,
            led_inputs,
            req.dimensions,
            req.user.clone(),
        ));
        let (led_vectors, tokens) = task.await.map_err(|e| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "internal_error", format!("Internal error: {e}"))
        })??;
        upstream_tokens = tokens;
        for (&i, v) in lead_positions.iter().zip(led_vectors) {
            vectors[i] = Some(v);
        }
    }
    for (i, mut rx) in joined {
        let shared = rx.wait_for(Option::is_some).await.map(|v| (*v).clone()).ok().flatten();
        match shared {
            Some(Ok(v)) => vectors[i] = Some(v),
            Some(Err(err)) => return Err(err),
            None => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server_error",
                    "dedup_retry_exhausted",
                    "An identical embeddings request was cancelled. Retry.",
                )
                .retry_after(1))
            }
        }
    }
    // Duplicates within this request take the first occurrence's vector.
    for (i, key) in keys.iter().enumerate() {
        if vectors[i].is_none() {
            vectors[i] = first_of.get(key.as_str()).and_then(|&f| vectors[f].clone());
        }
    }

    let prompt_tokens = match upstream_tokens {
        Some(t) if sources.cached == 0 && sources.joined == 0 && unique.len() == inputs.len() => {
            usize::try_from(t).unwrap_or(usize::MAX)
        }
        _ => inputs.iter().map(|i| input_tokens(&selected.upstream_model, i)).sum(),
    };
    let mut data = Vec::with_capacity(inputs.len());
    for (index, v) in vectors.into_iter().enumerate() {
        let v = v.ok_or_else(|| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "server_error", "internal_error", "An input got no embedding.")
        })?;
        let embedding = if base64 { Value::from(base64_f32(&v)) } else { serde_json::json!(*v) };
        data.push(serde_json::json!({"object": "embedding", "index": index, "embedding": embedding}));
    }
    debug!(
        model = %selected.reported_model,
        inputs = data.len(),
        led = sources.led,
        joined = sources.joined,
        cached = sources.cached,
        "POST /v1/embeddings"
    );
    let mut resp = Json(serde_json::json!({
        "object": "list",
        "data": data,
        "model": selected.reported_model,
        "usage": {"prompt_tokens": prompt_tokens, "total_tokens": prompt_tokens},
    }))
    .into_response();
    resp.headers_mut()
        .insert("x-orchestrator-dedup", HeaderValue::from_static(sources.header()));
    let reused = inputs.len() - sources.led;
    if let Ok(v) = HeaderValue::from_str(&reused.to_string()) {
        resp.headers_mut().insert("x-orchestrator-cached-inputs", v);
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inputs_normalize_to_one_value_each() {
        assert_eq!(normalize_input(&json!("a")).expect("ok"), vec![json!("a")]);
        assert_eq!(normalize_input(&json!(["a", "b"])).expect("ok").len(), 2);
        assert_eq!(normalize_input(&json!([1, 2, 3])).expect("ok"), vec![json!([1, 2, 3])]);
        assert_eq!(normalize_input(&json!([[1], [2, 3]])).expect("ok").len(), 2);
        assert!(normalize_input(&json!([])).is_err());
        assert!(normalize_input(&json!(["a", 1])).is_err());
        assert!(normalize_input(&json!([""])).is_err());
        assert!(normalize_input(&json!({"text": "a"})).is_err());
    }

    #[test]
    fn keys_depend_on_backend_model_dimensions_and_input() {
        let k = input_key("default", "m", None, &json!("a"));
        assert_eq!(k, input_key("default", "m", None, &json!("a")));
        assert_ne!(k, input_key("route:x", "m", None, &json!("a")));
        assert_ne!(k, input_key("default", "m2", None, &json!("a")));
        assert_ne!(k, input_key("default", "m", Some(8), &json!("a")));
        assert_ne!(k, input_key("default", "m", None, &json!("b")));
    }

    #[test]
    fn base64_matches_known_encodings() {
        // 1.0f32 = 00 00 80 3f
        assert_eq!(base64_f32(&[1.0]), "AACAPw==");
        // 1.0, 2.0 = 00 00 80 3f 00 00 00 40
        assert_eq!(base64_f32(&[1.0, 2.0]), "AACAPwAAAEA=");
        assert_eq!(base64_f32(&[]), "");
        // 3 floats = 12 bytes, no padding.
        assert!(!base64_f32(&[0.5, -0.25, 3.0]).contains('='));
    }

    #[test]
    fn upstream_bodies_are_parsed_in_index_order() {
        let body = json!({
            "data": [
                {"object": "embedding", "index": 1, "embedding": [0.0, 1.0]},
                {"object": "embedding", "index": 0, "embedding": [1.0, 0.0]}
            ],
            "usage": {"prompt_tokens": 4, "total_tokens": 4}
        });
        let (v, t) = parse_openai_embeddings(&body.to_string(), 2).expect("parses");
        assert_eq!(v, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert_eq!(t, Some(4));
        assert!(parse_openai_embeddings(&body.to_string(), 3).is_err(), "missing vector");
        assert!(parse_openai_embeddings("{}", 1).is_err());
    }

    #[test]
    fn prices_err_high_for_unknown_models() {
        assert!((embedding_price_per_1m("text-embedding-3-small") - 0.02).abs() < 1e-9);
        assert!((embedding_price_per_1m("my-embedder") - 0.13).abs() < 1e-9);
    }
}
