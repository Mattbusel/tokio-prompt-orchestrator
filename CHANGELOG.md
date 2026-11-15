# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.4.1] - 2026-11-15

### Changed

- **rmcp 0.16 to 3.10** for the optional `mcp` feature (the `mcp` stdio server binary). This takes
  the old rmcp 0.16 tree, and the RUSTSEC-2026-0189 advisory that came with it, out of the
  dependency graph, so the audit job no longer has to ignore it. `get_info` now uses rmcp 3's
  `ServerConfig` builder, and `initialize` reports `serverInfo` as `tokio-prompt-orchestrator`
  with the crate version (rmcp 0.16 filled in its own name). The nine tools, their parameters and
  their output are unchanged.
- Lockfile refresh for the security audit: quinn-proto 0.11.19, quinn-udp 0.5.16,
  rustls-webpki 0.103.17, webpki-roots and webpki-root-certs 1.0.12, tokio 1.54.1, rustix 1.1.6.
  RUSTSEC-2026-0382 in rustls-webpki 0.102 joins the existing `distributed`-only async-nats ignores.

## [2.4.0] - 2026-11-15

The proxy speaks more client protocols: Anthropic clients with tool use (Claude SDKs, Claude Code
via `ANTHROPIC_BASE_URL`), the OpenAI Responses API, and embeddings, all through the same routing,
dedup, circuit breaker and spend cap.

### Added

- **Tool use on `POST /v1/messages`.** `tools` (`name`, `description`, `input_schema`),
  `tool_choice` (`auto`, `any`, `tool`, `none`, `disable_parallel_tool_use`), assistant `tool_use`
  blocks and user `tool_result` blocks (`is_error` included) are translated for whichever upstream
  the model routes to, so an Anthropic client can drive an OpenAI-compatible model and the other way
  round. The model's tool calls come back as `tool_use` blocks with `stop_reason: "tool_use"`, and
  when streaming as `content_block_start` / `input_json_delta` / `content_block_stop` events. A
  `system` given as text blocks, `thinking` and `redacted_thinking` blocks in the history,
  `cache_control`, `thinking`, `top_k` and the `?beta=true` query are accepted (the extras are
  ignored). `usage` now also carries `cache_creation_input_tokens` and `cache_read_input_tokens`
  (0). Anthropic server tools (`web_search`, `bash`, `text_editor`, ...) get a 400 naming the tool.
- **`POST /v1/messages/count_tokens`**: `{"input_tokens": n}` for a Messages request (tool
  definitions included), counted without calling the model. Auth errors use Anthropic's shape.
- **`POST /v1/responses`**: the OpenAI Responses API, JSON or the `response.*` SSE event sequence
  (`response.created`, `response.output_item.added`, `response.output_text.delta`,
  `response.function_call_arguments.delta`, ..., `response.completed` or `response.incomplete`,
  each with a `sequence_number`). `instructions`, string or item `input` (messages with
  `input_text` / `output_text` parts, `function_call`, `function_call_output`; `reasoning` items
  skipped), function `tools`, `tool_choice`, `parallel_tool_calls`, `temperature`, `top_p`,
  `max_output_tokens` and `text.format` (`text`, `json_object`, `json_schema`) are honoured; tool
  calls come back as `function_call` output items, a cut-off reply as `status: "incomplete"`. The
  orchestrator stores nothing: `previous_response_id`, `conversation` and `background` get a 400,
  `store` is reported as `false`.
