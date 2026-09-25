window.BENCHMARK_DATA = {
  "lastUpdate": 1790367679043,
  "repoUrl": "https://github.com/Mattbusel/tokio-prompt-orchestrator",
  "entries": {
    "Pipeline Benchmarks": [
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f8ba89ec461056975a060ab070920385ef678766",
          "message": "Get CI green, fix bugs found by the test suite, add a runnable LLM pipeline example (#4)\n\n* Get CI green, fix bugs found by the test suite, add a runnable LLM pipeline example\n\nCI had been red on every push since 2026-03-19. Causes and fixes:\n\n- web-api feature did not compile: web_api.rs used a rate_limiter::RateLimiter\n  type that the rate_limiter rewrite removed. The /api/v1/rate-limiter/stats\n  handler now reads RateLimiterRegistry (per-model throttled counts).\n- trace_ui.rs format width used a u16 (needs usize).\n- Clippy -D warnings: unused imports/mut, lock().unwrap() under the crate's\n  unwrap_used/expect_used deny lints (now poison-tolerant), manual clamp,\n  while-let loops, etc.\n- rustdoc -D warnings: ~50 broken intra-doc links.\n- cargo audit: cargo update for patched h2, rustls, quinn-proto,\n  crossbeam-epoch; the remaining advisories are ignored in CI with a reason\n  each (not reachable, or need an MSRV bump).\n\nReal bugs fixed (each was caught by an existing failing test):\n\n- priority_queue: aging used the thresholds in reverse order, so Background\n  items waited 60 s instead of 5 s; promoted items now keep enqueue order.\n- context_mgr: SummarizeOldest ended over budget (placeholder not counted).\n- hot_config: set()/get() panicked inside a Tokio runtime (blocking_write);\n  switched to a short parking_lot lock.\n- multi_modal: serializing any Text part failed (internally tagged newtype);\n  now {\"type\":\"text\",\"text\":...}.\n- prompt_template: {{ x | default:y }} errored when x was unset; truncate\n  sliced bytes and could panic on multi-byte text.\n- prompt_versioning: blame() returned a version for lines that do not exist.\n- prompt_optimizer: filler removal missed \"kindly.\" and mangled words such\n  as \"displease\"; now whole-word regex.\n- job_scheduler: intervals shorter than the 100 ms poll fired late.\n- intent_classifier: \"Analyse X\" was classified as a plain command.\n- retry_policy::retry_async panicked with max_attempts = 0.\n- orchestrator binary: --provider on the command line was ignored when a\n  saved config was absent (\"Unknown provider 'unknown'\").\n\nStale tests updated to match intentional code changes: 401 responses map to\nOrchestratorError::AuthFailed; leader renewal interval is ttl/3.\n\nDocs and quick start:\n\n- README: `--worker echo` does not exist; it is `--provider echo`. Added\n  default-run so `cargo run` works with several binaries.\n- README is included as crate docs, so its snippets are doctests: fragments\n  are now `rust,ignore`, prose blocks `text`; broken module doctests fixed.\n- New examples/llm_pipeline.rs: runs with no API key (mock model), or\n  against Anthropic/OpenAI via PROVIDER=. Shows dedup (12 requests, 3 model\n  calls) and the circuit breaker + DLQ during a simulated outage.\n\nCI: added a test job (integration tests, doctests, examples, runs the new\nexample) and dependency caching.\n\n* Fix config watcher hang and lints from current stable clippy (1.98)\n\n- config::watcher ran a blocking recv_timeout loop inside tokio::spawn,\n  which stalls a runtime worker and hangs a current-thread runtime\n  (the hot-reload load tests never finished). It now runs on its own OS\n  thread and exits when the watcher is dropped.\n- Clippy 1.98 lints: sort_by_key, collapsible match, explicit counter\n  loop, checked_div, redundant borrows.\n\n* Remove stray build cache files\n\n* Config watcher: trailing-edge debounce so the last write in a burst is reloaded\n\nThe leading-edge debounce reloaded on the first event of a burst (often\nreading a half-written file) and silently dropped the rest, so rapid saves\ncould leave the old config in place. Fixes the failing\ntest_hot_reload_rapid_writes_converge_to_final_config on Linux.",
          "timestamp": "2026-09-25T16:16:51-04:00",
          "tree_id": "5c4cf4ceb2450a3627d600442d4999953cf67940",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/f8ba89ec461056975a060ab070920385ef678766"
        },
        "date": 1790367678360,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11122632,
            "range": "± 150533",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51058579,
            "range": "± 197398",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51129304,
            "range": "± 125039",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51097969,
            "range": "± 145459",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37879,
            "range": "± 1722",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 36776,
            "range": "± 1932",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 37850,
            "range": "± 1607",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 148,
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