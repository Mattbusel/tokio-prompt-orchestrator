window.BENCHMARK_DATA = {
  "lastUpdate": 1774239226621,
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
          "id": "1f721f3bba895e61ab4f926e89e03555a671e72a",
          "message": "feat(round-7): add Plugin system and RequestDeduplicator\n\nPlugin system (plugin.rs):\n- New Plugin trait with on_request/on_response hooks\n- PluginV2Chain: ordered serial execution, stops on RequestRejected\n- PluginV2Registry: register/disable/enable/list with per-plugin stats\n  (request_calls, response_calls, errors)\n- PluginError: RequestRejected, ResponseModified, Fatal\n- Built-in plugins: ProfanityFilterPlugin, ResponseLengthCapPlugin,\n  LatencyLoggerPlugin\n- 21 unit tests in plugin_v2_tests module\n\nRequest deduplication (request_dedup.rs):\n- SHA-256 hash key over model_id + prompt_text\n- DedupDecision::Original / Waiting(oneshot::Receiver)\n- Fan-out of results to all registered waiters on complete()\n- 30s TTL with prune_stale() called on every submit()\n- DedupStats with dedup_rate\n- 17 unit tests\n\nWire-in: pub mod request_dedup added to lib.rs\nDocs: README updated with v1.9.0 section",
          "timestamp": "2026-03-23T00:09:14-04:00",
          "tree_id": "2cf8d5a16f19e8347d115f838e11fe4e9cfb174e",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/1f721f3bba895e61ab4f926e89e03555a671e72a"
        },
        "date": 1774239226226,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11143333,
            "range": "± 139456",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51107418,
            "range": "± 139257",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51154622,
            "range": "± 202113",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51209520,
            "range": "± 126077",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37361,
            "range": "± 1260",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37056,
            "range": "± 1012",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36984,
            "range": "± 1006",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 147,
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