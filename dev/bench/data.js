window.BENCHMARK_DATA = {
  "lastUpdate": 1773918909994,
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
          "id": "b5bb3e7651dff41a5975ab6560e514effc9f7a24",
          "message": "fix(ci): restore watcher test inside mod block, add WorkersConfig import, add bench write permission",
          "timestamp": "2026-03-19T07:11:07-04:00",
          "tree_id": "9b5861d51a4a8b2f86dde7ad173f8e211d35aa99",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/b5bb3e7651dff41a5975ab6560e514effc9f7a24"
        },
        "date": 1773918909432,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11137386,
            "range": "± 147478",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51272377,
            "range": "± 154068",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51104104,
            "range": "± 167115",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51242985,
            "range": "± 190479",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 28790,
            "range": "± 2251",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 29630,
            "range": "± 2439",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 29867,
            "range": "± 2760",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 251,
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