- **`POST /v1/embeddings`** with per-input caching: each input's vector is cached for
  `dedup_window_secs`, so a batch that repeats recent inputs sends only the new ones upstream, the
  same input twice in one batch is embedded once, and identical inputs in concurrent requests share
  one upstream call. `x-orchestrator-dedup` is `miss`, `cached`, `joined` or `partial`, and
  `x-orchestrator-cached-inputs` counts the inputs that needed no new call. `float` and `base64`
  encodings (the OpenAI Python SDK's default), string and token-array inputs, `dimensions` and
  `user` are supported. Calls count against the spend cap at OpenAI's embedding prices, and each
  backend has its own circuit breaker (5 upstream failures in a row, 4xx excluded, open it for
  30 s).
- **`web_api::EmbeddingsBackend`** and **`ServerConfig::embeddings`**: where the vectors come from,
  either `EmbeddingsBackend::openai_compatible(base_url, key)` (OpenAI, OpenRouter, vLLM, llama.cpp,
  Ollama, LM Studio) or `EmbeddingsBackend::from_embedder(embedder, model)` (any `Embedder`, such as
  `FastEmbedder`), with `with_default_model`, `with_billing` and `with_timeout`. A request whose
  `model` matches an `openai` or `llama_cpp` `[[models]]` route goes to that route's server
  instead. The `orchestrator` binary sets it from `--provider openai` (`ORCHESTRATOR_UPSTREAM_URL`
  or api.openai.com), `--provider llama` (`LLAMA_CPP_URL`), or the `--semantic-dedup` model.
- Integration tests against mock OpenAI, Anthropic and embeddings upstreams for all of the above:
  `tests/client_protocols_tests.rs`.

### Changed

- `ServerConfig` has a new public field, `embeddings`. Code that builds `ServerConfig` with
  `..ServerConfig::default()` (as the docs and examples do) is unaffected.
- The Anthropic endpoint's code moved from `web_api/openai_compat.rs` to its own module; behaviour
  for text requests is unchanged, apart from the extra `usage` fields above and the `ping` event
  now coming right after `message_start`.

## [2.3.0] - 2026-11-15

The deduplication and circuit breaker now work without the pipeline or the proxy: drop them into
the client stack an app already has.

### Added

- **`Dedup<K, V>`** (module `single_flight`, re-exported at the crate root with `DedupStats`): a
  standalone single-flight primitive for any `K: Hash + Eq + Clone` and `V: Clone`, LLM or not.
  Concurrent `run(key, work)` calls for the same key run `work` once and all get its result.
  If the leader is cancelled or panics, a waiter takes over and runs its own `work`, so nobody
  waits forever and no panic spreads. The work needs no `'static` or `Send` bound.
  `run_with_status` also reports whether the result was shared, `claim` is the low-level form
  (`Claim::Leader` / `Claim::Joined`) for sharing something other than what the leader returns,
  and `stats` counts leaders, coalesced calls and keys in flight. Example: `examples/single_flight.rs`.
- **`integrations::ResilienceLayer`** (feature `tower`): a tower `Layer` that adds single-flight
  dedup (`dedup_by(|req| Some(key))`, `None` to skip a request) and a circuit breaker (on by
  default: 5 consecutive failures, 30 s probe; `with_circuit_breaker`, `without_circuit_breaker`)
  to any `tower::Service`. `failure_if(|resp| ...)` counts responses such as HTTP 5xx against the
  breaker while still returning them. Errors come back as `ResilienceError` (`CircuitOpen` with a
  `retry_after`, or `Upstream(Arc<E>)`, shared with every deduplicated caller). Without dedup the
  response need not be `Clone`, so a breaker-only layer wraps HTTP services directly
  (`NoDedup`). `ResilienceService::dedup_stats` exposes the counters. Example:
  `examples/tower_resilience.rs`.
- **`integrations::ResilienceMiddleware`** (new feature `reqwest-middleware`, reqwest-middleware 0.4
  on reqwest 0.12): the same for a `reqwest_middleware::ClientWithMiddleware`. Identical in-flight
  `GET`, `HEAD` and `POST` requests (method, URL, body and headers, credentials included; tracing
  and request-id headers ignored, more with `ignore_header`) share one upstream call; each caller
  gets its own copy of the buffered response. Streaming requests (`"stream": true` or
  `Accept: text/event-stream`) and requests carrying the `SkipDedup` extension always go out on
  their own. A circuit breaker per host (or one shared breaker) counts transport errors and 5xx
  (`failure_if_status` to change) and fails fast with `CircuitOpenError`; callers that joined a
  failed request get `JoinedRequestFailed`. Example: `examples/reqwest_middleware.rs`.

## [2.2.0] - 2026-11-14

The OpenAI-compatible endpoint now honours each request instead of one model and one set of
settings per process.

### Added

- **Per-request parameters.** `POST /v1/chat/completions` sends the request's messages as written
  (assistant `tool_calls` and `tool` results included), `model`, `temperature`, `top_p`,
  `max_tokens`, `max_completion_tokens`, `stop`, `seed`, `response_format`, `tools`, `tool_choice`
  and `parallel_tool_calls` to the provider. The model's tool calls come back as
  `message.tool_calls` (a `tool_calls` delta when streaming) with `finish_reason: "tool_calls"`,
  and the provider's own `usage` is reported when it gives one. `n` must still be 1.
  `POST /v1/messages` passes `max_tokens`, `temperature`, `top_p` and `stop_sequences` through.
- `ChatRequest`, `ChatResponse`, `ChatUsage` (module `chat`) and `ModelWorker::chat`, a new trait
  method with a default implementation (flatten the messages, call `infer`), so existing workers
  keep compiling. `OpenAiWorker` sends the request to `/chat/completions` field for field,
  `AnthropicWorker` translates it to the Messages API (system prompt, `tool_use` / `tool_result`
  blocks, `tool_choice`, `stop_sequences`) and back, `LlamaCppWorker` uses llama.cpp's
  OpenAI-compatible `/v1/chat/completions`, and `LoadBalancedWorker` forwards it. A setting the
  client leaves unset falls back to the worker's own configured value, and is left out when the
  worker has none (so no temperature or reply cap is invented). The pipeline's inference stage calls
  `chat` for every request from the OpenAI and Anthropic endpoints, inside the same circuit
  breaker, timeout and retries.
- **Model routing.** A static `[[models]]` table maps model names or globs (`gpt-*`, `claude-*`,
  `llama-*`) to providers: `openai` (any OpenAI-compatible base URL, key from an environment
  variable), `anthropic`, `llama_cpp` and `echo`, with optional `model` (alias), `max_tokens` and
  `timeout_secs`. Load it with `orchestrator --models <file>` (a file of its own or a pipeline TOML;
  `config::loader` now accepts `[[models]]` and `default_route` alongside the pipeline tables) or
  `ModelRoutesConfig::from_file`, start it with `ModelRoutes::spawn` (one pipeline, queue and
  circuit breaker per route) and pass it as `web_api::ServerConfig::model_routes`. The single
  `--provider` / `--model` stays the default route for unmatched models, so existing setups behave
  as before; with `default_route = false` an unmatched model gets `400 model_not_found` in OpenAI's
  error shape, listing the configured models. `GET /v1/models` lists the default model and every
  route. Example: `examples/models.example.toml`.
- **Route timeouts.** A route's per-call timeout is its `timeout_secs`, else `--timeout`, else its
  provider's default: 60 s for an Anthropic route (`ANTHROPIC_DEFAULT_TIMEOUT_SECS`, the same as
  `--provider anthropic`), the pipeline's 30 s for the others. `ModelRoute::effective_timeout`
  gives the value and `ModelRoute::with_timeout_secs` sets it. The HTTP server waits long enough
  for the slowest route (or the default pipeline) to use every retry
  (`ModelRoutesConfig::longest_timeout` gives the slowest per-call timeout), so a route with a
  long `timeout_secs` is not answered `504` while its upstream call is still running.
- `ServerConfig` gains the public field `model_routes`. `ServerConfig` is not
  `#[non_exhaustive]`, so code that builds it with a full struct literal and no
  `..ServerConfig::default()` must add `model_routes: None` (as with fields added in earlier minor
  releases). Building it from `ServerConfig::default()` or a config file is unaffected.
- `OrchestratorError::UpstreamRejected { status, message }`: the provider refused the request with
  a 4xx (other than 401, 403, 408, 409, 429). The endpoints return it with the provider's status
  and message (`code: upstream_rejected`, `x-should-retry: false`); it is not retried and does not
  count against the circuit breaker, so a client sending bad parameters cannot open it.

### Changed

- **Dedup key.** It is now a SHA-256 over a canonical (sorted-key) encoding of the route, model,
  messages, tools, tool choice, every sampling setting and the response format, instead of the
  flattened prompt alone, so requests with different settings never share an answer.
- **Sampled requests are not cached.** A request with `temperature > 0` and no `seed` is shared
  only with identical requests still in flight; its answer is no longer reused for the dedup
  window. Requests with `temperature: 0`, no temperature, or a `seed` are cached as before.
  Semantic dedup only compares requests whose settings are identical.
- The `orchestrator` binary's Anthropic worker sends `max_tokens: 4096` when the client gives none
  (was 1024), the same default as an Anthropic route. Clients that send `max_tokens` get exactly
  that.
- **No temperature is sent when the client sends none.** Before 2.2.0 every request to the
  OpenAI-compatible endpoint was sampled at the worker's temperature: 0.7 for `--provider openai`
  (0.8 for `llama`, 1.0 for `anthropic`), whatever the client asked. Now the client's
  `temperature` is sent, and a request without one gets the provider's own default (1.0 for OpenAI
  and Anthropic, 0.8 for llama.cpp). So on `--provider openai`, clients that leave `temperature`
  out are sampled at 1.0 instead of 0.7 after upgrading; send `temperature: 0.7` to keep the old
  sampling. (Sending 0.7 by default would make every request to reasoning models such as `o3` or
  `gpt-5`, which accept only their default temperature, fail.) Those answers are still cached for
  the dedup window, as every answer was before 2.2.0; only an explicit `temperature > 0` without a
  `seed` turns caching off.
