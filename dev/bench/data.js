window.BENCHMARK_DATA = {
  "lastUpdate": 1774253130042,
  "repoUrl": "https://github.com/Mattbusel/tokio-prompt-orchestrator",
  "entries": {
    "Pipeline Benchmarks": [
      {
        "commit": {
          "author": {
            "email": "agent@improvement-loop.dev",
            "name": "Improvement Agent"
          },
          "committer": {
            "email": "agent@improvement-loop.dev",
            "name": "Improvement Agent"
          },
          "distinct": true,
          "id": "993d7b5fd181a7f7332d319712713076e04e0c77",
          "message": "feat: add semantic_cache and rate_limiter modules",
          "timestamp": "2026-03-23T04:01:03-04:00",
          "tree_id": "1e986e42b54bcad9763aa8281e11cf7adb940944",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/993d7b5fd181a7f7332d319712713076e04e0c77"
        },
        "date": 1774253129639,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11203613,
            "range": "± 129269",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51179281,
            "range": "± 156819",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51108365,
            "range": "± 132010",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51247146,
            "range": "± 154631",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 35093,
            "range": "± 656",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 34950,
            "range": "± 682",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 35245,
            "range": "± 712",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 194,
            "range": "± 1",
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
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}