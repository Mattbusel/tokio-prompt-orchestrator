window.BENCHMARK_DATA = {
  "lastUpdate": 1774245398875,
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
          "id": "2bf1186d4f493ccb426b8e70d7d9045fa3ed047f",
          "message": "Add worker_pool and model_selector modules\n\n- src/worker_pool.rs: Dynamic auto-scaling worker pool with WorkerConfig,\n  WorkItem, WorkerPool<T,R>, PoolStats; auto-scales up when queue depth\n  exceeds workers*queue_depth_per_worker; idles workers down after timeout;\n  drain_and_shutdown waits for all pending work; 5 unit tests all pass.\n\n- src/model_selector.rs: Intelligent model selection with ModelProfile,\n  SelectionCriteria, SelectionStrategy (CheapestFirst/FastestFirst/BestQuality/\n  Balanced), ModelSelector, ModelUsageTracker; 10 unit tests all pass.\n\n- Fix pre-existing test compilation errors: add #[derive(Debug)] to\n  RegexReplaceStage in pipeline.rs; add mut to rx binding in request_dedup.rs.",
          "timestamp": "2026-03-23T01:52:01-04:00",
          "tree_id": "50194f7f9a558d87159bf495487888633a3cc6e7",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/2bf1186d4f493ccb426b8e70d7d9045fa3ed047f"
        },
        "date": 1774245397942,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11110913,
            "range": "± 126456",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51091154,
            "range": "± 136860",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51076390,
            "range": "± 123253",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51161180,
            "range": "± 187958",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33193,
            "range": "± 1009",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 32771,
            "range": "± 948",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32924,
            "range": "± 1517",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 150,
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