- `tool_choice` and `parallel_tool_calls` are dropped when the request has no tools (no `tools`
  field or `tools: []`, which many SDK wrappers send). OpenAI refuses either one without tools, and
  Anthropic refuses `tool_choice` without tools, so these requests used to come back as an
  `upstream_rejected` 400. `ChatRequest::to_openai_body` and the Anthropic translation leave them
  out too (`ChatRequest::has_tools`).
## [2.1.1] - 2026-11-14

### Fixed

- Review fixes on top of 2.1.0 for the rig transport and the answer-length change (tests added).

## [2.1.0] - 2026-11-14

### Added

- `integrations::OrchestratedTransport` (new feature `rig-transport`): a rig `driver::Transport`
  for rig-core 0.45 and 0.46 that wraps the transport of any rig model with the orchestrator's
  deduplication, circuit breaker and retry. Swap it in with
  `Model::new(model.wire, OrchestratedTransport::new(model.transport))`. The rig request is
  forwarded exactly as rig encoded it, so messages, tools, `temperature`, `max_tokens` and
  structured output all reach the provider (unlike `RigWorker`, which only passes the prompt).
  - Identical unary requests in flight share one upstream call (keyed on a SHA-256 of method,
    URI, credential headers and body); streams are never deduplicated. Waiting callers also share
    the first caller's failure (a 400, a 401, retries used up, the breaker open) as
    `ProviderError::Relayed`, which keeps the status, code, retry verdict and provider reply,
    instead of all sending the request again at once. Failures are never cached.
  - Retryable failures (connection errors, 5xx, 429, truncated replies) are retried, and every
    failed attempt counts against the breaker; a 400 or 401 is neither retried nor counted.
  - Defaults: in-flight dedup only; the breaker opens after 5 consecutive failed attempts (with
    3 attempts per request, two fully failed requests are enough) and probes again after 30 s;
    3 attempts with exponential backoff from 200 ms. `OrchestratedTransport::builder` changes them.
  - What rig's observation does not see: a deduplicated caller's reply is rebuilt from the first
    caller's frames and document and has no HTTP status, headers, provider request id or
    observation slot (rig gives a wrapping transport no way to read them); a reply that fails or
    stalls before it opens is not recorded on the observation; neither is a send refused by the
    open breaker. Details on the type's rustdoc.
  - Tested in `tests/integrations_tests.rs` against a counting mock transport (8 concurrent
    identical calls, 1 upstream send; 6 concurrent identical rejected calls, 1 send; 5 concurrent
    identical failing calls, one retry budget; breaker opens after K failures; retries stop at the
    maximum) and against rig's real HTTP client on a wiremock server, on rig-core 0.45 and 0.46.
    Example: `examples/rig_transport.rs`.
  - Needs Rust 1.97.1 (rig-core 0.45's minimum). With an older rig-core in your lockfile the
    feature does not build: run `cargo update -p rig-core`.
- `Deduplicator::fail_with(token, result)`: release a failed request and hand `result` to the tasks
  waiting on it, without caching it.
- `OpenAiWorker::with_key(model, key)` and `AnthropicWorker::with_key(model, key)` take the API key
  from the caller, so a key from a vault or a per-tenant config no longer needs
  `std::env::set_var` (unsafe in edition 2024 and racy in a multi-threaded runtime).
  `from_env(model)` reads the environment variable as before; `new` is now an alias of it.
- `with_http_client(reqwest::Client)` on `OpenAiWorker`, `AnthropicWorker` and `LlamaCppWorker`,
  to share the caller's connection pool, proxy and TLS settings.
- CI: an `msrv` job runs `cargo check --lib` on Rust 1.88 (the declared `rust-version`), and the
  test job runs the rig integration tests.

### Changed

- The `rig` feature (`RigWorker`) accepts rig-core 0.43 to 0.46 (`>=0.43, <0.47`, was
  `>=0.43, <0.45`). This only widens the range: a project on rig-core 0.43 or 0.44 keeps its
  version and its Rust 1.95 floor. A fresh resolve now picks rig-core 0.46, which needs
  Rust 1.97.1 (with Cargo's MSRV-aware resolver, the default under resolver 3, an older
  toolchain gets 0.44 instead). Tested:
  `integrations_tests` passes for `RigWorker` on 0.43, 0.44, 0.45 and 0.46.

### Fixed

- `OpenAiWorker` sent `max_tokens: 256` on every request unless told otherwise, so answers were
  silently cut off at 256 tokens. It now sends no `max_tokens` unless `with_max_tokens` is called,
  and the model's own limit applies. `LlamaCppWorker` had the same problem with `n_predict: 256`;
  it now omits `n_predict` unless set. The setters keep their signatures. `AnthropicWorker` still
  sends 1024 by default because the Messages API requires a value.

  The old cap also bounded cost and time, so check these if you relied on it:
  - Call `.with_max_tokens(256)` to keep the old behaviour.
  - Without a cap, one OpenAI request can cost up to the model's full output limit. The
    `orchestrator` proxy's `--max-spend` is checked after each call, so it stops the next request,
    not the expensive one.
  - The llama.cpp server's own default for `n_predict` is -1 (generate until end of text or a
    full context). A long or runaway generation that used to stop at 256 tokens can now run into
    the worker's 30 s HTTP timeout (`with_timeout` changes it) and fail instead of returning a
    short answer.

## [2.0.6] - 2026-11-14

First crates.io release of the changes below. A 2.0.5 was already on crates.io (published
2026-10-21), so the GitHub tag v2.0.5 and image 2.0.5 carry this code but crates.io has it as 2.0.6.

### Added

- Docker image `ghcr.io/mattbusel/tokio-prompt-orchestrator` (linux/amd64 and linux/arm64,
  distroless, non-root, 16 MB compressed), built by `.github/workflows/docker.yml` on each tag.
- Helm chart at `oci://ghcr.io/mattbusel/charts/tokio-prompt-orchestrator`: Secret for provider
  keys and the proxy's bearer key, `/health` probes, read-only root filesystem, optional Ingress.
- `ORCHESTRATOR_UPSTREAM_URL` sends `--provider openai` traffic to any OpenAI-compatible server
  (OpenRouter, Groq, Together, vLLM, LM Studio). It is not `OPENAI_BASE_URL` on purpose: clients
  in the same shell point that at the proxy, and reading it would make the proxy call itself.
- `bench/vs-litellm`: a reproducible benchmark against the LiteLLM proxy on a shared mock upstream.

### Fixed

- `Dockerfile` copied a binary that does not exist (`orchestrator-demo`), pinned Rust 1.79 under an
  MSRV of 1.88 and exposed the wrong port. It now builds `orchestrator` with `web-api` and serves 8080.
