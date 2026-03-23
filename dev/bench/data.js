window.BENCHMARK_DATA = {
  "lastUpdate": 1774243971940,
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
          "id": "09d44e8b84cf4031a257524a0cf66af7ea49710c",
          "message": "Add observability and retry_policy modules\n\n- src/observability.rs: OpenTelemetry-compatible tracing and metrics\n  implemented from scratch. Includes SpanContext (trace/span IDs, sampling,\n  parent linking), Span with attributes/events/status, SpanExporter trait,\n  InMemoryExporter, Tracer (start_span, start_child_span, finish_span), and\n  ObservabilityRegistry with Prometheus exposition-format metric export.\n  8 unit tests covering duration calculation, trace ID inheritance, exporter\n  collection, and Prometheus output format.\n\n- src/retry_policy.rs: Configurable retry policy engine with NoRetry,\n  FixedDelay, ExponentialBackoff, LinearBackoff, and Fibonacci strategies;\n  Full/Equal/Decorrelated/None jitter modes via LCG RNG; RetryState with\n  next_delay(); retry_async generic over any async fallible function; and\n  RetryMetrics summary. 11 unit tests covering all strategies, jitter bounds,\n  max-attempts enforcement, non-retryable error short-circuit, and async\n  success-on-third-attempt.",
          "timestamp": "2026-03-23T01:28:07-04:00",
          "tree_id": "993c845084d6a5d4e487f33179d62928d7884e9c",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/09d44e8b84cf4031a257524a0cf66af7ea49710c"
        },
        "date": 1774243971454,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11144030,
            "range": "± 143297",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51072753,
            "range": "± 207750",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51066398,
            "range": "± 161833",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51197837,
            "range": "± 139730",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37448,
            "range": "± 1130",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37232,
            "range": "± 950",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36876,
            "range": "± 918",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 149,
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