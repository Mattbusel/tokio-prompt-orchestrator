window.BENCHMARK_DATA = {
  "lastUpdate": 1774253952356,
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
          "id": "91166e15ad6e3c30dc164794adef085f6f56bf61",
          "message": "feat: add context_compression and prompt_router modules\n\n- context_compression: SlidingWindow/Summarize/DropOldest/Hybrid strategies,\n  ContextCompressor, ContextBudget, estimate_tokens, importance_score with unit tests\n- prompt_router: RoutingRule/RoutingCondition/RouteTarget, PromptRouter with\n  priority-ordered evaluation, glob matching, PromptRouterBuilder fluent API,\n  and unit tests covering keyword match, priority, AND/OR conditions, reject rule",
          "timestamp": "2026-03-23T04:14:45-04:00",
          "tree_id": "fc611fc154cb487f610911af057cb8921359c26b",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/91166e15ad6e3c30dc164794adef085f6f56bf61"
        },
        "date": 1774253951995,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11104302,
            "range": "± 136292",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51125830,
            "range": "± 156680",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51045268,
            "range": "± 215290",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51122819,
            "range": "± 191874",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 36972,
            "range": "± 1588",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37312,
            "range": "± 1584",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 38133,
            "range": "± 1707",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 183,
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