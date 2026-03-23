window.BENCHMARK_DATA = {
  "lastUpdate": 1774246700892,
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
          "id": "a183930e3c872c0ac431403ada9d210e1ccfb0f2",
          "message": "Add spec-compliant AbTestManager and PromptOptimizer with bandit algorithms\n\n- ab_test.rs: new multi-variant AbTestManager with weighted FNV-hash\n  assignment, two-proportion z-test significance (erfc approximation),\n  winner detection, and auto-complete on FixedSamples/FixedDuration/\n  StatisticalSignificance end conditions; legacy AbTestRunner preserved\n  for backward compatibility\n- prompt_optimizer.rs: new PromptOptimizer with UCB1, EpsilonGreedy,\n  ThompsonSampling, and BestFirst strategies; PromptMutator with\n  paraphrase (mini-thesaurus), add_instruction, and trim_to_budget;\n  legacy PromptAbOptimizer/ScoringEngine/PromoterRegistry preserved\n- Unit tests for deterministic assignment, weighted split, z-test\n  significance, winner detection, UCB1 unsampled-first, epsilon exploration,\n  best_variant correctness, and mutator transformations",
          "timestamp": "2026-03-23T02:13:48-04:00",
          "tree_id": "8b33b2ba31b14dab4138ad6a3daa5da8b51f0733",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/a183930e3c872c0ac431403ada9d210e1ccfb0f2"
        },
        "date": 1774246700347,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11107242,
            "range": "± 145568",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51113734,
            "range": "± 114022",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51030691,
            "range": "± 169007",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51181264,
            "range": "± 131257",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33671,
            "range": "± 1192",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 34356,
            "range": "± 1076",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 34849,
            "range": "± 1086",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 157,
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