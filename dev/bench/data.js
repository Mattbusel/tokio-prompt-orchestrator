window.BENCHMARK_DATA = {
  "lastUpdate": 1774256773465,
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
          "id": "8f8f9b323948dd0a9691618300d9e89868d393e9",
          "message": "Round 25: adaptive_timeout + cost_estimator",
          "timestamp": "2026-03-23T05:01:15-04:00",
          "tree_id": "744f186391812428caef1e9a56c143da1bc5c536",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/8f8f9b323948dd0a9691618300d9e89868d393e9"
        },
        "date": 1774256772865,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11166273,
            "range": "± 174201",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51135490,
            "range": "± 277000",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51203803,
            "range": "± 178384",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51184812,
            "range": "± 187599",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 35125,
            "range": "± 661",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 35235,
            "range": "± 723",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 35173,
            "range": "± 744",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 161,
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
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}