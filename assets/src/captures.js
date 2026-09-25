window.CAPTURES={
 "llm": {
  "title": "cargo run --example llm_pipeline",
  "cmd": "cargo run --example llm_pipeline",
  "out": "Backend: mock model (no network)\n\n1) 12 requests: 4 users x 3 questions\n   req-01 alice  The capital of France is Paris.\n   req-02 alice  Backpressure means a slow consumer makes fast producers wait instead of letting queues grow without bound.\n   req-03 alice  Bounded channels fill / the sender waits its turn now / memory stays calm\n   req-04 bob    The capital of France is Paris.\n   req-05 bob    Backpressure means a slow consumer makes fast producers wait instead of letting queues grow without bound.\n   req-06 bob    Bounded channels fill / the sender waits its turn now / memory stays calm\n   req-07 carol  The capital of France is Paris.\n   req-08 carol  Backpressure means a slow consumer makes fast producers wait instead of letting queues grow without bound.\n   req-09 carol  Bounded channels fill / the sender waits its turn now / memory stays calm\n   req-10 dave   The capital of France is Paris.\n   req-11 dave   Backpressure means a slow consumer makes fast producers wait instead of letting queues grow without bound.\n   req-12 dave   Bounded channels fill / the sender waits its turn now / memory stays calm\n   -> 12 answers in 1.0s, 3 model calls (9 saved by dedup)\n\n2) Provider outage: 8 new requests while every call fails\n   DLQ req-13  inference_failure:inference failed: 503 Service Unavailable (simulated outage)\n   DLQ req-14  inference_failure:inference failed: 503 Service Unavailable (simulated outage)\n   DLQ req-15  inference_failure:inference failed: 503 Service Unavailable (simulated outage)\n   DLQ req-16  inference_failure:inference failed: 503 Service Unavailable (simulated outage)\n   DLQ req-17  inference_failure:inference failed: 503 Service Unavailable (simulated outage)\n   DLQ req-18  circuit open, failed fast (provider not called)\n   DLQ req-19  circuit open, failed fast (provider not called)\n   DLQ req-20  circuit open, failed fast (provider not called)\n   -> breaker is Open; it lets one probe through after 60s to test recovery"
 },
 "web": {
  "title": "orchestrator --provider echo",
  "cmd": "orchestrator --provider echo",
  "out": "╔════════════════════════════════════════════════════╗\n║  tokio-prompt-orchestrator v1.4.0                  ║\n╠════════════════════════════════════════════════════╣\n║  Provider  : echo                                  ║\n║  Web API   : http://127.0.0.1:8080                 ║\n║  Auth      : open (no bearer token)                ║\n║  Log level : info                                  ║\n╠════════════════════════════════════════════════════╣\n║  HOW TO USE                                        ║\n║                                                    ║\n║  Terminal: type any question below                 ║\n║                                                    ║\n║  Agents and IDEs, over HTTP while this runs:       ║\n║    POST http://127.0.0.1:8080/api/v1/infer         ║\n║    GET  http://127.0.0.1:8080/api/v1/result/<id>   ║\n║    WS   ws://127.0.0.1:8080/v1/stream              ║\n║    GET  http://127.0.0.1:8080/health               ║\n║                                                    ║\n║  To change key or provider: run with --reset       ║\n╚════════════════════════════════════════════════════╝\n\nAsk me anything. Try: What can you help me with?\nCommands: 'exit' to quit  |  'settings' to change key/provider\n\n> Explain backpressure in one sentence.\n\nCONTEXT: Retrieved documents for 'Explain backpressure in one sentence.' User Query: Explain backpressure in one sentence. Assistant:\n\n> exit\nGoodbye."
 },
 "curl": {
  "title": "curl",
  "cmds": [
   [
    "curl -s -X POST http://127.0.0.1:8080/api/v1/infer \\\n  -H 'Content-Type: application/json' \\\n  -d '{\"prompt\":\"What is backpressure?\"}'",
    "{\"request_id\":\"f516415e-98b3-4e06-ad39-66da0c9306d9\",\"status\":\"processing\"}"
   ],
   [
    "curl -s http://127.0.0.1:8080/api/v1/result/f516415e-98b3-4e06-ad39-66da0c9306d9",
    "{\"request_id\":\"f516415e-98b3-4e06-ad39-66da0c9306d9\",\"status\":\"completed\",\"result\":\"CONTEXT: Retrieved documents for 'What is backpressure?' User Query: What is backpressure? Assistant:\"}"
   ]
  ]
 }
};
