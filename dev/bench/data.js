window.BENCHMARK_DATA = {
  "lastUpdate": 1774238250641,
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
          "id": "1a070f9b3b38f27ffcfcce5621ce24b169c57d75",
          "message": "feat(round-6): add SessionManager and StreamAggregator\n\n- src/session_mgr.rs: concurrent session store (DashMap) with create,\n  append, get, delete, get_context (token-budget trimming, always\n  preserves system message), summarize_if_needed, and stats; 22 unit tests\n- src/stream_agg.rs: streaming token aggregator with feed/complete/subscribe\n  (broadcast channel via async_stream) and AggStats; 15 unit tests\n- src/lib.rs: pub mod session_mgr; pub mod stream_agg;\n- src/web_api.rs: POST/GET/DELETE /api/v1/sessions and\n  POST /api/v1/sessions/:id/messages; SessionManager wired into AppState\n- README.md: v1.8.0 section documenting both new modules",
          "timestamp": "2026-03-22T23:53:00-04:00",
          "tree_id": "70770948a4b873021fdb0b99637558fcafb3023d",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/1a070f9b3b38f27ffcfcce5621ce24b169c57d75"
        },
        "date": 1774238250055,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11118381,
            "range": "± 121354",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51172890,
            "range": "± 186940",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51139193,
            "range": "± 139162",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51129759,
            "range": "± 188922",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 36512,
            "range": "± 951",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 36288,
            "range": "± 974",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36029,
            "range": "± 1817",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 167,
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