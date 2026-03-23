window.BENCHMARK_DATA = {
  "lastUpdate": 1774260857228,
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
          "id": "43ecb2ad7d306bcf2cca5d6e01858020eb4e7cae",
          "message": "Round 29: model_registry + prompt_validator",
          "timestamp": "2026-03-23T06:09:13-04:00",
          "tree_id": "cd4ac71ff5fc6d87bfb97a8d66bef196db4e0cc1",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/43ecb2ad7d306bcf2cca5d6e01858020eb4e7cae"
        },
        "date": 1774260856502,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11112969,
            "range": "± 146596",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51118507,
            "range": "± 85199",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51031206,
            "range": "± 139250",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51147580,
            "range": "± 170070",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 36852,
            "range": "± 1670",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 36705,
            "range": "± 837",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36772,
            "range": "± 1082",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 158,
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