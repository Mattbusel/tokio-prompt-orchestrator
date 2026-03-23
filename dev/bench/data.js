window.BENCHMARK_DATA = {
  "lastUpdate": 1774248646451,
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
          "id": "2c555c0b25972d47ee3e9175168597fe9b7dea62",
          "message": "Round 15: intent classifier, response validator\n\n- src/intent_classifier.rs: IntentCategory enum (7 variants) with\n  description() and suggested_model_tier(); IntentFeatures struct;\n  IntentClassifier with extract_features, classify,\n  classify_with_confidence, batch_classify; code + creative keyword\n  lists; full unit tests.\n- src/response_validator.rs: ValidationRule enum (8 variants including\n  glob-wildcard MatchesRegex with no regex dep); ValidationResult,\n  ValidationViolation; ResponseValidator with validate, validate_all,\n  pass_rate; full unit tests.\n- src/lib.rs: pub mod declarations for both new modules.",
          "timestamp": "2026-03-23T02:46:19-04:00",
          "tree_id": "fec11299cb654afff2042a71f682e0c7c726071a",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/2c555c0b25972d47ee3e9175168597fe9b7dea62"
        },
        "date": 1774248646073,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11099256,
            "range": "± 130972",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51111647,
            "range": "± 142128",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51092196,
            "range": "± 173649",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51163446,
            "range": "± 161375",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 36289,
            "range": "± 1186",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 36373,
            "range": "± 1026",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36239,
            "range": "± 695",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 162,
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