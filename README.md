# tokio-prompt-orchestrator: the proxy in front of your LLM should not be the slow part

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

[![crates.io](https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg)](https://crates.io/crates/tokio-prompt-orchestrator) [![docs.rs](https://img.shields.io/docsrs/tokio-prompt-orchestrator)](https://docs.rs/tokio-prompt-orchestrator) [![MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![CI](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml)

Python made the LLM gateway easy to write. It did not make it cheap to run. On the same 4 CPUs and the same mock upstream, the LiteLLM proxy added 7.8 ms per request, idled at 347 MB and topped out at 162 req/s; this one added 0.5 ms, idled at 55 MB and ran at 1,217 req/s, the upstream's own ceiling ([method and caveats](bench/vs-litellm/README.md)). It also sends a repeated prompt upstream once by default: 2,000 requests over 50 distinct prompts became 50 calls.

Ran 24 Claude Code agents on one RTX 4070. Cut API use to 26% of expected.

- Sends the same prompt once. Since 3.0 that includes sampled requests: an identical request inside the 5-minute window is a duplicate, not a new question. Want a fresh sample? Send `x-orchestrator-fresh: 1`.
- Caps retries in dollars, not attempts. A retry count says nothing about money: three retries of a 200K-token agent context are dollars. `--retry-budget` (default $0.50 an hour plus 10% of real spend) refuses the retry that does not fit, and a 4xx is never retried.
- Reserves the spend cap before the call. A budget checked after the call is a receipt.
- Fails fast when the provider is down
- Works with Anthropic, OpenAI, llama.cpp, vLLM

## How much are you paying twice?

```sh
cargo install --locked tokio-prompt-orchestrator --features cli --bin burn
burn your_request_log.jsonl          # OpenAI-style logs, LiteLLM spend logs, Helicone/Langfuse exports
burn audit path/to/your/agent        # retry code with no stop, no dollar cap, or retries on a 4xx
```

What it found in public data ([method and numbers](docs/burn-findings.md)):

- **SWE-agent trajectories** (nebius/SWE-agent-trajectories, 6,669 runs, 174,815 model calls): 12,533 calls (7.2%) repeated the previous action and got the identical result back, half of them the same rejected edit re-sent after the same syntax error. They used 10.9% of the tokens, because they happen late, in long contexts. 1,176 of the 6,669 runs (18%) did it at least once.
- **WildChat-1M** (2 of 14 shards, 334,731 real gpt-3.5 and gpt-4 calls from one public app): 5,097 exact repeats within 5 minutes of the same answer, 1.5% of spend at the 2023 list prices; 8,380 (2.3%) within an hour.

## Install

```sh
docker run --rm -p 8080:8080 ghcr.io/mattbusel/tokio-prompt-orchestrator
```

Kubernetes:

```sh
helm install llm-proxy oci://ghcr.io/mattbusel/charts/tokio-prompt-orchestrator   --set provider=openai --set secret.openaiApiKey=$OPENAI_API_KEY --set secret.apiKey=$PROXY_KEY
```

Rust:

```sh
cargo install --locked tokio-prompt-orchestrator --features web-api,tantivy
```

## Use

```sh
orchestrator --provider echo
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
```

Your OpenAI client now goes through it. No code change.

Any OpenAI-compatible upstream (OpenRouter, Groq, vLLM): `ORCHESTRATOR_UPSTREAM_URL=https://openrouter.ai/api/v1 orchestrator --provider openai`

## vs LiteLLM

Same mock upstream, same 4 CPUs ([method and caveats](bench/vs-litellm/README.md)):

| | this | LiteLLM |
|---|---|---|
| Memory | 55 MB | 347 MB |
| Added latency | +0.5 ms | +7.8 ms |
| Throughput, 64 in flight | 1,217 req/s | 162 req/s (376 with 4 workers) |
| 2,000 requests, 50 distinct prompts | 50 upstream calls | 2,000 (50 with cache on) |
| Docker image | 16 MB | 404 MB |

LiteLLM does far more: 100+ providers, per-request model and parameters, tools, virtual keys. This one serves one model and ignores the request's `model`, `temperature` and `max_tokens`.

[Docs](https://docs.rs/tokio-prompt-orchestrator) · [crates.io](https://crates.io/crates/tokio-prompt-orchestrator) · [Site](https://tokio-prompt-orchestrator.vercel.app/) · [Releases](https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases)

More: [docs/DETAILS.md](docs/DETAILS.md)

License: MIT
