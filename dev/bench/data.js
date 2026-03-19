window.BENCHMARK_DATA = {
  "lastUpdate": 1773923129014,
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
          "id": "faa59dfe247150ab37d7b43fa26540f35934aff5",
          "message": "fix(tests): correct SinkError display assertion, abort dedup cleanup task on shutdown",
          "timestamp": "2026-03-19T08:21:04-04:00",
          "tree_id": "150161515785b5ed76eba7031b9d6abfee087c59",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/faa59dfe247150ab37d7b43fa26540f35934aff5"
        },
        "date": 1773923128682,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11099603,
            "range": "± 121887",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51074394,
            "range": "± 155478",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51099512,
            "range": "± 112211",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51176230,
            "range": "± 217544",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37915,
            "range": "± 932",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37362,
            "range": "± 1145",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 37515,
            "range": "± 879",
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
            "value": 15,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}