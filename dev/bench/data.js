window.BENCHMARK_DATA = {
  "lastUpdate": 1774256359952,
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
          "id": "f229918ee166a792a8abee214b2ee4b434d27123",
          "message": "feat: add prompt_safety and experiment_runner modules\n\n- prompt_safety: content moderation with toxicity detection (hate, violence,\n  spam categories), PII detection (email, phone, SSN, credit card, IP),\n  PII redaction, and SafetyFilter wrapper\n- experiment_runner: A/B test runner with consistent FNV1a variant assignment,\n  Welch's t-test significance testing, Cohen's d effect size, and text reports",
          "timestamp": "2026-03-23T04:54:51-04:00",
          "tree_id": "23d2e7ada2d56a9b223a2e4801246d742445cdfe",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/f229918ee166a792a8abee214b2ee4b434d27123"
        },
        "date": 1774256359594,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11130373,
            "range": "± 141409",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51136077,
            "range": "± 128041",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51062112,
            "range": "± 142232",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51150309,
            "range": "± 121134",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33655,
            "range": "± 817",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33693,
            "range": "± 1021",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33246,
            "range": "± 733",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 143,
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