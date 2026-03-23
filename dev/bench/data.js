window.BENCHMARK_DATA = {
  "lastUpdate": 1774241941042,
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
          "id": "b362f32f6bbab8ec18b4a379e1d6ed0cf414c057",
          "message": "feat: add multi-level priority queue with aging and hot-reloadable config\n\n- src/priority_queue.rs: MultiLevelQueue<T> with five Priority levels\n  (Critical=0 … Background=4), AgingConfig with per-level ms thresholds,\n  AtomicU8 effective_priority, aging pass on every pop() that promotes\n  items waiting beyond threshold, QueueStats with avg_wait_ms per level,\n  and 9 unit tests covering ordering, aging promotion, starvation\n  prevention, FIFO drain, and stats tracking.\n\n- src/hot_config.rs: HotConfig backed by Arc<RwLock<ConfigSnapshot>>;\n  lightweight flat key=value TOML parser (handles comments, [sections],\n  bool/int/float/list/string values); start_watcher() spawns a Tokio\n  task that polls mtime and broadcasts version increments via\n  tokio::sync::watch; FromConfigValue trait for String/i64/f64/bool;\n  13 unit tests covering parse variants, missing-key defaults, type\n  coercion, version bumping, and watcher file-reload.",
          "timestamp": "2026-03-23T00:54:34-04:00",
          "tree_id": "228daa47f36c41722371f55291326d1dd60873f2",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/b362f32f6bbab8ec18b4a379e1d6ed0cf414c057"
        },
        "date": 1774241940448,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11098027,
            "range": "± 106404",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51114528,
            "range": "± 173488",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51078603,
            "range": "± 151192",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51223151,
            "range": "± 170474",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33481,
            "range": "± 892",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33246,
            "range": "± 1085",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33133,
            "range": "± 843",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 163,
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