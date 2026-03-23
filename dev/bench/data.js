window.BENCHMARK_DATA = {
  "lastUpdate": 1774255502978,
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
          "id": "d4916270ccaddda3602a1abccc70042b25d86bec",
          "message": "feat: add output_cache and pipeline_builder modules\n\n- output_cache: semantic deduplication cache with TTL, LRU/LFU/TTLFirst/Random\n  eviction policies, FNV-1a hashing, hit-rate stats, and unit tests\n- pipeline_builder: fluent builder for prompt processing pipelines with\n  Preprocess/Validate/Enrich/Transform/Postprocess stage kinds, fail-fast\n  support, and unit tests",
          "timestamp": "2026-03-23T04:40:27-04:00",
          "tree_id": "6fe21894257fad30987cda4ba1d3affd82da0eb5",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/d4916270ccaddda3602a1abccc70042b25d86bec"
        },
        "date": 1774255501975,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11092358,
            "range": "± 105102",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51184859,
            "range": "± 154659",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51155994,
            "range": "± 135474",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51098565,
            "range": "± 129761",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33402,
            "range": "± 985",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33053,
            "range": "± 923",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32623,
            "range": "± 776",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 173,
            "range": "± 4",
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