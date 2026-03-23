window.BENCHMARK_DATA = {
  "lastUpdate": 1774231250025,
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
          "id": "983754a35b4a72ab2e5477d3437f661eadf23842",
          "message": "feat: plugin system, semantic router, session budget enforcement, expanded health check, prompt template engine, cascade failover\n\nNew modules and enhancements:\n\n1. plugin.rs — PluginContext, HookMetrics, InferenceHook trait; PluginRegistry\n   gains register_pre_hook, register_post_hook, unregister_pre_hook,\n   unregister_post_hook, run_pre_hooks (timeout-bounded, abort-on-error),\n   run_post_hooks (timeout-bounded, non-fatal logging).\n\n2. routing/semantic.rs — SemanticRouter with keyword heuristics for Code, Math,\n   Creative, Data, General categories; SemanticRouterConfig for custom worker\n   labels and min_match threshold; ClassificationResult with per-category scores.\n   Exported from routing mod.rs.\n\n3. session/budget.rs — SessionBudget with hard limit enforcement\n   (reject before inference), soft limit warnings, daily reset, per-session\n   snapshots. Exported from session mod.rs.\n\n4. web_api.rs — Expanded GET /health and GET /v1/health: pipeline queue depth %,\n   circuit breaker state, DLQ depth, worker pool stats, memory RSS, uptime_secs.\n   New endpoints: GET /v1/sessions/{id}/budget, POST /v1/sessions/{id}/budget/reset.\n   AppState gains started_at (Instant) and session_budget (Arc<SessionBudget>).\n\n5. templates.rs — Added {{#if condition}}…{{/if}} conditional blocks and\n   {{#each items}}…{{/each}} loop blocks (comma-separated list, {{this}},\n   {{@index}}) to PromptTemplate::render; 9 new tests covering all block types.\n\n6. routing/cascade.rs — CascadeFailover: named FailoverTier (premium → standard\n   → local) with per-tier timeout and retry count; call() drives tiers in order\n   with tokio::time::timeout per attempt, collects FailoverExhausted on total\n   failure. Exported from routing mod.rs.\n\nBug fixes (pre-existing errors):\n- enhanced/tournament.rs: fix worker.infer call signature (pass &req.input, not\n  req) and response type (Vec<String> tokens, not InferenceOutput)\n- multi_pipeline.rs: fix spawn_pipeline_with_config call (pass &config ref)",
          "timestamp": "2026-03-22T21:56:16-04:00",
          "tree_id": "a829d5fd16cd001b28ed9de3b4d354bc5aaaf623",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/983754a35b4a72ab2e5477d3437f661eadf23842"
        },
        "date": 1774231249657,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11091805,
            "range": "± 116034",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51116070,
            "range": "± 139125",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51058943,
            "range": "± 125451",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51118026,
            "range": "± 139087",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 33465,
            "range": "± 1875",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33222,
            "range": "± 2036",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 33354,
            "range": "± 1685",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 148,
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