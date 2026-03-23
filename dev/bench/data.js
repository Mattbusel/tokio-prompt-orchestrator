window.BENCHMARK_DATA = {
  "lastUpdate": 1774258853456,
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
          "id": "9d0a0993b3d20cb9dcde13432a5c6a3f09ad7723",
          "message": "Round 27: prompt_template + response_classifier",
          "timestamp": "2026-03-23T05:36:19-04:00",
          "tree_id": "9e2d327d85f4544100cfc5311124a8600193c967",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/9d0a0993b3d20cb9dcde13432a5c6a3f09ad7723"
        },
        "date": 1774258853069,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11122425,
            "range": "± 135815",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51201292,
            "range": "± 172858",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51131642,
            "range": "± 161696",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51208205,
            "range": "± 181890",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33775,
            "range": "± 1645",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33567,
            "range": "± 1086",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33365,
            "range": "± 1399",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 166,
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