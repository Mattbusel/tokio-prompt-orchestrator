window.BENCHMARK_DATA = {
  "lastUpdate": 1774247819946,
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
          "id": "5652b103d22c5446e43d1e6513a8e5668165fa9b",
          "message": "Round 14: feedback loop, conversation graph\n\n- src/feedback_loop.rs: RL-style feedback collector with Feedback struct,\n  RewardSignal (quality*0.5 - cost*0.3 - latency*0.2), FeedbackStore ring\n  buffer (Arc<Mutex>), RewardModel (exponential decay weighted average), and\n  FeedbackLoop with record/best_variant/reward_history/convergence_score.\n  Full unit tests included.\n\n- src/conversation_graph.rs: DAG-based conversation branching with NodeId\n  newtype, ConversationNode, ConversationGraph (adjacency-list DAG) supporting\n  add_node, add_edge (cycle-rejecting DFS), branch_from, merge_paths,\n  linearize (Kahn's topological sort), path_tokens, and GraphError enum.\n  Full unit tests included.\n\n- src/lib.rs: registered both new modules.",
          "timestamp": "2026-03-23T02:32:25-04:00",
          "tree_id": "86662dd6894ae4a5d48ccd832e1a387fa08eb913",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/5652b103d22c5446e43d1e6513a8e5668165fa9b"
        },
        "date": 1774247819173,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11121496,
            "range": "± 137402",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51054223,
            "range": "± 170724",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51127909,
            "range": "± 119762",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51111177,
            "range": "± 198942",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37275,
            "range": "± 1052",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 36844,
            "range": "± 909",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36999,
            "range": "± 1374",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 173,
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