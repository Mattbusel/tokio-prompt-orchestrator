window.BENCHMARK_DATA = {
  "lastUpdate": 1774254648059,
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
          "id": "081cef251c7f45e0b133daa9921ed3aeda2154d0",
          "message": "feat: add tool_call_parser and session_manager modules\n\n- tool_call_parser: LLM output parser with balanced-brace extraction,\n  schema validation, ToolCallBuilder, and format_tool_result helper\n- session_manager: multi-session lifecycle management with idle/lifetime\n  expiry, message limits, context limits, and aggregate stats",
          "timestamp": "2026-03-23T04:26:18-04:00",
          "tree_id": "d54fd430859096c3ba8c66620f48c2d744b89bc6",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/081cef251c7f45e0b133daa9921ed3aeda2154d0"
        },
        "date": 1774254647702,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11117462,
            "range": "± 133045",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51158770,
            "range": "± 177712",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51097307,
            "range": "± 153094",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51202915,
            "range": "± 208312",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33206,
            "range": "± 1234",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 32927,
            "range": "± 967",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32731,
            "range": "± 1087",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 162,
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