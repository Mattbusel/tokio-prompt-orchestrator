# Moving from Helicone AI Gateway to tokio-prompt-orchestrator

[Helicone AI Gateway](https://github.com/Helicone/ai-gateway) is a Rust gateway with an
OpenAI-compatible API in front of many providers. Its repository has had no push since
2025-11-21 and its most reacted open issue asks whether it is abandoned
([Helicone/ai-gateway#298](https://github.com/Helicone/ai-gateway/issues/298)). This page is
for people running it self-hosted who want a maintained Rust gateway with the same shape:
one binary, an OpenAI-compatible endpoint, a config file, Docker.

tokio-prompt-orchestrator (TPO) is maintained by the author of this page. It is not a fork of
Helicone and does not do everything Helicone does; the last section lists the gaps. Everything
below was run against the 2.4.2 release binary.

## What maps to what

| Helicone AI Gateway | tokio-prompt-orchestrator |
|---|---|
| `npx @helicone/ai-gateway@latest`, Docker `helicone/ai-gateway` | release binary `orchestrator` (Linux, macOS, Windows), `cargo binstall tokio-prompt-orchestrator`, Docker `ghcr.io/mattbusel/tokio-prompt-orchestrator`, `brew install mattbusel/tap/tokio-prompt-orchestrator`, `scoop bucket add mattbusel https://github.com/Mattbusel/scoop-bucket` then `scoop install tokio-prompt-orchestrator` |
| `config.yaml` with `routers:` | flags, plus a TOML file of `[[models]]` routes passed with `--models` |
| base URL `http://localhost:8080/ai` or `/router/<name>` | base URL `http://localhost:8080/v1` (one router per process) |
| `POST /ai/chat/completions` | `POST /v1/chat/completions` (JSON or SSE), also `/v1/responses`, `/v1/embeddings`, `/v1/models` |
| Anthropic-format clients | `POST /v1/messages` and `/v1/messages/count_tokens` for Anthropic SDKs and Claude Code |
| model names `openai/gpt-4o-mini`, `anthropic/claude-...` | any name you route: exact names or globs (`gpt-*`); a route can map `openai/gpt-4o-mini` to the upstream name `gpt-4o-mini` (example below) |
| provider keys in `.env` (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) | the same variables; a route can name another one with `api_key_env` |
| `HELICONE_CONTROL_PLANE_API_KEY` (gateway auth) | `ORCHESTRATOR_API_KEY`: clients send `Authorization: Bearer <key>` (OpenAI) or `x-api-key` (Anthropic); without it requests get 401 |
| `load-balance` strategies (`model-latency`, `latency`, weighted) | one upstream per route; each route has its own queue, circuit breaker, timeout and retries, so one failing provider only trips its own breaker |
| `rate-limit` (`per-api-key`, global) | `--concurrency` (calls in flight upstream) and `--max-spend <USD>` (the OpenAI endpoint answers 429 once the spend cap is reached); see the gaps below for per-key limits |
| `cache` (in-memory, Redis, S3) with `directive` | identical requests are answered once: calls in flight are shared, and a completed answer is reused for `ORCHESTRATOR_DEDUP_SECS` seconds (default 300); in memory only |
| retries | `--retries N` (429, 5xx and network errors, with backoff and `Retry-After`) and `--timeout <seconds>` per upstream call |
| Helicone observability, OpenTelemetry | Prometheus `/metrics` and `/health` on the same port; Grafana dashboard and Prometheus config in the repository |

## Step 1: run it

```bash
# no key, no network: an echo model, to check your clients first
orchestrator --provider echo

# a real provider
export OPENAI_API_KEY=sk-...
orchestrator --provider openai --model gpt-4o-mini --retries 3 --timeout 60
```

The server listens on `127.0.0.1:8080` (`--host 0.0.0.0` and `--port` to change it). `GET /health`
answers 200 when the pipeline is up.

## Step 2: translate your router config

A Helicone router like this one:

```yaml
routers:
  my-router:
    load-balance:
      chat:
        strategy: model-latency
        models:
          - openai/gpt-4o-mini
          - anthropic/claude-sonnet-4-6
```

becomes a routes file. Keep the provider-prefixed names your clients already send, and give
each route the name the provider expects upstream:

```toml
# models.toml
default_route = false        # a model that matches no route gets a 400 listing the routes

[[models]]
name = "openai/gpt-4o-mini"
provider = "openai"
model = "gpt-4o-mini"        # the name sent upstream

[[models]]
name = "anthropic/claude-sonnet-4-6"
provider = "anthropic"
model = "claude-sonnet-4-6"
max_tokens = 8192            # sent when the client gives no max_tokens

# Any OpenAI-compatible server: OpenRouter, Together, Fireworks, Groq, vLLM, LiteLLM ...
[[models]]
name = "openrouter/*"
provider = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
```

```bash
orchestrator --provider openai --models models.toml --retries 3
```

Matching is case-insensitive; exact names are tried first, then globs in file order. A glob
route sends the client's model name upstream unchanged, so for prefixed names use exact routes
with `model`, as above. Providers a route can name: `openai` (any OpenAI-compatible server
with `base_url`), `anthropic`, `llama_cpp`, `echo`.

## Step 3: point your clients at it

```diff
 from openai import OpenAI
 client = OpenAI(
-    base_url="http://localhost:8080/router/my-router",
-    api_key=os.environ["HELICONE_API_KEY"],
+    base_url="http://localhost:8080/v1",
+    api_key=os.environ["ORCHESTRATOR_API_KEY"],   # any string when ORCHESTRATOR_API_KEY is unset
 )
 client.chat.completions.create(model="openai/gpt-4o-mini", messages=[...])
```

Anthropic SDKs and Claude Code: `ANTHROPIC_BASE_URL=http://localhost:8080` and the same key as
`ANTHROPIC_API_KEY`.

## Step 4: Docker Compose

```yaml
services:
  gateway:
    image: ghcr.io/mattbusel/tokio-prompt-orchestrator:2.4.2
    command: ["--provider", "openai", "--models", "/config/models.toml", "--retries", "3"]
    ports:
      - "8080:8080"
    environment:
      - OPENAI_API_KEY
      - ANTHROPIC_API_KEY
      - OPENROUTER_API_KEY
      - ORCHESTRATOR_API_KEY
      - ORCHESTRATOR_DEDUP_SECS=300
    volumes:
      - ./models.toml:/config/models.toml:ro
```

The image's entrypoint already binds `0.0.0.0:8080`. `docker-compose.yml` in this repository
adds Prometheus, Grafana and Jaeger next to it.

## Checks that it works

With `ORCHESTRATOR_API_KEY=tpo-test-key orchestrator --provider echo --models models.toml`
(routes pointing at `echo`), on the 2.4.2 Windows release binary:

| Request | Result |
|---|---|
| `GET /v1/models` without a key | 401 |
| `GET /v1/models` with `Authorization: Bearer tpo-test-key` | the routes, as OpenAI model objects |
| `POST /v1/chat/completions` with `"model": "openai/gpt-4o-mini"` | answered by the route; `model` in the response is the upstream name `gpt-4o-mini` |
| `POST /v1/chat/completions` with an unrouted model and `default_route = false` | 400, `model_not_found`, listing the configured models |
| `POST /v1/messages` with `x-api-key` | an Anthropic-format message |
| the same chat request twice | the second answered from the dedup cache (16 ms, then 1 ms) |
| `GET /metrics` with the key | Prometheus counters (`orchestrator_cache_hits_total`, ...) |

## What TPO does not do (yet)

Plan around these before switching:

- **Per-key budgets and rate limits.** There is one gateway key (`ORCHESTRATOR_API_KEY`) and one
  global spend cap (`--max-spend`). Helicone's `rate-limit.per-api-key` and dollar limits per
  user or team have no equivalent today.
- **Load-balancing strategies.** No latency-based (P2C, PeakEWMA) or weighted routing across
  providers for one model name. Each name goes to one route.
- **Shared caches.** Dedup is in memory, per process. No Redis or S3 backend, no cache
  directives per request.
- **Built-in provider catalog.** Helicone ships 20+ providers by name. TPO has OpenAI-compatible
  (any `base_url`), Anthropic and llama.cpp; Bedrock and Vertex AI need an OpenAI-compatible
  proxy in front of them.
- **Helicone's hosted dashboard and control plane.** Monitoring is Prometheus and Grafana.
- **One router per process.** Helicone's `/router/<name>` paths become one TPO process (or one
  port) per router.

Issues and questions: https://github.com/Mattbusel/tokio-prompt-orchestrator/issues
