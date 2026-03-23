window.BENCHMARK_DATA = {
  "lastUpdate": 1774231803432,
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
          "id": "40318b48543ab7563da57ffd7924337ed9916b11",
          "message": "fix: remove unused imports in cost_optimizer\n\nRemove unused `Duration` import (only `Instant` is needed) and unused\n`warn` import (only `info` is used) from cost_optimizer.rs. Eliminates\ntwo compiler warnings from the clean build.",
          "timestamp": "2026-03-22T22:05:39-04:00",
          "tree_id": "eac17e3ed30b308de66c73c3a8565da04f5b01bc",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/40318b48543ab7563da57ffd7924337ed9916b11"
        },
        "date": 1774231802880,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11150281,
            "range": "± 136241",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51103853,
            "range": "± 189626",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51141805,
            "range": "± 156733",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51139607,
            "range": "± 114648",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 30524,
            "range": "± 2085",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 31126,
            "range": "± 1982",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32095,
            "range": "± 2162",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 260,
            "range": "± 5",
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