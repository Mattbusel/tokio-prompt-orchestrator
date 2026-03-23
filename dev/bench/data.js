window.BENCHMARK_DATA = {
  "lastUpdate": 1774251434869,
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
          "id": "d0eaba3dbeb5164862beac0b4aa69fff160f8d99",
          "message": "Round 18: streaming processor, provider manager\n\n- src/streaming_processor.rs: StreamToken, FinishReason, StreamState,\n  StreamBuffer (sentence accumulation), StreamingProcessor (transform /\n  filter / aggregate_stats), StreamTransform, StreamFilter, StreamStats;\n  full unit test suite.\n- src/provider_manager.rs: Provider, ProviderHealth, ProviderSelection,\n  ProviderStats, ProviderManager with register/update_health/select_provider/\n  failover_chain/record_request/provider_stats/best_provider_for_budget;\n  sliding rate-limit window; full unit test suite.\n- src/lib.rs: pub mod streaming_processor; pub mod provider_manager.",
          "timestamp": "2026-03-23T03:32:51-04:00",
          "tree_id": "c9ace54448c0f5e69c46d0c63ad4305441c6f5ac",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/d0eaba3dbeb5164862beac0b4aa69fff160f8d99"
        },
        "date": 1774251434360,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11177275,
            "range": "± 161646",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51213713,
            "range": "± 165332",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51115498,
            "range": "± 150582",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51038801,
            "range": "± 145143",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33667,
            "range": "± 1364",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 34103,
            "range": "± 1096",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33886,
            "range": "± 1111",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 142,
            "range": "± 1",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 11,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 14,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}