- `docker-compose.yml` mapped port 9090, health-checked with curl, and mounted a missing alerts file;
  `prometheus.yml` scraped Prometheus itself instead of the orchestrator's `/metrics`.

## [2.0.4] - 2026-10-19

### Changed

- The `rig` feature accepts rig-core 0.43 and 0.44 (`>=0.43, <0.45`), so a
  project already on rig-core 0.44 gets one copy of rig and `RigWorker::new`
  takes its models directly. Before, it pulled a second rig-core 0.43 and
  rig 0.44 models did not fit. Tested: `integrations_tests` passes on 0.44.

## [2.0.3] - 2026-10-18

### Fixed

- The `orchestrator` server sent one upstream call at a time: `spawn_pipeline_with`
  (and so `spawn_pipeline`) ran a single inference task, so 20 different prompts
  sent at once against a provider that takes 1.5 s took about 30 s. The inference
  stage is now a pool of workers. Measured with the openai-node SDK against a
  1.5 s mock provider: slowest of 20 concurrent distinct prompts 30.2 s in 2.0.1,
  1.5 s now. Identical requests in flight still coalesce into one upstream call.
- A hung provider never opened the circuit breaker. The per-call timeout wrapped
  the breaker call, so a timeout cancelled the call before the breaker recorded a
  failure. The timeout now runs inside the breaker: a call that times out counts
  as a failure, and five of them open it.
- `stages.inference.timeout_ms` below one second was rounded down to 0 s; it is
  now applied to the millisecond.

### Added

- `--concurrency <N>`: model calls sent upstream at the same time (default 16,
  1 to 1024). `PipelineOptions::concurrency` and `DEFAULT_INFERENCE_CONCURRENCY`
  in the library.
- `--timeout <seconds>`: longest one upstream call may take (default 30 s, 60 s
  for `anthropic`, as before; 1 to 3600). Past it the request is answered
  `504 upstream_timeout` and the breaker counts a failure.
  `PipelineOptions::inference_timeout` in the library.
- Requests refused by an open circuit breaker (`503 circuit_open`) carry
  `x-should-retry: false`, so the OpenAI and Anthropic SDKs stop retrying them
  (by default they retried twice, about 1.3 s per refusal), and `Retry-After`
  set to when the breaker will probe the provider again.
  `CircuitBreaker::retry_after_sync` returns that time.

### Changed

- `spawn_pipeline` and `spawn_pipeline_with` run up to 16 model calls at once by
  default, so outputs can arrive in a different order than requests went in
  (match them by `request_id`). Set `PipelineOptions::concurrency = 1` for the
  old one-at-a-time behaviour. `spawn_pipeline_with_config` keeps using
  `inference_workers` (default 1).
- With retries on, the inference timeout applies to each attempt rather than to
  all attempts together.

## [2.0.2] - 2026-10-17

### Security

- `Cargo.lock` moves rustls 0.23.45 to 0.23.46 for RUSTSEC-2026-0346, -0347, -0348
  and -0349 (TLS secrets in `Debug` output, ECH rejection data leak, Brotli
  certificate decompression allocation, ServerHello key-exchange downgrade). This
  reaches anyone who installs the binaries with `cargo install --locked`; library
  users get it with `cargo update -p rustls`.

### Fixed

- Builds clean on Rust 1.99 with `-D warnings`: clippy `manual_is_multiple_of`,
  the deprecated `fetch_update` (kept, because `try_update` needs a newer Rust than
  the 1.88 MSRV) and a redundant rustdoc link.

### Changed

- `repository` now points at GitHub; GitLab stays the mirror and hosts the release
  downloads, so the `cargo binstall` URLs name GitLab directly.

## [2.0.1] - 2026-10-04

### Fixed

- `cargo binstall tokio-prompt-orchestrator` works on Linux and Windows. It
  refused to install because the package declared `coordinator` and
  `validate` as always-built binaries while the release archive ships only
  `orchestrator`, and the Windows single-file asset mapped every binary to
  one path. Both tools now need the `cli` feature (with `replay`), and
  Windows releases add a `.zip` with the same layout as the Linux archive.
  Checked with `cargo binstall --dry-run` against the real release assets.

## [2.0.0] - 2026-10-04

A leaner library that works with the Rust LLM clients people already use.
Upgrading from 1.x: see "Changed (breaking)" below; most code only needs the
`otel`, `hot-reload` or `cli` feature if it used those parts.

### Added

- **Anthropic-compatible endpoint, `POST /v1/messages`.** Anthropic clients
  (the official SDKs included) work against the orchestrator by changing the
  base URL: text conversations, system prompts and streaming with the full
  event sequence, with the same dedup, circuit breaker and spend cap as the
  OpenAI endpoint. The API key may be sent as `x-api-key`. Tested with the
  official Python SDK (`messages.create`, `messages.stream`).
- **Answers grounded in your documents.** New `Retriever` trait, and
  `TantivyRetriever` (feature `tantivy`): BM25 search with English stemming
  over a folder of Markdown/text files, in memory. `spawn_pipeline_with(worker,
  PipelineOptions::with_retriever(..))` in Rust, `orchestrator --docs <folder>`
  on the command line. Retrieval failures and timeouts (2 s default) send the
  prompt without context instead of dropping it.
- **Semantic dedup with real embeddings.** New `Embedder` trait with
  `FastEmbedder` (feature `fastembed`, local ONNX models, no API key),
  `OpenAiEmbedder` and `GenaiEmbedder`. `Deduplicator::with_embedder` and
  `check_and_register_semantic` answer a reworded question from the cache;
  `ServerConfig::embedder` and `orchestrator --semantic-dedup [threshold]`
  turn it on for the OpenAI and Anthropic endpoints (`x-orchestrator-dedup:
  semantic`). A match must also pass `same_specifics` (same numbers, shared
  words in the same order), because measured embedding similarity alone puts
  "convert 10 miles to km" and "convert 10 km to miles" at 0.99. Default
  threshold 0.93 from those measurements.
- **Integrations with other Rust LLM crates**, each its own feature and each
  a `ModelWorker`, so deduplication, the circuit breaker, retries and the
  dead-letter queue sit in front of the client you already use:
  - `async-openai`: `integrations::AsyncOpenAiWorker` wraps an
    `async_openai::Client` (OpenAI, Azure, or any OpenAI-compatible server).
  - `genai`: `integrations::GenaiWorker` wraps a `genai::Client`, so one
    worker covers OpenAI, Anthropic, Gemini, Ollama, Groq, DeepSeek, xAI,
    Cohere and the other providers genai supports.
  - `rig`: `integrations::RigWorker` wraps any rig-core completion model
    (rig-core needs Rust 1.95+).
  - `tower`: `integrations::ServiceWorker` runs any `tower::Service<String>`
    as a worker (with its tower layers), and `integrations::WorkerService`
    turns a worker into a service.
  All map provider errors the same way: 401/403 become `AuthFailed` (never
  retried), 429 becomes `RateLimited` (with `Retry-After` when the client
  exposes it), everything else a retryable `Inference` error. Tested end to
  end against a mock OpenAI server, including streaming.
