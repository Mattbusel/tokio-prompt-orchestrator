window.BENCHMARK_DATA = {
  "lastUpdate": 1774257612362,
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
          "id": "e3f8f021d816e4505eaf2032bc462a795de12f5d",
          "message": "Round 26: streaming_processor + model_fallback",
          "timestamp": "2026-03-23T05:15:46-04:00",
          "tree_id": "03804adc30964297a247f7b1384acc62f950912f",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/e3f8f021d816e4505eaf2032bc462a795de12f5d"
        },
        "date": 1774257611970,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11094276,
            "range": "± 127502",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51119488,
            "range": "± 119945",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51016854,
            "range": "± 135158",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51169433,
            "range": "± 132438",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33409,
            "range": "± 1037",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33238,
            "range": "± 1118",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33177,
            "range": "± 973",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 148,
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