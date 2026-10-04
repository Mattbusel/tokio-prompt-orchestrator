window.BENCHMARK_DATA = {
  "lastUpdate": 1791087064371,
  "repoUrl": "https://github.com/Mattbusel/tokio-prompt-orchestrator",
  "entries": {
    "Pipeline Benchmarks": [
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "f4fd847ad65434a19e8fb9cc365fce974b60fa8c",
          "message": "CI: do not cache the bench job's target dir (#10)\n\nA restored partial criterion/ directory made the bench job fail on main\n(Criterion looked for a missing base/sample.json and produced no results).",
          "timestamp": "2026-09-25T16:29:03-04:00",
          "tree_id": "659fd44a38f9bf4b177dd3071cb365267fe39327",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/f4fd847ad65434a19e8fb9cc365fce974b60fa8c"
        },
        "date": 1790368372037,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11148551,
            "range": "± 146498",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51051750,
            "range": "± 190150",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51152847,
            "range": "± 120339",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51063591,
            "range": "± 187687",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 25214,
            "range": "± 817",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 25186,
            "range": "± 499",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 25064,
            "range": "± 877",
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
            "value": 10,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 13,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "d9a33e1183362b32f21f7bd174b1b468163812a5",
          "message": "Site, README and visuals from a real llm_pipeline run; fix the web-api binary exiting at startup (#11)\n\n- orchestrator built with web-api (the release binaries) exited right after its\n  banner: the REPL found the output channel taken and ended the program. Split\n  pipeline output by request id so the REPL and HTTP API both work.\n- Banner and --help list the real HTTP routes and the stdio mcp binary; banner\n  box aligns; clear message when the port is in use.\n- New README: banner and terminal captures from real runs, theme-aware\n  architecture diagram, corrected endpoints, config and deployment notes,\n  long reference sections collapsed.\n- Project site in site/ with a replay of the recorded run; the Pages workflow\n  publishes it with rustdoc under /api/ and keeps /dev/bench/.\n- Social preview card at .github/social-preview.png.",
          "timestamp": "2026-09-25T18:49:39-04:00",
          "tree_id": "cfaa38d494552227fd6bd45d860cf9cdcafab5f3",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/d9a33e1183362b32f21f7bd174b1b468163812a5"
        },
        "date": 1790376863501,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11148255,
            "range": "± 142435",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51054346,
            "range": "± 161310",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51073036,
            "range": "± 149945",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51085970,
            "range": "± 118227",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 25137,
            "range": "± 490",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 25116,
            "range": "± 552",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 24889,
            "range": "± 535",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 122,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 10,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 12,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "7f0e2b9b0487e94f829e7c4451e246cda242702b",
          "message": "Release 1.4.1: release binaries keep serving after the banner (#12)",
          "timestamp": "2026-09-25T18:55:44-04:00",
          "tree_id": "6ea7a8360959daad2e960cb0ebcb4707f85b2ab0",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/7f0e2b9b0487e94f829e7c4451e246cda242702b"
        },
        "date": 1790377207987,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11192709,
            "range": "± 95609",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51184673,
            "range": "± 137140",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51049809,
            "range": "± 157798",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51116805,
            "range": "± 154809",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 32972,
            "range": "± 513",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 33203,
            "range": "± 450",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32830,
            "range": "± 331",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 156,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 13,
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
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fbe8cedd3840cb602edc7ad7ccd1d9f7065df455",
          "message": "1.4.2: one-line installs, demo GIF, README that starts with what it does, friendlier CLI errors (#13)",
          "timestamp": "2026-09-25T19:53:44-04:00",
          "tree_id": "67cebe34bf39b99374423676b68642ef0d792ee2",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/fbe8cedd3840cb602edc7ad7ccd1d9f7065df455"
        },
        "date": 1790380789456,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11092404,
            "range": "± 127611",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51158484,
            "range": "± 203991",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51082850,
            "range": "± 141051",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51033299,
            "range": "± 124956",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 31809,
            "range": "± 2978",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 31193,
            "range": "± 3165",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 30462,
            "range": "± 2829",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 210,
            "range": "± 3",
            "unit": "ns/iter"
          },
          {
            "name": "shard_session",
            "value": 9,
            "range": "± 0",
            "unit": "ns/iter"
          },
          {
            "name": "session_id_creation",
            "value": 11,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fbe8cedd3840cb602edc7ad7ccd1d9f7065df455",
          "message": "1.4.2: one-line installs, demo GIF, README that starts with what it does, friendlier CLI errors (#13)",
          "timestamp": "2026-09-25T19:53:44-04:00",
          "tree_id": "67cebe34bf39b99374423676b68642ef0d792ee2",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/fbe8cedd3840cb602edc7ad7ccd1d9f7065df455"
        },
        "date": 1790383710078,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11160876,
            "range": "± 193893",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51188919,
            "range": "± 160289",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51101403,
            "range": "± 171713",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51142408,
            "range": "± 216830",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 39679,
            "range": "± 1640",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 39516,
            "range": "± 1398",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 39654,
            "range": "± 1283",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 158,
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
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Vladislav Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "fe14c9cc219f331bc83240978263e1a525b5473a",
          "message": "CI: benchmark alerts comment instead of failing main (runner noise) (#15)",
          "timestamp": "2026-09-25T21:01:17-04:00",
          "tree_id": "b9c230d623881507d3790043966a799ef04c8e54",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/fe14c9cc219f331bc83240978263e1a525b5473a"
        },
        "date": 1790384753063,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11180484,
            "range": "± 105619",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51091150,
            "range": "± 166115",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51123392,
            "range": "± 157580",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51162128,
            "range": "± 177224",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 32062,
            "range": "± 448",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 31887,
            "range": "± 575",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32018,
            "range": "± 408",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 162,
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
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "distinct": true,
          "id": "11069edbdabf896e96697e9e5d06bbcc71d574e4",
          "message": "1.6.0: retries that actually happen, a bounded dedup cache, exact OpenAI token counts\n\n- backon: transient model errors (429, 5xx, network) are retried with jittered exponential\n  backoff, honouring Retry-After up to the cap; auth, config and budget errors never retry, and\n  all tries of one request count once for the circuit breaker. Before this, the [resilience]\n  retry_attempts / retry_base_ms / retry_max_ms settings were read and ignored. New CLI flag\n  --retries N (ORCHESTRATOR_RETRIES), spawn_pipeline_with_retry, InferenceRetry, and the\n  orchestrator_inference_retries_total metric. Retries stay off by default in the CLI.\n- moka: the deduplicator was an unbounded map swept once a minute, so a flood of distinct\n  prompts could grow memory without limit. Now at most 100,000 keys, expired entries never\n  served, no sweeper task, two race bugs fixed; dedup keys move from 64-bit FNV to 128-bit\n  SHA-256 so different prompts cannot collide.\n- tiktoken-rs: exact OpenAI token counts for the proxy's usage, the spend cap and\n  CostEstimator (new tiktoken feature, on with web-api); other models keep the estimate.\n- prometheus 0.14 without protobuf, which clears RUSTSEC-2024-0437; std LazyLock replaces\n  lazy_static.\n\nBehaviour changes are in the CHANGELOG: config files that set retry_attempts now really retry,\ndedup key format changed, retry_inference no longer retries AuthFailed/ConfigError/Other.\nTests: 1386 lib tests pass (2356 with full, tui, mcp, self-improving, schema, dashboard,\nzipkin, datadog), 132 doc tests, all previously compiling integration suites pass.",
          "timestamp": "2026-10-03T00:07:49-04:00",
          "tree_id": "9a58c807f888f90dee9e997140f056d7c2c5087e",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/11069edbdabf896e96697e9e5d06bbcc71d574e4"
        },
        "date": 1791000726446,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11188778,
            "range": "± 107058",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51249266,
            "range": "± 129239",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51131763,
            "range": "± 133449",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51111903,
            "range": "± 139931",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 32915,
            "range": "± 373",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 32771,
            "range": "± 808",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 32881,
            "range": "± 328",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 176,
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
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "distinct": true,
          "id": "a55ef733fd391c78f238346f8a5bc396291c09e9",
          "message": "2.0.0: Anthropic endpoint, semantic dedup, answers from your documents, and it plugs into async-openai, genai, rig and tower\n\nNew\n- POST /v1/messages: the Anthropic Messages API (text, system prompt, streaming with the full event sequence, x-api-key auth) with the same dedup, circuit breaker and spend cap as the OpenAI endpoint. Verified with the official anthropic Python SDK (messages.create and messages.stream) against the real binary.\n- Semantic dedup with real embeddings: Embedder trait, FastEmbedder (fastembed, local BGE-small, no API key), OpenAiEmbedder, GenaiEmbedder. Deduplicator::with_embedder / check_and_register_semantic, ServerConfig::embedder, orchestrator --semantic-dedup. Measured on BGE-small: \"Convert 10 miles to kilometers\" vs \"Convert 10 kilometers to miles\" scores 0.99, above every real paraphrase, so a hit must also pass same_specifics (same numbers, shared words in the same order). On the test pairs at 0.93: 4 of 5 paraphrases reuse the answer, 0 of 5 different questions do.\n- Retrieval: Retriever trait, TantivyRetriever (tantivy BM25 with English stemming over a folder of Markdown/text), spawn_pipeline_with + PipelineOptions, orchestrator --docs <folder>. Retrieval errors and timeouts send the prompt without context, never drop it. Release binaries include it.\n- Integrations, each a cargo feature and a ModelWorker: AsyncOpenAiWorker, GenaiWorker, RigWorker, and tower ServiceWorker / WorkerService. Tested end to end against a mock OpenAI server (reply, stream, 401 not retried, 429 backs off).\n- web_api::serve_pipeline, DeadLetterQueue::snapshot.\n\nFixed\n- Prompts reach the model as written. The Retrieve stage wrapped every prompt from the CLI, REST, WebSocket and MCP paths in invented \"CONTEXT: Retrieved documents for ... User Query: ... Assistant:\" text and slept 5 ms per request.\n- ServerConfig::default bound 0.0.0.0 with debug endpoints on and no key: now 127.0.0.1, debug off.\n- Partial ServerConfig JSON/TOML failed to load; the DLQ debug endpoint and MCP dump_dlq drained and re-pushed the queue; semantic dedup returned an empty answer on a hit and its store grew without limit; stage spans were held across .await.\n- cargo-binstall pointed at GitHub-style URLs that redirect to a sign-in page on GitLab; verified against the 1.5.0 release for Linux and Windows.\n- Four web examples and seven test suites no longer compiled; CI only ran --lib. CI now runs every test target, example and doctest. Eighteen self_modify tests that run cargo on the repository are #[ignore].\n\nChanged (breaking)\n- Default build 219 -> 167 crates: OpenTelemetry behind `otel` (no gRPC stack), notify behind `hot-reload`, clap behind `cli`.\n- OrchestratorError is #[non_exhaustive]; axum 0.8, tower 0.5, tower-http 0.6.\n- Minimum Rust 1.88 (1.85 no longer built with current dependencies). Feature minimums: tantivy 1.90, rig 1.95, fastembed 1.88.\n\nTests: 3069 passed, 0 failed (lib, integration, examples; 21 ignored on purpose) and 135 doctests with full tui mcp self-improving distributed schema tower async-openai genai tantivy; rig end to end on Rust 1.98.1; fastembed real-model tests passed. cargo clippy --all-targets: 0 errors.",
          "timestamp": "2026-10-04T00:00:39-04:00",
          "tree_id": "eabfdd657566d09f4bfb2f54f85ffe68a7b55f38",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/a55ef733fd391c78f238346f8a5bc396291c09e9"
        },
        "date": 1791086690762,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11168784,
            "range": "± 127303",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51148801,
            "range": "± 146558",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51199168,
            "range": "± 187180",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51279807,
            "range": "± 286564",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 37903,
            "range": "± 1198",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 37558,
            "range": "± 1165",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 37854,
            "range": "± 1915",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 148,
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
            "value": 16,
            "range": "± 0",
            "unit": "ns/iter"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "committer": {
            "email": "mattbusel@gmail.com",
            "name": "Matthew Charles Busel",
            "username": "Mattbusel"
          },
          "distinct": true,
          "id": "d86c02869f805eb38cc014629835f6dff120aea2",
          "message": "CI: build test binaries without debug info so the full-feature test job fits on the runner\n\nThe 2.0.0 test job linked ~60 test binaries with every feature and the\nrunner's linker died with SIGBUS (disk full). No test had run; the suite\npasses locally (3069 tests, 0 failed).",
          "timestamp": "2026-10-04T00:06:59-04:00",
          "tree_id": "51e79886a2f984e27effdabd397bec32ec6ee15e",
          "url": "https://github.com/Mattbusel/tokio-prompt-orchestrator/commit/d86c02869f805eb38cc014629835f6dff120aea2"
        },
        "date": 1791087062621,
        "tool": "cargo",
        "benches": [
          {
            "name": "full_pipeline_echo_worker",
            "value": 11195074,
            "range": "± 140333",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/10",
            "value": 51286058,
            "range": "± 143581",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/50",
            "value": 51154352,
            "range": "± 154791",
            "unit": "ns/iter"
          },
          {
            "name": "pipeline_throughput/requests/100",
            "value": 51198914,
            "range": "± 162308",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/512",
            "value": 32122,
            "range": "± 655",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/1024",
            "value": 31924,
            "range": "± 530",
            "unit": "ns/iter"
          },
          {
            "name": "channel_send/capacity/2048",
            "value": 31811,
            "range": "± 397",
            "unit": "ns/iter"
          },
          {
            "name": "send_with_shed_normal",
            "value": 171,
            "range": "± 2",
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