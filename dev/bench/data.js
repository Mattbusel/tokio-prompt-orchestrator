window.BENCHMARK_DATA = {
  "lastUpdate": 1774263624238,
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
          "id": "61e65de7e92f293b735f587f2bde7a8e65a192a7",
          "message": "Round 32: prompt_optimizer + ab_testing",
          "timestamp": "2026-03-23T06:55:59-04:00",
          "tree_id": "9c9776b49e47d91cf8906cf1456ac5af9ff7915e",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/61e65de7e92f293b735f587f2bde7a8e65a192a7"
        },
        "date": 1774263623699,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11138150,
            "range": "± 131053",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51193890,
            "range": "± 154730",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51055704,
            "range": "± 86830",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51061257,
            "range": "± 215515",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33547,
            "range": "± 1065",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33474,
            "range": "± 838",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33438,
            "range": "± 813",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 164,
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
            "value": 15,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}