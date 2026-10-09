# tokio-prompt-orchestrator

[English](README.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja.md) | 한국어

**Rust로 만든 LLM 요청 오케스트레이터입니다. 앱과 AI 모델(Anthropic, OpenAI, llama.cpp, vLLM) 사이에 위치해, 같은 프롬프트를 두 번 보내도 모델 호출은 한 번만 일어납니다. 프로바이더가 다운되면 요청이 계속 쌓이는 대신 명확한 이유와 함께 즉시 실패합니다(서킷 브레이커).**

앱, 에이전트, 스크립트에서 LLM API를 호출하면서 부하가 몰릴 때나 장애가 났을 때도 빠르고 예측 가능하게 동작하길 원하는 개발자를 위한 도구입니다. 바로 쓸 수 있는 서버(`orchestrator`)로 실행해도 되고, Rust 라이브러리로 가져다 써도 됩니다.

<p>
  <a href="https://crates.io/crates/tokio-prompt-orchestrator"><img alt="crates.io" src="https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg"></a>
  <a href="https://docs.rs/tokio-prompt-orchestrator"><img alt="docs.rs" src="https://img.shields.io/docsrs/tokio-prompt-orchestrator"></a>
  <a href="https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases"><img alt="release" src="https://img.shields.io/gitlab/v/release/mattbusel%2Ftokio-prompt-orchestrator"></a>
  <a href="LICENSE"><img alt="MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
</p>

<img alt="실제 세션 녹화. 1부: cargo run --example llm_pipeline 이 모델 호출 3번으로 요청 12개에 응답하고, 이어서 모의 장애 중에 호출 5번이 503으로 실패하자 서킷 브레이커가 열리고 다음 3개는 즉시 실패합니다. 2부: orchestrator --provider echo 가 프롬프트에 입력된 질문에 답하는 동안, 두 번째 터미널이 curl 로 프롬프트를 보내고 결과를 가져온 뒤 /health 를 확인합니다." src="assets/demo.gif" width="100%">

<sub>실제 녹화 영상이며, 기다리는 구간만 빠르게 돌렸습니다. <a href="https://tokio-prompt-orchestrator.vercel.app/">사이트</a>에서 같은 실행을 단계별로 다시 볼 수 있습니다.</sub>

## 설치

**Linux** (x86_64, Ubuntu 20.04+ / Debian 11+). 의존성 없이 한 줄로 `~/.local/bin`에 설치됩니다.

```sh
mkdir -p ~/.local/bin && curl -fsSL https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases/permalink/latest/downloads/orchestrator-linux-x86_64.tar.gz | tar xz --strip-components=1 -C ~/.local/bin --wildcards '*/orchestrator'
```

| 다른 운영체제 | |
|---|---|
| **Windows** | [orchestrator-windows-x86_64.exe 다운로드](https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases/permalink/latest/downloads/orchestrator-windows-x86_64.exe) 후 실행하세요. (서명되지 않은 파일이라 SmartScreen이 확인을 요청할 수 있습니다. *추가 정보*를 누른 다음 *실행*을 누르세요.) |
| **macOS 또는 소스에서 설치** | `cargo install --locked tokio-prompt-orchestrator --features web-api,tantivy` (`--semantic-dedup`을 쓰려면 `fastembed` 추가) |

어떤 방법을 쓰든 같은 `orchestrator` 명령이 설치됩니다. 모든 릴리스와 SHA-256 체크섬: [Releases](https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases).

## 그대로 갈아 끼우는 OpenAI 호환 프록시

OpenAI 클라이언트를 이미 쓰고 있는 앱이라면 코드를 한 줄도 바꾸지 않고 중복 제거, 서킷 브레이커, 타임아웃, 비용 상한, 데드 레터 큐를 얻을 수 있습니다. `orchestrator`를 실행하고 클라이언트의 base URL을 그쪽으로 지정하면 됩니다.

```sh
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
```

`orchestrator --provider echo`를 실행해 둔 상태에서(echo 모드는 프롬프트를 그대로 돌려주므로 API 키가 필요 없습니다), 공식 Python 클라이언트(`pip install openai`)를 수정 없이 그대로 사용한 예입니다.

