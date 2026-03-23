window.BENCHMARK_DATA = {
  "lastUpdate": 1774262594848,
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
          "id": "f74462c5c65339cde0f72c68cd0983fa1a640948",
          "message": "Round 31: rate_limiter + session_manager",
          "timestamp": "2026-03-23T06:38:45-04:00",
          "tree_id": "fc6babf4ebb6f5b133b7961432484aad92b7736c",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/f74462c5c65339cde0f72c68cd0983fa1a640948"
        },
        "date": 1774262593911,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11111625,
            "range": "± 132743",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51140835,
            "range": "± 163623",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51151088,
            "range": "± 171529",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51106072,
            "range": "± 159093",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33436,
            "range": "± 805",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 32813,
            "range": "± 892",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33159,
            "range": "± 1492",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 165,
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