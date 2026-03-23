window.BENCHMARK_DATA = {
  "lastUpdate": 1774227956887,
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
          "id": "2b9e4b13c7eaeb4514ddb6e97289a0c3640288fa",
          "message": "docs: major README overhaul — onboarding guide, architecture diagrams, benchmarks, troubleshooting, self-improving docs",
          "timestamp": "2026-03-22T21:01:35-04:00",
          "tree_id": "ffaff12e811a1251ec3cc9ea3facf6cf7d1f7d30",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/2b9e4b13c7eaeb4514ddb6e97289a0c3640288fa"
        },
        "date": 1774227955986,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11096114,
            "range": "± 111744",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51207207,
            "range": "± 167456",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51155356,
            "range": "± 165394",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51122088,
            "range": "± 145132",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37382,
            "range": "± 1073",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37273,
            "range": "± 1591",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 37463,
            "range": "± 990",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 184,
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