```python
from openai import OpenAI

client = OpenAI()  # reads OPENAI_BASE_URL and OPENAI_API_KEY

reply = client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "Summarize this ticket"}],
)
print(reply.choices[0].message.content, reply.usage.total_tokens)

for chunk in client.chat.completions.create(
    model="gpt-4o-mini",
    messages=[{"role": "user", "content": "Now stream it"}],
    stream=True,
):
    if chunk.choices:
        print(chunk.choices[0].delta.content or "", end="")
print()
```
```text
Summarize this ticket 10
Now stream it
```

같은 질문을 `curl`로 다시 보내면 모델을 호출하지 않고 중복 제거 윈도우에서 바로 응답합니다.

```bash
curl -s -i localhost:8080/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "Summarize this ticket"}]}'
```
```text
x-orchestrator-dedup: cached
{"choices":[{"finish_reason":"stop","index":0,"logprobs":null,"message":{"content":"Summarize this ticket","refusal":null,"role":"assistant"}}],"created":1790804186,"id":"chatcmpl-4d5284a811bd4c0f90bee01018103e1a","model":"echo","object":"chat.completion","system_fingerprint":null,"usage":{"completion_tokens":5,"prompt_tokens":5,"total_tokens":10}}
```

요청에 어떤 `model`이 적혀 있든, 오케스트레이터는 실행할 때 지정한 프로바이더와 모델(예: `--provider openai --model gpt-4o-mini`)로 응답합니다. 오류는 OpenAI 형식으로 돌아옵니다. 브레이커가 열려 있는 동안은 503, 비용 상한에 도달하면 429입니다. 자세한 내용, 인증, 제한 사항은 [docs/REFERENCE.md](docs/REFERENCE.md#openai-compatible-api)를 참고하세요.

OpenAI 모델의 경우 `usage`의 토큰 수(그리고 이를 기반으로 하는 비용 상한)는 해당 모델 자체의 토크나이저로 계산하므로 OpenAI 청구서와 일치합니다. 다른 모델은 토큰당 약 4글자로 추정하며, 위의 `echo` 결과에 보이는 값이 바로 이 추정치입니다.

## 그대로 갈아 끼우는 Anthropic 프록시

Anthropic 클라이언트도 마찬가지입니다. base URL을 오케스트레이터로 지정하면 `POST /v1/messages`에도 같은 중복 제거, 서킷 브레이커, 비용 상한이 적용됩니다. 공식 Python SDK를 수정 없이 `orchestrator --provider echo`에 연결한 예입니다.

```python
import anthropic

client = anthropic.Anthropic(base_url="http://127.0.0.1:8080", api_key="local")
msg = client.messages.create(
    model="claude-sonnet-4-6",
    max_tokens=200,
    messages=[{"role": "user", "content": "Summarize this ticket"}],
)
print(msg.content[0].text, msg.stop_reason)

with client.messages.stream(model="claude-sonnet-4-6", max_tokens=200,
                            messages=[{"role": "user", "content": "Now stream it"}]) as s:
    print("".join(s.text_stream))
```
```text
Summarize this ticket end_turn
Now stream it
```

텍스트 대화, 시스템 프롬프트, 스트리밍을 지원합니다. 이미지 블록과 도구 블록은 명확한 `invalid_request_error`와 함께 거부됩니다. 키는 `x-api-key`(Anthropic SDK가 보내는 방식) 또는 `Authorization: Bearer`에 넣습니다.

## 내 문서를 바탕으로 답변하기

`orchestrator --docs ./my-docs`는 폴더 안의 Markdown 파일과 텍스트 파일을 인덱싱하고([tantivy](https://crates.io/crates/tantivy) BM25 검색, 영어 형태소 어간 추출, 메모리 내 인덱스), 가장 관련 있는 구절을 파일 이름과 함께 각 질문 앞에 붙여 보냅니다. 모델이 실제로 받는 내용을 그대로 보여 주는 `--provider echo`로 실행한 실제 결과입니다.

```text
  Answering from 2 passages in ./demo-docs
> How long do refunds take?
Use the following excerpts to answer. If they do not contain the answer, say so. [1] (policies/refunds.md) # Refunds Refunds are issued to the original card within 14 days of us receiving the return. Gift cards cannot be refunded. Question: How long do refunds take?
```

검색이 실패하거나 2초 넘게 걸리면 프롬프트는 컨텍스트 없이 전송됩니다. 검색 때문에 요청이 버려지는 일은 절대 없습니다. Rust에서는 `spawn_pipeline_with(worker, PipelineOptions::with_retriever(Arc::new(TantivyRetriever::index_dir("./docs")?)))`를 쓰거나, 직접 운영하는 검색(벡터 데이터베이스, Elasticsearch, Postgres) 위에 `Retriever` 트레이트를 구현하면 됩니다.

## 시맨틱 중복 제거: 같은 질문, 다른 표현

정확히 일치하는 중복 제거는 완전히 똑같은 프롬프트만 잡아냅니다. 임베더(embedder)를 설정하면 OpenAI와 Anthropic 엔드포인트가 표현을 바꾼 질문에도 캐시로 응답합니다(`x-orchestrator-dedup: semantic`, 유사도는 `x-orchestrator-similarity`에 담깁니다). `orchestrator --semantic-dedup`은 로컬 모델([fastembed](https://crates.io/crates/fastembed), BGE-small, API 키 불필요, 처음 한 번 128 MB를 캐시 디렉터리에 내려받음)을 사용합니다. Rust에서는 `Deduplicator::with_embedder`에 이 모델이나 OpenAI/genai 임베더를 넘기면 됩니다.

임베딩만으로는 안전하지 않습니다. BGE-small로 측정해 보니 "Convert 10 miles to kilometers"와 "Convert 10 kilometers to miles"의 점수가 0.99로, 저희가 시험해 본 어떤 진짜 바꿔 말하기보다도 높았습니다. 그래서 일치로 판정되려면 숫자가 같아야 하고, 공통 단어의 순서도 같아야 합니다. 테스트 쌍(`tests/semantic_dedup_tests.rs`)에 기본 임계값 0.93을 적용한 결과입니다.

| 쌍 | 유사도 | 답변 재사용 |
|---|---|---|
| "What is the capital of France?" / "Which city is the capital of France?" | 0.959 | 예 |
| "How many ounces are in a pound?" / "How many oz in one lb?" | 0.931 | 예 |
| "How do I reverse a list in Python?" / "What's the way to reverse a Python list?" | 0.985 | 아니요 (단어 순서가 달라서. 호출 1회 추가) |
| "Convert 10 miles to kilometers" / "Convert 10 kilometers to miles" | 0.992 | 아니요 |
| "Is 17 a prime number?" / "Is 21 a prime number?" | 0.845 | 아니요 |

애매하면 모델을 한 번 더 호출하는 쪽을 택하고, 다른 사람의 답을 돌려주는 쪽은 절대 택하지 않습니다. 임계값을 낮추기 전에 실제 트래픽에서 캐시 적중 내용을 먼저 확인하세요.

## 동작 방식

<img alt="실제 파이프라인을 그린 애니메이션 다이어그램. 요청은 input_tx.send() 또는 POST /api/v1/infer 로 들어와, 용량이 512, 512, 512, 1024, 512, 256인 유한(bounded) 채널로 이어진 다섯 단계(Retrieve, Assemble, Inference, Post-process, Stream)를 거칩니다. 3단계 안에서 모든 요청은 데드라인 확인, 서킷 브레이커(실패 5번이면 열리고, 60초 동안 호출을 거부한 뒤 프로브 하나를 통과시킴), 120초 타임아웃을 지나 여러분의 ModelWorker 에 도달합니다. Deduplicator, RetryPolicy, RateLimiter 는 worker 를 감싸는 선택적 래퍼입니다. 버려진 요청은 1000개 항목짜리 데드 레터 큐에 backpressure, deadline_expired, circuit_breaker_open, inference_timeout, inference_failure 같은 이유와 함께 들어갑니다. 애니메이션은 정상 트래픽을 보여 준 뒤, 브레이커가 열리고 호출이 즉시 실패해 데드 레터 큐로 들어가는 장애 상황을 보여 줍니다." src="assets/how-it-works.svg" width="100%">

각 단계는 별도의 Tokio 태스크입니다. 채널이 가득 차도 메모리는 늘어나지 않습니다. 요청은 이유와 함께 데드 레터 큐로 빠지고, HTTP API는 `Retry-After`와 함께 `429`를 응답합니다. 그림의 모든 내용은 [`src/stages.rs`](src/stages.rs)에서 그대로 가져왔습니다. 더 자세한 내용은 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)에 있습니다.

**잠깐의 오류에는 재시도.** 프로바이더는 가끔 1초 정도 실패합니다. 429, 503, 끊어진 연결 같은 경우입니다. `orchestrator --retries 2`로 실행하면(또는 파이프라인 설정에서 `retry_attempts`를 지정하면) 이런 호출은 실패로 집계되기 전에, 매번 두 배로 늘어나는 짧은 무작위 대기 후 다시 시도됩니다. 프로바이더의 `Retry-After`를 따르고, 잘못된 API 키는 절대 재시도하지 않으며, 한 요청의 모든 시도는 서킷 브레이커에서 한 번으로 계산됩니다. 기본값은 꺼져 있으므로 위의 브레이커 데모는 보이는 그대로 동작합니다.

**검증된 크레이트 위에 구축.** 미묘하게 잘못 만들기 쉬운 부분은 여기서 직접 작성한 코드가 아니라 널리 쓰이는 오픈소스 라이브러리를 사용합니다. 재시도 타이밍에는 [backon](https://crates.io/crates/backon), 중복 제거 캐시에는 [moka](https://crates.io/crates/moka)(크기 제한이 있어서 서로 다른 프롬프트가 쏟아져도 메모리가 끝없이 늘어나지 않습니다), OpenAI 토큰 수 계산에는 [tiktoken-rs](https://crates.io/crates/tiktoken-rs), `/metrics`에는 [prometheus](https://crates.io/crates/prometheus)를 씁니다.

## 예제

**1. 요청 12개, 모델 호출 3번, 그리고 장애.** `cargo run --example llm_pipeline`(API 키 불필요). 오늘 실제로 실행한 출력이며, 일부를 생략했습니다.

```text
1) 12 requests: 4 users x 3 questions
   req-01 alice  The capital of France is Paris.
   req-02 alice  Backpressure means a slow consumer makes fast producers wait instead of letting queues grow without bound.
   ...
   req-12 dave   Bounded channels fill / the sender waits its turn now / memory stays calm
   -> 12 answers in 0.9s, 3 model calls (9 saved by dedup)

2) Provider outage: 8 new requests while every call fails
   DLQ req-13  inference_failure:inference failed: 503 Service Unavailable (simulated outage)
   ...
   DLQ req-17  inference_failure:inference failed: 503 Service Unavailable (simulated outage)
   DLQ req-18  circuit open, failed fast (provider not called)
   DLQ req-19  circuit open, failed fast (provider not called)
   DLQ req-20  circuit open, failed fast (provider not called)
   -> breaker is Open; it lets one probe through after 60s to test recovery
```

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/banner-dark.png">
  <img alt="같은 실행을 그림으로 나타낸 것. 사용자 4명의 요청 12개가 중복 제거를 거쳐 모델 호출 3번이 되고, 모의 장애 중에는 호출 5번이 실패한 뒤 서킷 브레이커가 열려 3개를 더 거부하며, 8개 모두 데드 레터 큐에 들어갑니다." src="assets/banner-light.png" width="100%">
</picture>

`PROVIDER=anthropic ANTHROPIC_API_KEY=...` 또는 `PROVIDER=openai OPENAI_API_KEY=...`를 설정하면 같은 3번의 호출을 실제 모델에 보냅니다. 소스 코드인 [`examples/llm_pipeline.rs`](examples/llm_pipeline.rs)는 직접 만드는 백엔드의 템플릿으로 쓰기 좋습니다.

**2. 어떤 도구든 HTTP로 사용할 수 있습니다.** `orchestrator --provider echo`를 실행해 둔 상태에서(echo 모드는 모델이 받는 그대로의 프롬프트를 돌려줍니다):

```bash
curl -s -X POST localhost:8080/api/v1/infer -H 'Content-Type: application/json' -d '{"prompt": "Summarize this ticket"}'
```
```json
{"request_id":"d6a70800-99ef-4f2b-9b27-63b02af7d972","status":"processing"}
```
```bash
curl -s localhost:8080/api/v1/result/d6a70800-99ef-4f2b-9b27-63b02af7d972
```
```json
{"request_id":"d6a70800-99ef-4f2b-9b27-63b02af7d972","status":"completed","result":"Summarize this ticket"}
```
```bash
curl -s localhost:8080/health
```
```json
{"memory":{"rss_bytes":0,"rss_mb":0.0},"pipeline":{"circuit_breaker":{"is_open":false,"state":"closed"},"dead_letter_queue_depth":0,"inbound_queue":{"capacity":512,"depth_pct":0.0,"used":0}},"shutting_down":false,"status":"healthy","uptime_secs":2,"version":"2.0.0","worker_pool":{"pending_requests":0,"tracker_capacity":100000,"tracker_depth_pct":0.0,"tracker_used":1}}
```

**3. 내 Rust 코드에서 사용하기.** [`examples/quickstart.rs`](examples/quickstart.rs), `cargo run --example quickstart`:

```rust,no_run
use std::{collections::HashMap, sync::Arc};
use tokio_prompt_orchestrator::{spawn_pipeline, EchoWorker, ModelWorker, PromptRequest, SessionId};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Swap EchoWorker for AnthropicWorker, OpenAiWorker, LlamaCppWorker, VllmWorker,
    // or a worker over the client you already use (next section).
    let worker: Arc<dyn ModelWorker> = Arc::new(EchoWorker::new());
    let handles = spawn_pipeline(worker);
    let mut output = handles.take_output_rx().await.ok_or("output already taken")?;

    handles.input_tx.send(PromptRequest {
        session: SessionId::new("demo"),
        request_id: "req-1".into(),
        input: "Hello, pipeline!".into(),
        meta: HashMap::new(),
        deadline: None,
    }).await?;

    let answer = output.recv().await.ok_or("pipeline closed")?;
    println!("{}", answer.text);
    for dropped in handles.dlq.drain() { println!("dropped {}: {}", dropped.request_id, dropped.reason); }
    Ok(())
}
```

```text
Hello, pipeline!
```

`Cargo.toml`에 `tokio-prompt-orchestrator = "2"`와 `tokio = { version = "1", features = ["rt-multi-thread", "macros"] }`가 필요합니다.

## async-openai, genai, rig, tower를 이미 쓰고 있다면?

지금 쓰는 클라이언트를 그대로 두세요. 각각이 클라이언트를 worker로 바꿔 주는 cargo feature로 제공되므로, 중복 제거, 서킷 브레이커, 재시도, 타임아웃, 데드 레터 큐가 기존 코드 앞단에 놓입니다.

| 사용 중인 것 | 추가할 것 | 사용법 |
|---|---|---|
| [async-openai](https://crates.io/crates/async-openai) | `features = ["async-openai"]` | `AsyncOpenAiWorker::new(client, "gpt-4o-mini")`: OpenAI, Azure 또는 모든 OpenAI 호환 서버(Ollama, vLLM, llama.cpp, LM Studio) |
| [genai](https://crates.io/crates/genai) | `features = ["genai"]` | `GenaiWorker::new(genai::Client::default(), "claude-sonnet-4-6")`: OpenAI, Anthropic, Gemini, Ollama, Groq, DeepSeek, xAI 등. 모델 이름으로 자동 선택 |
| [rig](https://crates.io/crates/rig-core) | `features = ["rig"]` | `RigWorker::new(OpenAI::from_env()?.completion("gpt-5.2"))`: 모든 rig completion 모델(Rust 1.95+ 필요) |
| [tower](https://crates.io/crates/tower) | `features = ["tower"]` | `ServiceWorker::new(svc)`는 어떤 `Service<String>`이든(tower 레이어 포함) worker로 실행합니다. `WorkerService::new(worker)`는 그 반대입니다 |

```rust,ignore
use std::sync::Arc;
use async_openai::{config::OpenAIConfig, Client};
use tokio_prompt_orchestrator::{integrations::AsyncOpenAiWorker, spawn_pipeline};

// The client you already have, here pointed at a local Ollama.
let client = Client::with_config(
    OpenAIConfig::new().with_api_base("http://localhost:11434/v1").with_api_key("ollama"),
);
let handles = spawn_pipeline(Arc::new(AsyncOpenAiWorker::new(client, "llama3.2")));
```

네 가지 모두 모의(mock) OpenAI 서버를 상대로 엔드 투 엔드 테스트를 거쳤습니다([`tests/integrations_tests.rs`](tests/integrations_tests.rs)). 응답, 스트리밍, 그리고 거부된 키는 절대 재시도하지 않고 429에서는 백오프하는지를 확인합니다.

### Feature flags

feature를 하나도 켜지 않으면 라이브러리는 파이프라인, worker, 장애 대응(resilience) 구성 요소만 포함하며, 의존성 트리는 크레이트 167개입니다. 나머지는 모두 필요할 때 켜는 옵트인 방식입니다.

| Feature | 추가되는 것 |
|---|---|
| `web-api` | HTTP 서버: REST, SSE, WebSocket, OpenAI 호환 `/v1/chat/completions` |
| `otel` | `OTEL_EXPORTER_OTLP_ENDPOINT`가 설정되어 있으면 OTLP/HTTP로 OpenTelemetry 스팬 내보내기 |
| `tiktoken` | 정확한 OpenAI 토큰 수(`web-api`와 함께 켜짐) |
| `metrics-server`, `caching`, `rate-limiting` | `/metrics` 엔드포인트, Redis 결과 캐시, 토큰 버킷 레이트 리미터 |
| `full` | 위의 전부에 `cli`와 `hot-reload` 추가 |
| `async-openai`, `genai`, `rig`, `tower` | 위에서 소개한 통합 기능 |
| `tantivy` | `TantivyRetriever`와 `--docs`: 폴더 안의 문서에 근거한 답변(Rust 1.90+) |
| `fastembed` | `FastEmbedder`와 `--semantic-dedup`: 시맨틱 중복 제거용 로컬 임베딩(Rust 1.88+. Windows에서는 동적 C 런타임이 필요하므로 `+crt-static`과 함께 쓸 수 없음) |
| `tui`, `mcp`, `dashboard` | 터미널 대시보드, Claude Desktop과 Claude Code용 MCP 서버, 웹 대시보드 |
| `distributed` | Redis 기반 노드 간 중복 제거, NATS 작업 큐, 리더 선출 |
| `hot-reload`, `cli`, `schema`, `core-pinning` | 설정 파일 감시, `replay` 바이너리, JSON Schema 내보내기, CPU 고정(pinning) |
| `self-tune`, `self-modify`, `intelligence`, `evolution`, `self-improving` | 실험적인 자가 튜닝 단계들 |

## 3단계로 시작하기

1. **실행합니다.** `orchestrator --provider echo`는 키가 필요 없습니다. 터미널에 `>` 프롬프트와 웹 주소 `http://127.0.0.1:8080`이 표시됩니다.
2. **프롬프트를 보냅니다.** 터미널에서 입력하거나, 어떤 앱에서든 `POST /api/v1/infer`와 `GET /api/v1/result/<id>`로 보내면 됩니다(위의 예제 2).
3. **실제 모델에 연결합니다.** `orchestrator --reset`은 프로바이더와 키를 한 번 물어본 뒤 저장합니다. 직접 넘겨도 됩니다: `ANTHROPIC_API_KEY=sk-ant-... orchestrator --provider anthropic --model claude-sonnet-4-6`.

`orchestrator --help`로 모든 플래그를 볼 수 있습니다. 문제가 생기면 [문제 해결](docs/GUIDE.md#troubleshooting)을 참고하세요.

## 문서

| 읽을 문서 | 내용 |
|---|---|
| [docs/GUIDE.md](docs/GUIDE.md) | 서버 실행, HTTP API, 터미널 대시보드, Claude Desktop과 Claude Code(MCP), 배포, 튜닝, 문제 해결, 모든 예제 |
| [docs/REFERENCE.md](docs/REFERENCE.md) | 아키텍처, 장애 대응 구성 요소, 벤치마크, 설정 파일, 환경 변수, feature flags, API 표, 알려진 문제 |
| [docs/MODULES.md](docs/MODULES.md) | 모든 선택 모듈: 플러그인, 세션, 템플릿, A/B 테스트, 시맨틱 중복 제거, 캐시, 레이트 리미터, cron, DLQ 재처리 등 |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/configuration.md](docs/configuration.md) | 파이프라인 설계, 모든 설정 필드와 기본값 |
| [WEB_API.md](WEB_API.md), [BENCHMARKS.md](BENCHMARKS.md), [CHANGELOG.md](CHANGELOG.md) | HTTP 엔드포인트, 벤치마크 실행 결과, 릴리스 노트 |
| [docs.rs](https://docs.rs/tokio-prompt-orchestrator) | 전체 Rust API |

기여를 환영합니다. [CONTRIBUTING.md](CONTRIBUTING.md)를 참고하세요. MIT 라이선스이며, [LICENSE](LICENSE)를 확인하세요.

## 개발 의뢰

**이런 수준의 엔지니어링이 필요한 제품이 있으신가요?** 소수의 클라이언트 프로젝트를 고정 가격으로 맡고 있습니다. LLM 기능, iOS 앱, 성능 개선 작업입니다. [서비스 및 가격](https://mattbusel.vercel.app/) · [이메일](mailto:mattbusel@gmail.com) · [LinkedIn](https://www.linkedin.com/in/matthewbusel/)
