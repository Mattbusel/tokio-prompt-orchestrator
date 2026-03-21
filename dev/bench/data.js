window.BENCHMARK_DATA = {
  "lastUpdate": 1774105231335,
  "repoUrl": "https://github.com/Mattbusel/tokio-prompt-orchestrator",
  "entries": {
    "Pipeline Benchmarks": [
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "distinct": true,
          "id": "87f0284dde7f084b7485323a7905a284812926c2",
          "message": "chore: update Cargo.lock for v1.2.0",
          "timestamp": "2026-03-21T10:56:15-04:00",
          "tree_id": "69c88a25998f9bf0326e52b6d3263cb3f46617e3",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/87f0284dde7f084b7485323a7905a284812926c2"
        },
        "date": 1774105230787,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11104203,
            "range": "± 142715",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51082261,
            "range": "± 170604",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51119064,
            "range": "± 159544",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51112232,
            "range": "± 172606",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 36875,
            "range": "± 1688",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37065,
            "range": "± 960",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 37308,
            "range": "± 971",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 199,
            "range": "± 2",
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