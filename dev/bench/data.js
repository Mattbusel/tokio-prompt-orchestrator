window.BENCHMARK_DATA = {
  "lastUpdate": 1774236247265,
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
          "id": "d473e40435ee81531c14c55edb5638eb8cf7dd0a",
          "message": "feat: add multi-model load balancer and prompt template engine (Round 4)\n\n- src/load_balancer.rs: weighted round-robin LoadBalancer with health\n  tracking (3 consecutive failures → unhealthy, 1 success → recover),\n  failover support, latency EMA, and LoadBalancerStats; 20 unit tests\n- src/template.rs: PromptTemplate {{variable}} engine with upper/lower/\n  truncate/default filters, TemplateContext with Text/Number/Bool/List\n  values, TemplateLibrary named store; 22 unit tests\n- src/lib.rs: pub mod load_balancer + pub mod template, re-exports for\n  both modules\n- src/web_api.rs: POST/GET /api/v1/templates, POST /api/v1/templates/:name/render,\n  GET /api/v1/load-balancer/stats endpoints; AppState gains template_library\n  and load_balancer fields\n- README.md: Load Balancer and Template Engine sections under v1.6.0",
          "timestamp": "2026-03-22T23:19:43-04:00",
          "tree_id": "ba47304b1a8a56ed672b21100a360f950aed1015",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/d473e40435ee81531c14c55edb5638eb8cf7dd0a"
        },
        "date": 1774236246751,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11191497,
            "range": "± 98609",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51213051,
            "range": "± 104905",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51060589,
            "range": "± 152765",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51127549,
            "range": "± 156717",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 34883,
            "range": "± 684",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 35145,
            "range": "± 726",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 35229,
            "range": "± 3513",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 181,
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
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      }
    ]
  }
}