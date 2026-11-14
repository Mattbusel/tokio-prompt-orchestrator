# tokio-prompt-orchestrator

English | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

[![crates.io](https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg)](https://crates.io/crates/tokio-prompt-orchestrator) [![docs.rs](https://img.shields.io/docsrs/tokio-prompt-orchestrator)](https://docs.rs/tokio-prompt-orchestrator) [![MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![CI](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/tokio-prompt-orchestrator/actions/workflows/ci.yml)

Sits in front of your LLM API and cuts the bill.

Ran 24 Claude Code agents on one RTX 4070. Cut API use to 26% of expected.

- Sends the same prompt once
- Fails fast when the provider is down
- Caps spend
- Works with Anthropic, OpenAI, llama.cpp, vLLM

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
