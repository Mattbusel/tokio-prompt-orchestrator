window.BENCHMARK_DATA = {
  "lastUpdate": 1774242940625,
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
          "id": "99db0ab7da0144b11853f56566e3013fddc7c892",
          "message": "feat: add job_scheduler and context_mgr modules\n\n- job_scheduler: cron-like async job scheduler with Interval, Once, and\n  Cron schedules; JobId newtype; Scheduler with start/cancel/enable/stats;\n  run_loop spawns handlers as independent Tokio tasks every 100ms; unit\n  tests cover repeated firing, cancel, disable, and one-shot semantics.\n\n- context_mgr: multi-turn LLM conversation context manager with token\n  budget enforcement; TokenCounter word-count heuristic; Role/Message\n  types; ContextConfig with DropOldest, SummarizeOldest, and KeepFirst\n  truncation strategies; ConversationContext tracks utilisation and\n  provides messages_for_api/summary; unit tests verify all strategies.",
          "timestamp": "2026-03-23T01:11:06-04:00",
          "tree_id": "22213e74002eaf5b4035eadc0f8590e8d8fc05d2",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/99db0ab7da0144b11853f56566e3013fddc7c892"
        },
        "date": 1774242940268,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11092494,
            "range": "± 126092",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51080409,
            "range": "± 146702",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51137169,
            "range": "± 196375",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51102725,
            "range": "± 124763",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33529,
            "range": "± 1079",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33043,
            "range": "± 1340",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33592,
            "range": "± 1168",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 144,
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