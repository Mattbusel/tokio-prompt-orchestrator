window.BENCHMARK_DATA = {
  "lastUpdate": 1774259610331,
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
          "id": "3a566d7b3ca818f2b1ce9c3a2ebae9e47f0386d3",
          "message": "Round 28: conversation_analyzer + retry_budget",
          "timestamp": "2026-03-23T05:48:59-04:00",
          "tree_id": "db27018d97bc1c7d4c014ec5db28834d4e894f04",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/3a566d7b3ca818f2b1ce9c3a2ebae9e47f0386d3"
        },
        "date": 1774259609328,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11089582,
            "range": "± 119524",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51137318,
            "range": "± 125023",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51129445,
            "range": "± 103944",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51113962,
            "range": "± 192330",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33316,
            "range": "± 1124",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33337,
            "range": "± 990",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33417,
            "range": "± 764",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 141,
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