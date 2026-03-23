window.BENCHMARK_DATA = {
  "lastUpdate": 1774250573511,
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
          "id": "72a7a1e548bdd152b318b1f00d54ee0f8192d5cb",
          "message": "Round 17: persona manager, chain-of-thought\n\nAdd src/persona_manager.rs: PersonaVoice enum with style_notes(), PersonaConstraints,\nPersona, PersonaManager (register/activate/apply_persona/validate_response/blend_personas/\nlist_by_voice), default_personas() factory, and full unit tests.\n\nAdd src/chain_of_thought.rs: ThoughtStep, ChainOfThought, CotStrategy (ZeroShot/FewShot/\nTreeOfThought/SelfConsistency), CotPromptBuilder (build_cot_prompt/few_shot_examples/\ntree_of_thought_prompt), CotParser (parse_steps/extract_final_answer/compute_confidence/\nparse), and full unit tests.\n\nUpdate src/lib.rs to export both new modules.",
          "timestamp": "2026-03-23T03:18:18-04:00",
          "tree_id": "7cd5b4574b8255c932866a9feca7b7ec887b53d1",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/72a7a1e548bdd152b318b1f00d54ee0f8192d5cb"
        },
        "date": 1774250573150,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11100244,
            "range": "± 125226",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51137940,
            "range": "± 159498",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51078617,
            "range": "± 128995",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51104585,
            "range": "± 148796",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 34108,
            "range": "± 842",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 34112,
            "range": "± 1392",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 36782,
            "range": "± 2020",
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