# Configuration Reference

`PipelineConfig` is a TOML-deserialisable struct that controls every aspect of
the pipeline.  Load it with `config::loader::load_config("pipeline.toml")` and
pass it to `spawn_pipeline_with_config`.

All sections are required unless marked **optional**.

---

## `[pipeline]`

Pipeline identity metadata.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | `String` | yes | Human-readable name, e.g. `"production"` |
| `version` | `String` | yes | Semantic version string, e.g. `"1.0"` |
| `description` | `String` | no | Optional free-text description |

```toml
[pipeline]
name        = "production"
version     = "1.0"
description = "Production pipeline — GPT-4o"
```

---

## `[stages.rag]`

Retrieval-augmented generation stage.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Disable to skip the stage |
| `timeout_ms` | `u64` | `5000` | Max ms to wait for retrieval |
| `max_context_tokens` | `usize` | `2048` | Max tokens prepended from retrieval |
| `channel_capacity` | `usize?` | `512` | Buffer depth for RAG → Assemble channel |

---

## `[stages.assemble]`

Prompt assembly stage.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Disable to pass-through |
| `channel_capacity` | `usize` | `512` | Buffer depth |

---

## `[stages.inference]`

Inference stage — the only required stage fields.

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `worker` | `WorkerKind` | yes | Backend: `"open_ai"`, `"anthropic"`, `"llama_cpp"`, `"vllm"`, `"echo"` |
| `model` | `String` | yes | Model name, e.g. `"gpt-4o"` |
| `max_tokens` | `u32?` | no | Max generation tokens |
| `temperature` | `f32?` | no | Sampling temperature |
| `timeout_ms` | `u64?` | no | Per-request inference timeout |

```toml
[stages.inference]
worker      = "open_ai"
model       = "gpt-4o"
max_tokens  = 1024
temperature = 0.7
timeout_ms  = 30000
```

---

## `[stages.post_process]`

Post-processing stage.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Disable to skip |
| `channel_capacity` | `usize` | `512` | Buffer depth |

---

## `[stages.stream]`

Output streaming stage.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Disable to skip |
| `channel_capacity` | `usize` | `512` | Buffer depth |

---

## `[resilience]`

Retry and circuit-breaker settings.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `retry_attempts` | `u32` | — | Total attempts (includes first try) |
| `retry_base_ms` | `u64` | `100` | Initial exponential back-off delay (ms) |
| `retry_max_ms` | `u64` | `5000` | Max back-off delay cap (ms) |
| `circuit_breaker_threshold` | `u32` | — | Failures before circuit opens |
| `circuit_breaker_timeout_s` | `u64` | — | Seconds open before probing |
| `circuit_breaker_success_rate` | `f64` | — | Success rate to close circuit (0.0–1.0) |

```toml
[resilience]
retry_attempts              = 3
retry_base_ms               = 100
retry_max_ms                = 5000
circuit_breaker_threshold   = 5
circuit_breaker_timeout_s   = 60
circuit_breaker_success_rate = 0.8
```

---

## `[rate_limits]`

**Optional section** (defaults to disabled).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `false` | Enable/disable rate limiting |
| `requests_per_second` | `u32` | `100` | Sustained request rate |
| `burst_capacity` | `u32` | `20` | Extra requests allowed in a burst |

```toml
[rate_limits]
enabled              = true
requests_per_second  = 50
burst_capacity       = 10
```

---

## `[deduplication]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | — | Enable/disable |
| `window_s` | `u64` | `300` | Seconds to cache completed results |
| `max_entries` | `usize` | `10000` | Max in-memory dedup entries |

---

## `[observability]`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `log_format` | `"pretty"` or `"json"` | — | Log output format |
| `metrics_port` | `u16?` | `None` | Prometheus scrape port; `None` = disabled |
| `tracing_endpoint` | `String?` | `None` | OTel collector URL; `None` = disabled |

```toml
[observability]
log_format       = "json"
metrics_port     = 9090
tracing_endpoint = "http://jaeger:4318"
```

---

## `[channel_sizes]`

**Optional section** — override inter-stage channel capacities.

| Field | Type | Compiled default | Description |
|-------|------|-----------------|-------------|
| `rag_to_assemble` | `usize?` | 512 | RAG → Assemble |
| `assemble_to_inference` | `usize?` | 512 | Assemble → Inference |
| `inference_to_post` | `usize?` | 1024 | Inference → Post |
| `post_to_stream` | `usize?` | 512 | Post → Stream |
| `stream_output` | `usize?` | 256 | Stream output ring |

```toml
[channel_sizes]
inference_to_post = 2048   # widen the inference output buffer
```

---

## Full example

```toml
[pipeline]
name    = "production"
version = "1.0"

[stages.rag]
enabled            = true
timeout_ms         = 3000
max_context_tokens = 1024

[stages.assemble]
enabled = true

[stages.inference]
worker      = "open_ai"
model       = "gpt-4o"
max_tokens  = 2048
temperature = 0.7
timeout_ms  = 30000

[stages.post_process]
enabled = true

[stages.stream]
enabled = true

[resilience]
retry_attempts               = 3
circuit_breaker_threshold    = 5
circuit_breaker_timeout_s    = 60
circuit_breaker_success_rate = 0.8

[rate_limits]
enabled             = true
requests_per_second = 100
burst_capacity      = 20

[deduplication]
enabled   = true
window_s  = 300

[observability]
log_format   = "json"
metrics_port = 9090
```
