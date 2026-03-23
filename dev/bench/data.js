window.BENCHMARK_DATA = {
  "lastUpdate": 1774240305950,
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
          "id": "02bd6aedf22aaedd835e0bcbd550acd14b61178f",
          "message": "feat: add circuit breaker state machine and AIMD admission control\n\n- src/circuit_breaker.rs: per-model 3-state FSM (Closed/Open/HalfOpen)\n  with sliding-window failure tracking, CircuitBreakerRegistry (DashMap),\n  failure_rate() metric, and comprehensive unit tests covering all\n  state transitions\n- src/admission_control.rs: AIMD concurrency limiter with AcquireGuard\n  RAII, gradient-based overload detection via latency EWMA, rolling\n  100-sample acceptance-rate window, and concurrent unit tests\n- src/lib.rs: pub mod circuit_breaker; pub mod admission_control;",
          "timestamp": "2026-03-23T00:27:09-04:00",
          "tree_id": "66ab988ff88b2a9df0a14c78b5d4d1707bcd787c",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/02bd6aedf22aaedd835e0bcbd550acd14b61178f"
        },
        "date": 1774240305457,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11102495,
            "range": "± 143141",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51103138,
            "range": "± 119158",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51075383,
            "range": "± 142086",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51132821,
            "range": "± 188138",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 31294,
            "range": "± 2145",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 30181,
            "range": "± 2241",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 30639,
            "range": "± 2517",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 256,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 9,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 12,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}