- `web_api::serve_pipeline(config, &handles)`: serve the HTTP API for a
  pipeline in one call, with its output receiver, dead-letter queue and
  circuit breaker wired in.
- `DeadLetterQueue::snapshot()`: read the queue without draining it.
- `otel`, `hot-reload` and `cli` features (see below).

### Changed (breaking)

- **Lighter default build: 219 crates down to 167.** OpenTelemetry moved
  behind the new `otel` feature (on in `full`). `OtelLayer` and
  `try_build_otel_layer` need it; `init_tracing` works either way and warns
  if `OTEL_EXPORTER_OTLP_ENDPOINT` is set in a build without `otel`. The OTLP
  exporter no longer compiles the gRPC stack it never used, and the unused
  `opentelemetry-http` dependency is gone.
- `config::watcher` (the file watcher, via notify) moved behind
  `hot-reload`; `HotConfig` is unaffected. The `replay` binary needs `cli`.
- **Safer `web_api::ServerConfig::default()`**: it binds `127.0.0.1`
  instead of `0.0.0.0`, and `debug_mode` is off. The old default exposed an
  unauthenticated proxy to the provider (on your API key) to the whole
  network. Set `host` explicitly to serve other machines, and pair it with
  `api_key`. The `orchestrator` binary keeps its debug endpoints only while
  bound to a loopback address.
- `OrchestratorError` is `#[non_exhaustive]`, so new error kinds can be
  added without another major release. Add a `_` arm to exhaustive matches.
- axum 0.7 -> 0.8, tower 0.4 -> 0.5, tower-http 0.5 -> 0.6. One tower version
  in the tree instead of two.
- Minimum Rust is now 1.88. The declared 1.85 was no longer true: with
  current dependencies the default build already failed on 1.85
  (`yoke-derive` needs 1.87), and `full` needs 1.88. Checked on 1.88 for the
  default build and every feature except `tantivy` (1.90), `rig` (1.95) and
  `fastembed` (1.88, checked separately).

### Fixed

- **Prompts reach the model as written.** The Retrieve stage was a
  placeholder that wrapped every prompt from the CLI, `POST /api/v1/infer`,
  the WebSocket and the MCP server in `CONTEXT: Retrieved documents for
  '...' User Query: ... Assistant:`, so providers billed for, and answered
  around, invented context. It also slept 5 ms per request to "simulate"
  retrieval. Both are gone; context now comes only from a configured
  `Retriever`.
- Pipeline stages held a `tracing` span guard across `.await`, which
  attributes other tasks' events to the wrong request in traces. Spans are
  now attached to the futures with `Instrument`.
- Semantic dedup with a caller-supplied embedding returned an empty answer
  on a hit, and its embedding store grew without limit. It now returns the
  matched answer, and the store is bounded (10,000 entries) and expires with
  the cache.
- `ServerConfig` now loads from a partial JSON/TOML config: missing fields
  take their defaults instead of failing to deserialize.
- `GET /api/v1/debug/dlq` and the MCP `dump_dlq` tool drained the
  dead-letter queue and pushed it back, which could reorder or drop entries
  that arrived meanwhile and re-sent every entry to DLQ subscribers. Both now
  read a snapshot.
- The `rest_api`, `sse_stream`, `websocket_api` and `web_api_demo` examples
  had not compiled since `start_server` gained arguments, and they passed a
  dummy output channel, so results never reached clients. They now use
  `serve_pipeline`.
- Seven test suites had stopped compiling (web API, CORS, result tracker, MCP,
  TUI, tier integration, Redis dedup) because CI only ran `cargo test --lib`.
  They are fixed, and CI now builds and runs every test target and example.
  Eighteen `self_modify` tests that run `cargo test`/`clippy`/`bench` on the
  repository itself are now `#[ignore]` (run them with `--ignored`).

## [1.6.0] - 2026-10-02

This release swaps several hand-written parts for well-known open-source
crates and adds the features those crates make easy.

### Added

- **Retries for brief provider failures.** `orchestrator --retries N` (or
  `ORCHESTRATOR_RETRIES`) retries a model call up to N times (0 to 10) when the
  provider answers 429 or 5xx or the connection fails. The wait starts at
  200 ms and doubles each time, with random jitter, capped at 10 s. A
  provider's `Retry-After` is respected; if it asks for more than the cap, the
  request fails at once instead of stalling. A bad key or a budget error is
  never retried. Off by default.
- In the library: `spawn_pipeline_with_retry(worker, InferenceRetry)`, and
  `spawn_pipeline_with_config` now applies `[resilience] retry_attempts`,
  `retry_base_ms` and `retry_max_ms`. These fields used to be parsed and then
  ignored. All attempts of one request count as one call for the circuit
  breaker, and together they must fit in the inference timeout.
- New metric `orchestrator_inference_retries_total`.
- **Exact token counts for OpenAI models** with
  [tiktoken-rs](https://crates.io/crates/tiktoken-rs) (new `tiktoken`
  feature, turned on by `web-api`). The OpenAI proxy's `usage` is now counted
  with the model's own tokenizer, including message framing, so it matches
  the provider's bill, and the spend cap uses the same numbers.
  `CostEstimator::estimate_cost` counts exactly too. Other models keep the
  estimate. New functions: `token_counter::exact_token_count`,
  `exact_chat_prompt_tokens`, `count_for_model` and
  `TokenizerFamily::from_model`.
- `Deduplicator::with_max_entries`, `Deduplicator::cache_duration` and
  `enhanced::dedup::DEFAULT_DEDUP_MAX_ENTRIES`.

### Changed

