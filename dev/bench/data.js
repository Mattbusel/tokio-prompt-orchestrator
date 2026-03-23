window.BENCHMARK_DATA = {
  "lastUpdate": 1774249584591,
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
          "id": "0fdae01335d3229203fdaee852e306dec8d527e2",
          "message": "Round 16: prompt versioning, multi-modal content\n\nAdd prompt_versioning.rs: PromptRepository with Wagner-Fischer LCS diff,\ncommit/checkout/rollback/blame/history_for_author, full unit tests.\n\nAdd multi_modal.rs: ContentPart enum (Text, ImageUrl, CodeSnippet,\nDataTable, AudioTranscript), MultiModalContent with token_estimate/\ntext_only/has_images/has_code/tables, ContentBuilder fluent API,\nContentSerializer (serde_json roundtrip), full unit tests.",
          "timestamp": "2026-03-23T03:01:44-04:00",
          "tree_id": "1b91827732c464a056a5e358dbefd344968d48f7",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/0fdae01335d3229203fdaee852e306dec8d527e2"
        },
        "date": 1774249583670,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11087566,
            "range": "± 110926",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51083486,
            "range": "± 107337",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51133982,
            "range": "± 141833",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51230634,
            "range": "± 160319",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 32991,
            "range": "± 978",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33207,
            "range": "± 990",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32653,
            "range": "± 1129",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 145,
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