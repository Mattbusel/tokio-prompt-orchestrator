window.BENCHMARK_DATA = {
  "lastUpdate": 1774252286341,
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
          "id": "029632f6d4de5a94b6fb525ef054f4416c7296d5",
          "message": "Round 19: conversation state machine, eval harness\n\nAdd ConversationStateMachine with phase lifecycle tracking, heuristic\ntrigger detection from user text, and full transition-rule table.\nAdd EvalHarness for multi-metric prompt strategy evaluation, strategy\ncomparison ranking, tag filtering, and difficulty-level pass-rate breakdown.",
          "timestamp": "2026-03-23T03:46:43-04:00",
          "tree_id": "2403d61c645c773d1edafd953fd35473386b9512",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/029632f6d4de5a94b6fb525ef054f4416c7296d5"
        },
        "date": 1774252286000,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11116627,
            "range": "± 136593",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51070097,
            "range": "± 151941",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51108758,
            "range": "± 148358",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51133942,
            "range": "± 151151",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 34116,
            "range": "± 1620",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 34831,
            "range": "± 2187",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 35104,
            "range": "± 2281",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 180,
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