- **The deduplicator now has a size limit.** It stores its entries in a
  [moka](https://crates.io/crates/moka) cache (100,000 keys by default)
  instead of an unbounded map swept by a background task every 60 s, so a
  flood of different prompts can no longer grow memory without limit.
  Expired answers are never served, the sweeper task is gone, and
  `Deduplicator::new` no longer has to be called inside a Tokio runtime.
  `shutdown()` and `signal_shutdown()` remain and do nothing.
- The deduplicator is safer under races: a finished or cancelled call only
  clears its own entry, never a newer one for the same key, and waiters are
  always woken even if the entry was evicted.
- `dedup_key` uses a 128-bit SHA-256 prefix instead of a 64-bit FNV hash, so
  two different prompts cannot share a cached answer by a hash collision.
  Keys change format (`dedup:g:` plus 32 hex characters), which only matters
  if you stored them.
- `RetryPolicy`, `retry_if` and `retry_inference` run on
  [backon](https://crates.io/crates/backon) instead of hand-written loops.
  Same API and same delays. `retry_inference` no longer retries
  `AuthFailed`, `ConfigError` or `Other` errors (it used to retry everything
  except `BudgetExceeded`).
- [prometheus](https://crates.io/crates/prometheus) 0.13 to 0.14, built
  without its protobuf support. `/metrics` output is unchanged, and the
  protobuf dependency with advisory RUSTSEC-2024-0437 is gone (so is the
  `deny.toml` ignore for it).
- `lazy_static` replaced by `std::sync::LazyLock`.

## [1.5.0] - 2026-09-30

### Added

- OpenAI-compatible API: `POST /v1/chat/completions` (JSON, or Server-Sent
  Events in `chat.completion.chunk` format ending with `data: [DONE]` when
  `"stream": true`) and `GET /v1/models`. Point any OpenAI client at
  `http://127.0.0.1:8080/v1` (or set `OPENAI_BASE_URL`) and its requests go
  through the pipeline: bounded queues, circuit breaker, timeouts and the
  dead-letter queue. Verified with the official `openai` Python package and
  `curl`; see "Drop-in OpenAI proxy" in the README and docs/REFERENCE.md.
- Deduplication on that endpoint: identical conversations share one upstream
  call while it is in flight, and the answer is reused for
  `ORCHESTRATOR_DEDUP_SECS` (default 300). Response header
  `x-orchestrator-dedup: miss | joined | cached`.
- Errors in OpenAI's JSON shape with matching status codes: 503 while the
  circuit breaker is open, 429 at the spend cap or when a queue is full, 502
  for a failed provider call, 504 on timeout, 401 for a bad key.
- `ORCHESTRATOR_API_KEY` turns on bearer auth for the HTTP API (`API_KEY`
  still works).
- `DeadLetterQueue::subscribe()` to be told about dropped requests as they
  happen, `PromptRequest::is_raw_prompt()` and `META_PROMPT_MODE` (send the
  input to the model without the placeholder context template), and
  `CostEstimator::cost_for_tokens()`.
- `ServerConfig` fields `provider`, `model`, `max_spend_usd`,
  `dedup_window_secs` and `api_key` (all with defaults).

### Changed

- `--max-spend` now works with the web API: the OpenAI endpoint records an
  estimated cost per upstream call and answers 429 `insufficient_quota` at the
  cap, and the server keeps running. Before, nothing recorded cost, so the cap
  never triggered. With `--no-web` it still exits at the cap.
- Requests the pipeline drops now show as `failed` with the reason on
  `GET /api/v1/status/<id>` and `/api/v1/result/<id>` right away, instead of
  staying `processing` until the timeout.
- The Linux release binary is built with the `web-api` feature. Earlier Linux
  builds (1.4.x) had no HTTP server; the Windows build already had it. The
  release job now also starts the binary and checks `/v1/models` and a chat
  completion before publishing.

### Fixed

- `cargo test --lib --features web-api` compiles again (a stale test helper
  in `web_api.rs`).

## [1.4.3] - 2026-09-28

### Changed

- README cut from 76 KB to about 10 KB: what it does, a direct Windows download,
  an animated "how it works" diagram drawn from `src/stages.rs`, three real
  examples, then links. Everything else moved, unchanged, to `docs/GUIDE.md`,
  `docs/REFERENCE.md` and `docs/MODULES.md`.
- `orchestrator --help` and the first-run welcome describe what the binary
  really does (bounded queue, circuit breaker, timeouts, dead-letter queue);
  they no longer claim deduplication and retries, which the server does not add.
- Terminal logs respect `NO_COLOR`; the log file never gets colour codes.
- Releases also carry `orchestrator-windows-x64.exe`, a stable name for the
  README's download link.
- Removed the stale `releases/orchestrator.exe` from the source tree.

### Added

- `examples/quickstart.rs`: the README's library example, runnable.

## [1.4.2] - 2026-09-25

### Added

- One-line installs: `install.sh` (Linux, macOS) and `install.ps1` (Windows)
  download the release archive, verify it against `SHA256SUMS.txt` and put
  `orchestrator` on your PATH. Also Homebrew (`mattbusel/tap`), Scoop
  (`mattbusel` bucket) and `cargo binstall` metadata.
- Releases now ship `SHA256SUMS.txt` and include the `replay`, `coordinator`
  and `validate` binaries next to `orchestrator`.
- `orchestrator --help` has an EXAMPLES section (echo mode, curl, a real model).

### Changed

- `--provider` with an unknown name now stops with the list of valid providers
  instead of silently falling back to echo.
- A missing API key error lists every way to fix it, including the
  `--provider echo` mode that needs no key.
- README opens with what it does, a recorded demo GIF, an install table and
  three steps; the docs.rs front page opens with a runnable example and links
  to the main types instead of repeating the whole README.

## [1.4.1] - 2026-09-25

### Fixed

- `orchestrator` built with `web-api` (the release binaries) exited right after
  printing its banner: the terminal prompt found the output channel already
  taken, returned, and ended the program. The pipeline output is now split by
  request id so the terminal prompt and the HTTP API both work; if stdin
  closes, the server keeps running.
- The startup banner and `--help` advertised `POST /v1/prompt`, which does not
  exist, and an HTTP URL for Claude Desktop. They now list the real routes
  (`/api/v1/infer`, `/api/v1/result/<id>`, `/v1/stream`, `/health`) and point
  Claude Desktop at the stdio `mcp` binary.
- The banner box is padded by character count, so its right border lines up,
  and it says when the web API is off because the `web-api` feature is missing.
- A port already in use now prints a clear message instead of exiting silently.

### Changed

- README and project site rebuilt around real output of `llm_pipeline`;
  the Pages workflow publishes the site with the API docs under `/api/` and
  keeps the benchmark history.

## [1.4.0] - 2026-09-25

Version 1.3.0 was never published to crates.io, so this release also carries
everything listed under 1.3.0 below.

### Added

- `examples/llm_pipeline.rs`: an end-to-end pipeline example that runs with no
  API key (mock model) or against Anthropic/OpenAI with `PROVIDER=`. Shows
  request deduplication (12 requests, 3 model calls) and the circuit breaker
  plus dead-letter queue during a simulated provider outage.
- `enhanced::retry_if` is re-exported (its doc example referenced it).
- Release workflow: pushing a `v*` tag builds `orchestrator` for Linux,
  macOS (arm64, x86_64) and Windows and attaches the archives to the release.
- CI `test` job: integration tests, doctests, example builds and a run of the
  `llm_pipeline` example.

### Fixed

- `web-api` feature did not compile (stale `rate_limiter::RateLimiter` type).
  `GET /api/v1/rate-limiter/stats` now returns per-model throttled counts from
  `RateLimiterRegistry`.
- `priority_queue`: aging thresholds were applied in reverse order
  (Background waited for the High threshold); promoted items now keep their
  enqueue order in the destination level.
- `context_mgr`: `SummarizeOldest` left the context over budget because the
  placeholder message was not counted.
- `hot_config`: `set`/`get` panicked when called inside a Tokio runtime.
- `config::watcher`: the reload loop blocked a Tokio worker thread (and hung
  current-thread runtimes); the debounce dropped the last write of a burst.
- `multi_modal`: serializing a `ContentPart::Text` always failed. Text parts
  now serialize as `{"type":"text","text":"..."}`.
- `prompt_template`: `{{ x | default:y }}` errored when `x` was unset;
  `truncate` could panic on multi-byte text.
- `prompt_versioning::blame` returned a version for out-of-range lines.
- `prompt_optimizer`: filler removal missed words before punctuation and
  mangled words that contain a filler (for example "displease").
- `job_scheduler`: intervals shorter than 100 ms fired late.
- `intent_classifier`: "Analyse X" was classified as a command.
- `retry_policy::retry_async` panicked with `max_attempts = 0`.
- `orchestrator` binary: `--provider` on the command line was ignored unless a
  saved config existed. `cargo run` now picks the `orchestrator` binary
  (`default-run`).
- README: the quick start used a nonexistent `--worker` flag; code fragments
  are marked so `cargo test --doc` passes; broken intra-doc links fixed.
- Clippy (`-D warnings`, Rust 1.98) and rustdoc (`-D warnings`) are clean;
  `Cargo.lock` updated for patched `h2`, `rustls`, `quinn-proto` and
  `crossbeam-epoch`.

### Changed

- Published package excludes `releases/` (a prebuilt binary), editor and hook
  files.

## [1.3.0] - 2026-03-22

### Added

- **Multi-provider cascade fallback** (`src/routing/cascade.rs`): new
  `ProviderCascade` type that chains an ordered list of `(WorkerKind,
  CircuitBreakerConfig)` pairs and tries primary → secondary → tertiary on
  failure. Skips providers whose circuit breaker is currently open. Records
  per-provider latency and success-rate atomics. Exposes Prometheus counters
  `cascade_attempts_total{provider}` and `cascade_failures_total{provider}`.
  Returns `CascadeResult { provider_used, attempts, total_latency_ms, value }`.
  Re-exported from `routing::cascade` and `routing`.

- **Deadline-aware priority queue** (`src/enhanced/priority.rs`): new
  `PriorityQueue::pop_with_deadline_check` method that skips requests whose
  `deadline` has already passed. Each expired request increments the new
  `expired_total: Arc<AtomicUsize>` field and emits a `tracing::warn!`.
  `QueueStats` gains a matching `expired_total: usize` field. The existing
  `pop` method is unchanged for backward compatibility.

- **DLQ replay scheduler** (`src/enhanced/dlq_replay.rs`): new
  `DlqReplayScheduler` type that wraps dead-letter queue entries and adds
  `replay_all(sender)`, `replay_by_session(session_id, sender)`, and
  `age_out(max_age)`. Uses exponential backoff (2^attempts seconds, capped at
  60 s) between retry sends. Exposes Prometheus counters `dlq_replayed_total`
  and `dlq_aged_out_total`. Re-exported from `enhanced`.

- **Provider health dashboard** (`src/routing/health.rs`): new
  `ProviderHealthBuilder` and `ProviderHealthSnapshot` types that aggregate
  per-provider health data — `last_success_at`, `last_failure_at`,
  `consecutive_failures`, `success_rate_1h`, `p50_latency_ms`,
  `p95_latency_ms` — and expose a `to_json()` method for REST endpoint
  integration. Re-exported from `routing`.

### Changed

- `Cargo.toml`: version bumped from `1.2.0` to `1.3.0`.
- `src/enhanced.rs`: added `pub mod dlq_replay` and re-exports for
  `DlqReplayScheduler`, `ReplayEntry`, and `QueueStats`.
- `src/routing/mod.rs`: added `pub mod cascade` and `pub mod health` with
  corresponding re-exports.

## [Unreleased]

### Added

- Production-readiness pass: doc comments added to all public API items in
  `lib.rs`, `worker.rs`, `stages.rs`, `config/mod.rs`, and `enhanced.rs`.
- CI workflow updated to run `cargo fmt --check`, `cargo clippy -- -D warnings`,
  `cargo test`, and `cargo doc --no-deps` on `ubuntu-latest` with the stable
  toolchain, plus cross-platform jobs on `windows-latest` and `macos-latest`.
- CI now triggers on both `master` and `main` branch pushes and pull requests.
- README rewritten with architecture ASCII diagram, quickstart async example,
  API overview table, full configuration reference, feature flags table, and
  contributing and license sections.
- Full `///` doc comments added to all five worker constructors (`OpenAiWorker::new`,
  `AnthropicWorker::new`, `LlamaCppWorker::new`, `VllmWorker::new`,
  `EchoWorker::new`), each with environment-variable table, `# Errors`, and
  `# Examples` sections.
- `## Timeout Semantics` section added to `src/stages.rs` module doc, covering
  `DEFAULT_INFERENCE_TIMEOUT_SECS`, per-request deadlines, circuit-breaker
  timeout independence, stage-level timeouts, and deadline-vs-stage-timeout
  interaction.
- Module-level `//!` doc comment in `src/config/validation.rs` now includes a
  complete validation-rule table. `validate()` gains a `# Errors` section
  listing every `ConfigError::InvalidField` variant that can be returned.
- `POST /api/v1/batch` and `GET /api/v1/batch/:job_id/progress` endpoints
  documented in `src/web_api.rs` with request/response JSON schemas and
  progress-polling instructions. `WEB_API.md` updated with curl examples.
- `## Configuration` subsections added to `README.md` covering timeout TOML
  snippets, circuit-breaker TOML, and rate-limit TOML.
- `## Troubleshooting` section added to `CONTRIBUTING.md` covering DLQ
  inspection, circuit-breaker state interpretation, replaying failed requests,
  and common configuration mistakes.
- Inline `// NOTE: …` comments added at every `parking_lot::Mutex` / sync
  `std::sync::Mutex` lock site inside async contexts, explaining why a sync
  lock is acceptable (short critical section, no `.await` inside the guard).
- `cargo doc --no-deps -D warnings` step added to CI to prevent documentation
  regressions.
- Module-level doc comment for `enhanced` with feature-table overview of all
  six sub-modules.
- `gather()` function in `metrics` now has a proper `///` doc comment.
- CI: added `--no-default-features` build and test steps to catch compilation
  breakage when all optional features are disabled.
- CI: `cargo doc` step now uses `--all-features` so feature-gated public API
  is always verified.
- CI: fixed bench step to reference the correct bench target name.
- Tests: `tier_integration_tests.rs` — independent integration tests for each
  of the four self-improvement tiers.
- Tests: four additional distributed tests covering quorum loss and leader
  election edge cases.
- Docstrings: `SessionId::new`, `SessionId::as_str`, `PromptRequest` fields,
  and `PipelineStage` variants now have `///` doc comments.

### Fixed

- `Deduplicator::check_and_register` now uses DashMap's atomic `entry()` API
  to eliminate the TOCTOU race condition that caused multiple concurrent callers
  to each receive `DeduplicationResult::New` for the same key. Under high
  concurrency (50 goroutines hitting the same key simultaneously), only one
  caller now receives `New`; all others correctly receive `InProgress`.
- Added `atomic_register_new` helper used after an expired entry is removed, so
  re-registration is also race-free.
- `test_concurrent_duplicate_requests_dedup_stress` in `tests/chaos_tests.rs`
  now reliably passes with the corrected deduplicator (was asserting `== 1` but
  getting 7 due to the pre-existing race).
- `LlamaCppWorker::infer` now returns an empty `Vec` for empty content instead
  of `vec![""]`, aligning with the test contract documented in `worker_extra_tests.rs`.
- `OpenAiWorker::infer` and `AnthropicWorker::infer` now return an empty `Vec`
  for whitespace-only responses instead of a single blank-string token.
- `try_build_otel_layer` replaced `eprintln!` with `tracing::warn!` so all
  diagnostic output goes through the structured logging layer.
- Five test assertions in `worker.rs` corrected: `vec!["hello", "world", "response"]`
  changed to `vec!["hello world response"]` to match the single-token return
  contract of OpenAI, Anthropic, LlamaCpp, and vLLM workers.
- Misplaced doc lines in `metrics.rs` and `stages.rs` now correctly attributed
  to their respective functions.
- `src/bin/self_improve.rs` and `src/main.rs` replaced `.expect()` calls with
  graceful error handlers that log via `tracing::error!` and exit with code 1.

## [1.0.0] - 2026-03-18

### Added

- `deny.toml` with an explicit license allow-list (MIT, Apache-2.0, ISC,
  Unicode-DFS-2016, BSD-2-Clause, BSD-3-Clause) and `cargo-deny` integrated
  into the CI `audit` job.
- Doc comment on `spawn_pipeline_with_config` in `stages.rs` (was absent,
  leaving a public API surface without documentation).
- `# Errors` section added to `metrics::init_metrics` doc comment.
- CI: `cargo deny check` step added to the `audit` job alongside the existing
  `rustsec/audit-check` action.

### Fixed

- Misplaced doc lines in `stages.rs`: the two doc sentences that belonged to
  `validated_channel_size` (a private helper) were incorrectly appended to the
  preceding public function's `# Panics` block. Both items are now correctly
  attributed.

### Changed

- Version bumped from `0.1.0` to `1.0.0` -- the public API is stable.

---

## [0.1.0] - 2025-01-01

### Added

- Five-stage async pipeline: RAG, Assemble, Inference, Post-process, Stream.
- Bounded `tokio::sync::mpsc` channels with configurable capacity and graceful load-shedding via `send_with_shed`.
- `DeadLetterQueue` ring-buffer for inspecting and replaying shed requests.
- `ModelWorker` trait with five production implementations: `EchoWorker`, `OpenAiWorker`, `AnthropicWorker`, `LlamaCppWorker`, `VllmWorker`.
- `LoadBalancedWorker` for round-robin distribution across multiple backends.
- `CircuitBreaker` (open/half-open/closed) with configurable failure threshold, timeout, and success-rate probe.
- `Deduplicator` — collapses identical in-flight prompts into a single inference call; unblocks all waiters on completion.
- `RetryPolicy` with exponential back-off, jitter, and per-attempt timeout.
- `CacheLayer` — in-process LRU cache with TTL for inference results.
- `RateLimiter` — token-bucket rate limiter configurable via `RateLimitConfig`.
- `PriorityQueue` with four-level priority scheduling.
- `ModelRouter` — complexity-scored routing between local llama.cpp and cloud APIs; adaptive threshold tuning.
- `PipelineConfig` — declarative TOML configuration with `deny_unknown_fields`, hot-reload support, and optional JSON Schema export.
- Prometheus metrics: 18 counters, histograms, and gauges covering every pipeline stage and resilience primitive.
- OpenTelemetry tracing integration with OTLP/Jaeger export and graceful fallback when no collector is present.
- `init_tracing()` with `RUST_LOG_FORMAT=json` support for structured log aggregation pipelines.
- `SessionId` with FNV-1a based `shard_session` for stable session affinity across process restarts.
- `PromptRequest::with_deadline` for per-request time-to-live enforcement.
- `PipelineStage` enum with `Display` for consistent metric/log labelling.
- Coordination module: TOML-driven agent fleet management with atomic filesystem-lock task claiming.
- `AgentSpawner`, `TaskQueue`, and `AgentMonitor` for zero-panic agent lifecycle management.
- Self-tuning stack (`self-tune` feature): PID controllers, telemetry bus, anomaly detection (Z-score + CUSUM), snapshot store.
- Self-modify stack (`self-modify` feature): task generation, validation gate (cargo test + clippy), agent memory.
- Intelligence layer (`intelligence` feature): `LearnedRouter` (epsilon-greedy bandit), `Autoscaler`, `FeedbackCollector`, `QualityEstimator`, `PromptOptimizer`, `SemanticDedup`.
- Evolution module (`evolution` feature): A/B experiments, snapshot rollback, transfer learning.
- Distributed mode (`distributed` feature): NATS pub/sub, Redis-based cross-node dedup, leader election with TTL renewal.
- TUI terminal dashboard (`tui` feature) built with ratatui: live stage latency, circuit-breaker status, dedup savings, sparklines.
- Web API (`web-api` feature): REST, WebSocket, and SSE streaming endpoints via axum.
- Metrics HTTP server (`metrics-server` feature): Prometheus scrape endpoint.
- MCP server (`mcp` feature): `infer`, `pipeline_status`, `batch_infer`, and `configure_pipeline` tools callable from Claude Desktop and Claude Code.
- CI: build, clippy `-D warnings`, rustfmt check, tests (default + all-features), MSRV 1.85, publish dry-run, benchmark regression check, `cargo audit` security scan.
- Clippy lints `unwrap_used` and `expect_used` set to `deny` in `Cargo.toml`.
- `#![forbid(unsafe_code)]` enforced across all production code paths.

[Unreleased]: https://github.com/Mattbusel/tokio-prompt-orchestrator/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Mattbusel/tokio-prompt-orchestrator/releases/tag/v0.1.0
