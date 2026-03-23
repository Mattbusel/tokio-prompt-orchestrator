window.BENCHMARK_DATA = {
  "lastUpdate": 1774261888535,
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
          "id": "7882365b2ed908e88db76a17e77f63bdc311664c",
          "message": "Round 30: cache_warmer + token_counter",
          "timestamp": "2026-03-23T06:26:47-04:00",
          "tree_id": "487844c45cfafb8f746a395a7653a5aafbaab9c4",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/7882365b2ed908e88db76a17e77f63bdc311664c"
        },
        "date": 1774261887513,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11082406,
            "range": "± 124666",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51067887,
            "range": "± 169708",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51104237,
            "range": "± 163792",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51187324,
            "range": "± 210137",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 29336,
            "range": "± 2207",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 30552,
            "range": "± 2140",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 29224,
            "range": "± 1972",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 201,
            "range": "± 1",
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