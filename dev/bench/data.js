window.BENCHMARK_DATA = {
  "lastUpdate": 1773922759937,
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
          "id": "91cfad4d5fae1f6125e2fb2d0d019d82efac6b2a",
          "message": "fix(ci): ignore filesystem-event tests in coverage, add 20m timeout",
          "timestamp": "2026-03-19T08:14:54-04:00",
          "tree_id": "df4cb9a698ee03cb70da307f6ca2bc84b461c5d8",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/91cfad4d5fae1f6125e2fb2d0d019d82efac6b2a"
        },
        "date": 1773922759048,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11113723,
            "range": "± 143629",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51099749,
            "range": "± 130579",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51127600,
            "range": "± 104345",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51138302,
            "range": "± 143343",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37176,
            "range": "± 972",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 36988,
            "range": "± 780",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36986,
            "range": "± 1114",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 147,
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