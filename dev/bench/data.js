window.BENCHMARK_DATA = {
  "lastUpdate": 1774234400351,
  "repoUrl": "https://github.com/Mattbusel/tokio-prompt-orchestrator",
  "entries": {
    "Pipeline Benchmarks": [
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Mattbusel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "mattbusel@gmail.com",
            "name": "Mattbusel",
            "username": "Mattbusel"
          },
          "distinct": true,
          "id": "31d3b46f5fd1cc913515a357227ce3b7f5be3985",
          "message": "feat: add PromptCache and RateLimiter with web-api endpoints\n\n- src/cache.rs: Content-addressed LRU prompt cache keyed by SHA-256 of\n  (model_id + prompt). Supports configurable TTL, max_entries, and\n  max_prompt_len. LRU eviction via VecDeque+HashMap. Thread-safe via\n  Arc<Mutex<>>. 19 unit tests covering hits, misses, LRU eviction, TTL\n  expiry, flush, clone sharing, and stats.\n\n- src/rate_limiter.rs: Per-model token bucket rate limiter with\n  configurable requests_per_second and burst_capacity. try_acquire\n  (non-blocking) and acquire (async wait). RateLimiterStats per model.\n  19 unit tests including async acquire.\n\n- src/lib.rs: pub mod cache; pub mod rate_limiter; + re-exports.\n\n- src/web_api.rs: Added PromptCache and RateLimiter fields to AppState.\n  New endpoints: GET /api/v1/cache/stats, DELETE /api/v1/cache,\n  GET /api/v1/rate-limiter/stats.\n\n- Cargo.toml: Added sha2 = \"0.10\" and hex = \"0.4\" dependencies.\n\n- README.md: Added \"Prompt Cache\" and \"Rate Limiter\" sections with\n  usage examples and HTTP endpoint tables.",
          "timestamp": "2026-03-22T22:48:47-04:00",
          "tree_id": "9f990fb634589c4519c812e22cfe0f8cb1977e8a",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/31d3b46f5fd1cc913515a357227ce3b7f5be3985"
        },
        "date": 1774234399456,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11092696,
            "range": "± 126124",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51185927,
            "range": "± 179894",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51077537,
            "range": "± 122368",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51187222,
            "range": "± 151855",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33356,
            "range": "± 794",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 32846,
            "range": "± 803",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32922,
            "range": "± 1172",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 171,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 12,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 15,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}