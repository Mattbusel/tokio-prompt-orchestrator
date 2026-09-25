window.BENCHMARK_DATA = {
  "lastUpdate": 1790368373168,
  "repoUrl": "https://github.com/Mattbusel/tokio-prompt-orchestrator",
  "entries": {
    "Pipeline Benchmarks": [
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f4fd847ad65434a19e8fb9cc365fce974b60fa8c",
          "message": "CI: do not cache the bench job's target dir (#10)\n\nA restored partial criterion/ directory made the bench job fail on main\n(Criterion looked for a missing base/sample.json and produced no results).",
          "timestamp": "2026-09-25T16:29:03-04:00",
          "tree_id": "659fd44a38f9bf4b177dd3071cb365267fe39327",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/f4fd847ad65434a19e8fb9cc365fce974b60fa8c"
        },
        "date": 1790368372037,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11148551,
            "range": "± 146498",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51051750,
            "range": "± 190150",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51152847,
            "range": "± 120339",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51063591,
            "range": "± 187687",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 25214,
            "range": "± 817",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 25186,
            "range": "± 499",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 25064,
            "range": "± 877",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 145,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 10,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 13,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}