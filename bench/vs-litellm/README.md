# tokio-prompt-orchestrator vs LiteLLM proxy

Same machine, same mock upstream, same client. Each proxy forwards to `mock_upstream.py`,
a fake OpenAI server that answers every request after 50 ms. Only the proxies' own costs
differ: added latency, throughput, memory, startup time, and how many upstream calls a
burst of repeated prompts turns into.

Run 2026-11-14: Linux container (Docker on WSL2) capped at 4 CPUs and 4 GB, tokio-prompt-orchestrator
2.0.5, LiteLLM 1.109.1 (`litellm[proxy]`), Python 3.12. Both proxies require a bearer key, and
the client sends one on every request.

| | direct (no proxy) | **tokio-prompt-orchestrator** | LiteLLM | LiteLLM, 4 workers | LiteLLM + local cache |
|---|---|---|---|---|---|
| Startup to first answer | - | **0.17 s** | 4.3 s | 8.0 s | 4.3 s |
| Memory, idle | - | **55 MB** | 347 MB | 1,725 MB | 347 MB |
| Memory, peak | - | **70 MB** | 363 MB | 1,784 MB | 363 MB |
| Added latency, p50 (1 request at a time) | 0 | **+0.5 ms** | +7.8 ms | +7.7 ms | +8.2 ms |
| Throughput, 64 in flight | 1,235 req/s | **1,217 req/s** | 162 req/s | 376 req/s | 154 req/s |
| p99 latency, 64 in flight | 57 ms | **74 ms** | 437 ms | 323 ms | 461 ms |
| 2,000 requests over 50 distinct prompts: upstream calls | 2,000 | **50** | 2,000 | 2,000 | 50 |
| ...time to finish | 3.2 s | **0.25 s** | 12.6 s | 6.4 s | 11.2 s |
| Docker image, compressed (amd64) | - | **16 MB** | 404 MB (`ghcr.io/berriai/litellm:main-stable`) | | |
| Errors | 0 | 0 | 0 | 0 | 0 |

Raw numbers: [`results.json`](out/results.json).

## How to read it

- **Throughput hit the upstream ceiling, not the proxy's.** The mock tops out around 1,235 req/s
  with this client, and the orchestrator ran at 99% of that. Its own ceiling is higher than this
  test can show.
- **Dedup is on by default here, and opt-in in LiteLLM.** Repeated prompts become one call in the
  orchestrator without configuration (`ORCHESTRATOR_DEDUP_SECS`, default 300 s). LiteLLM matches
  the call count once `cache: true` is set, but its cache does not raise throughput.
- **LiteLLM's workers buy throughput with memory.** Four workers (the recommended one per CPU) got
  2.3x more throughput for 5x the memory.

## What LiteLLM does that this does not

This benchmark measures the proxy layer. LiteLLM is a much broader product. Choose it if you need:

- **The request as sent.** The orchestrator flattens `messages` into one prompt and uses the model
  and settings it was started with. The request's `model`, `temperature`, `max_tokens`, `tools` and
  `response_format` are accepted but ignored. LiteLLM passes them through.
- **100+ providers and model routing.** The orchestrator speaks OpenAI (and anything
  OpenAI-compatible via `ORCHESTRATOR_UPSTREAM_URL`), Anthropic, llama.cpp and vLLM, one model per process.
- **Virtual keys, per-team budgets, an admin UI and a database.** The orchestrator has one bearer key
  and one global spend cap.

If you want a small, fast proxy in front of one model that turns repeated prompts into one call
and stops at a dollar limit, that is what this is for.

## Run it yourself

```sh
# from the repo root
docker build -t tpo .
cd bench/vs-litellm
docker build -t tpo-bench --build-arg TPO_IMAGE=tpo .
docker run --rm --cpus 4 --memory 4g -e BENCH_CPUS=4 tpo-bench
```

Without Docker: `pip install "litellm[proxy]" aiohttp psutil`, then
`python run.py --orchestrator path/to/orchestrator`. Add `--quick` for a shorter run. Set
`MOCK_DELAY_MS` to change the upstream's answer time.
