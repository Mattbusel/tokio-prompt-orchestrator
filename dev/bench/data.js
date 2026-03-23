window.BENCHMARK_DATA = {
  "lastUpdate": 1774237188952,
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
          "id": "aa123d7622e829b14bcf5d425686daa0a53c47e7",
          "message": "feat: add Prompt Pipeline and Audit Log modules (Round 5)\n\n- pipeline.rs: composable async stage pipeline with TrimStage,\n  TruncateStage, PrependStage, AppendStage, RegexReplaceStage,\n  LanguageDetectStage; PipelineBuilder fluent API; PipelineStats;\n  20+ unit tests\n- audit.rs: append-only VecDeque-backed AuditLog with capacity eviction;\n  AuditFilter (since/model_id/cache_hit/min_latency_ms); export_jsonl;\n  AuditStats with cache-hit rate and per-model counts; 15+ unit tests\n- Cargo.toml: add regex = \"1\" dependency\n- lib.rs: pub mod pipeline; pub mod audit; re-exports\n- README.md: Prompt Pipeline and Audit Log sections",
          "timestamp": "2026-03-22T23:35:20-04:00",
          "tree_id": "70571e7dca71d5dfdce76453ebe8ff09c910a2a8",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/aa123d7622e829b14bcf5d425686daa0a53c47e7"
        },
        "date": 1774237188664,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11097605,
            "range": "± 113957",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51073251,
            "range": "± 116863",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51117648,
            "range": "± 145532",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51091296,
            "range": "± 155636",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33294,
            "range": "± 897",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33781,
            "range": "± 1299",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33192,
            "range": "± 1034",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 177,
            "range": "± 5",
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
            "value": 18,
            "range": "± 1",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}