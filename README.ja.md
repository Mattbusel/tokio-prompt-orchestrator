# tokio-prompt-orchestrator

[English](README.md) | [简体中文](README.zh-CN.md) | 日本語 | [한국어](README.ko.md)

**Rust製のLLMリクエストオーケストレーター。アプリとAIモデル（Anthropic、OpenAI、llama.cpp、vLLM）の間に入り、同じプロンプトを2回送ってもモデル呼び出しは1回で済みます。プロバイダーが落ちたときは、リクエストが溜まり続けるのではなく、明確な理由付きで即座に失敗します（サーキットブレーカー）。**

アプリやエージェント、スクリプトからLLM APIを呼び出していて、高負荷時や障害時でも速く、予測どおりに動いてほしい開発者向けです。すぐに使えるサーバー（`orchestrator`）としても、Rustライブラリとしても使えます。

<p>
  <a href="https://crates.io/crates/tokio-prompt-orchestrator"><img alt="crates.io" src="https://img.shields.io/crates/v/tokio-prompt-orchestrator.svg"></a>
  <a href="https://docs.rs/tokio-prompt-orchestrator"><img alt="docs.rs" src="https://img.shields.io/docsrs/tokio-prompt-orchestrator"></a>
  <a href="https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases"><img alt="release" src="https://img.shields.io/gitlab/v/release/mattbusel%2Ftokio-prompt-orchestrator"></a>
  <a href="LICENSE"><img alt="MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
</p>

<img alt="実際のセッションの録画。パート1：cargo run --example llm_pipeline が12件のリクエストに3回のモデル呼び出しで応答し、その後の擬似障害では5回の呼び出しが503で失敗してサーキットブレーカーが開き、続く3件は即座に失敗します。パート2：orchestrator --provider echo がプロンプトに入力された質問に答え、同時に2つ目のターミナルが curl でプロンプトを送信し、結果を取得して /health を確認します。" src="assets/demo.gif" width="100%">

<sub>実際の録画で、待ち時間の部分だけ早送りしています。<a href="https://tokio-prompt-orchestrator.vercel.app/">サイト</a>では同じ実行をステップごとに再生できます。</sub>

## インストール

**Linux**（x86_64、Ubuntu 20.04+ / Debian 11+）。依存関係なしの1行で、`~/.local/bin`にインストールされます：

```sh
mkdir -p ~/.local/bin && curl -fsSL https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases/permalink/latest/downloads/orchestrator-linux-x86_64.tar.gz | tar xz --strip-components=1 -C ~/.local/bin --wildcards '*/orchestrator'
```

| その他のOS | |
|---|---|
| **Windows** | [orchestrator-windows-x86_64.exe をダウンロード](https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases/permalink/latest/downloads/orchestrator-windows-x86_64.exe)して実行します。（署名がないため、SmartScreenの確認が出たら「*詳細情報*」をクリックし、「*実行*」を選んでください。） |
| **macOS、またはソースから** | `cargo install --locked tokio-prompt-orchestrator --features web-api,tantivy`（`--semantic-dedup`を使う場合は`fastembed`も追加） |

どの方法でも、インストールされるのは同じ`orchestrator`コマンドです。全リリースとSHA-256チェックサムは[Releases](https://gitlab.com/mattbusel/tokio-prompt-orchestrator/-/releases)にあります。

## そのまま差し替えられるOpenAI互換プロキシ

OpenAIクライアントを使っているアプリなら、コードを一切変えずに重複排除、サーキットブレーカー、タイムアウト、利用額の上限、デッドレターキューが手に入ります。`orchestrator`を起動し、クライアントのベースURLをそこに向けるだけです。

```sh
export OPENAI_BASE_URL=http://127.0.0.1:8080/v1
```

`orchestrator --provider echo`を起動した状態で（echoモードはプロンプトをそのまま返すので、APIキーは不要です）、公式Pythonクライアント（`pip install openai`）を無修正で使った例です：

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

同じ質問を`curl`からもう一度送ると、モデルを呼ばずに重複排除ウィンドウから応答が返ります：

```bash
curl -s -i localhost:8080/v1/chat/completions -H 'Content-Type: application/json' \
  -d '{"model": "gpt-4o-mini", "messages": [{"role": "user", "content": "Summarize this ticket"}]}'
```
```text
x-orchestrator-dedup: cached
{"choices":[{"finish_reason":"stop","index":0,"logprobs":null,"message":{"content":"Summarize this ticket","refusal":null,"role":"assistant"}}],"created":1790804186,"id":"chatcmpl-4d5284a811bd4c0f90bee01018103e1a","model":"echo","object":"chat.completion","system_fingerprint":null,"usage":{"completion_tokens":5,"prompt_tokens":5,"total_tokens":10}}
```

リクエストの`model`に何が指定されていても、オーケストレーターは起動時に指定したプロバイダーとモデル（たとえば`--provider openai --model gpt-4o-mini`）で応答します。エラーはOpenAIの形式で返ります。ブレーカーが開いている間は503、利用額の上限に達したら429です。詳細、認証、制限については[docs/REFERENCE.md](docs/REFERENCE.md#openai-compatible-api)を参照してください。

OpenAIのモデルでは、`usage`のトークン数（つまり利用額の上限の計算も）はモデル自身のトークナイザーで数えるので、OpenAIの請求額と一致します。それ以外のモデルは1トークンあたり約4文字として見積もります。上の`echo`の出力に表示されているのはこの見積もりです。

## そのまま差し替えられるAnthropicプロキシ

Anthropicクライアントでも同じです。ベースURLをオーケストレーターに向ければ、`POST /v1/messages`にも同じ重複排除、サーキットブレーカー、利用額の上限が効きます。公式Python SDKを無修正のまま、`orchestrator --provider echo`に対して使った例です：

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

テキストの会話、システムプロンプト、ストリーミングに対応しています。画像ブロックとツールブロックは、わかりやすい`invalid_request_error`で拒否されます。キーは`x-api-key`（Anthropic SDKが送るのと同じ形）または`Authorization: Bearer`で渡します。

## 手元のドキュメントをもとに回答する

`orchestrator --docs ./my-docs`は、フォルダー内のMarkdownファイルとテキストファイルをインデックス化し（[tantivy](https://crates.io/crates/tantivy)によるBM25検索、英語のステミング付き、インメモリ）、関連度の高い箇所をファイル名とともに各質問の前に付けて送ります。モデルが実際に受け取る内容がそのまま見える`--provider echo`での実行結果です：

```text
  Answering from 2 passages in ./demo-docs
> How long do refunds take?
Use the following excerpts to answer. If they do not contain the answer, say so. [1] (policies/refunds.md) # Refunds Refunds are issued to the original card within 14 days of us receiving the return. Gift cards cannot be refunded. Question: How long do refunds take?
```

検索が失敗するか2秒以上かかった場合、プロンプトはコンテキストなしで送られます。検索が原因でリクエストが捨てられることはありません。Rustでは`spawn_pipeline_with(worker, PipelineOptions::with_retriever(Arc::new(TantivyRetriever::index_dir("./docs")?)))`と書くか、独自の検索基盤（ベクトルデータベース、Elasticsearch、Postgres）に対して`Retriever`トレイトを実装します。

## セマンティック重複排除：言い回しが違っても同じ質問

完全一致の重複排除で捕まえられるのは、まったく同じプロンプトだけです。埋め込みモデル（embedder）を設定すると、OpenAIとAnthropicのエンドポイントは言い換えられた質問にもキャッシュから応答します（`x-orchestrator-dedup: semantic`、類似度は`x-orchestrator-similarity`に入ります）。`orchestrator --semantic-dedup`はローカルモデル（[fastembed](https://crates.io/crates/fastembed)、BGE-small、APIキー不要、初回に128 MBをキャッシュディレクトリにダウンロード）を使います。Rustでは`Deduplicator::with_embedder`にこのモデルか、OpenAI/genaiのembedderを渡せます。

ただし、埋め込みだけに頼るのは安全ではありません。BGE-smallで実測すると、"Convert 10 miles to kilometers"と"Convert 10 kilometers to miles"のスコアは0.99で、私たちが試したどの本物の言い換えよりも高くなりました。そのため、一致と判定するには数値が同じで、共通する単語の並び順も同じである必要があります。テスト用のペア（`tests/semantic_dedup_tests.rs`）で、デフォルトのしきい値0.93を使った結果です：

| ペア | 類似度 | 回答を再利用 |
|---|---|---|
| "What is the capital of France?" / "Which city is the capital of France?" | 0.959 | する |
| "How many ounces are in a pound?" / "How many oz in one lb?" | 0.931 | する |
| "How do I reverse a list in Python?" / "What's the way to reverse a Python list?" | 0.985 | しない（語順が違うため。呼び出しが1回増える） |
| "Convert 10 miles to kilometers" / "Convert 10 kilometers to miles" | 0.992 | しない |
| "Is 17 a prime number?" / "Is 21 a prime number?" | 0.845 | しない |

迷ったときはモデルをもう1回呼ぶ側に倒し、他人の回答を返す側には決して倒しません。しきい値を下げる前に、自分のトラフィックでのヒット内容を確認してください。

## 仕組み

<img alt="実際のパイプラインのアニメーション図。リクエストは input_tx.send() または POST /api/v1/infer から入り、容量512、512、512、1024、512、256の有界チャネルでつながった5つのステージ（Retrieve、Assemble、Inference、Post-process、Stream）を通ります。ステージ3の中では、すべてのリクエストがデッドラインチェック、サーキットブレーカー（5回の失敗で開き、60秒間呼び出しを拒否し、その後1件のプローブを通す）、120秒のタイムアウトを経て、あなたの ModelWorker に届きます。Deduplicator、RetryPolicy、RateLimiter は worker を包むオプションのラッパーです。破棄されたリクエストは1000件分のデッドレターキューに、backpressure、deadline_expired、circuit_breaker_open、inference_timeout、inference_failure などの理由付きで入ります。アニメーションは正常なトラフィックのあと、ブレーカーが開いて呼び出しが即座に失敗し、デッドレターキューに入る障害の様子を示します。" src="assets/how-it-works.svg" width="100%">

各ステージはそれぞれ独立したTokioタスクです。チャネルがいっぱいになってもメモリは増えません。リクエストは理由とともにデッドレターキューへ退避され、HTTP APIは`Retry-After`付きの`429`を返します。図の内容はすべて[`src/stages.rs`](src/stages.rs)から読み取ったものです。詳しくは[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)を参照してください。

**一時的な不調にはリトライ。** プロバイダーは一瞬だけ失敗することがあります。429、503、接続の切断などです。`orchestrator --retries 2`で起動する（またはパイプライン設定で`retry_attempts`を指定する）と、こうした呼び出しは失敗として数えられる前に、毎回倍になるランダムな短い待ち時間をおいて再試行されます。プロバイダーの`Retry-After`は尊重され、不正なAPIキーは決してリトライされず、1つのリクエストの試行はすべてまとめてサーキットブレーカー上では1回と数えます。デフォルトではオフなので、上のブレーカーのデモは表示どおりに動きます。

**実績のあるクレートの上に構築。** 微妙なバグを生みやすい部分は、ここで自作したコードではなく、広く使われているオープンソースライブラリを使っています。リトライのタイミングには[backon](https://crates.io/crates/backon)、重複排除キャッシュには[moka](https://crates.io/crates/moka)（サイズ上限があるので、異なるプロンプトが大量に来てもメモリが際限なく増えることはありません）、OpenAIのトークン数には[tiktoken-rs](https://crates.io/crates/tiktoken-rs)、`/metrics`には[prometheus](https://crates.io/crates/prometheus)を使っています。

## 使用例

**1. 12件のリクエストを3回のモデル呼び出しで処理し、その後に障害が発生。** `cargo run --example llm_pipeline`（APIキー不要）。今日実際に出力されたものを一部省略して載せています：

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
  <img alt="同じ実行を図にしたもの。4人のユーザーからの12件のリクエストが重複排除によって3回のモデル呼び出しになり、擬似障害中は5回の呼び出しが失敗、サーキットブレーカーが開いてさらに3件を拒否し、8件すべてがデッドレターキューに入ります。" src="assets/banner-light.png" width="100%">
</picture>

`PROVIDER=anthropic ANTHROPIC_API_KEY=...`または`PROVIDER=openai OPENAI_API_KEY=...`を設定すれば、同じ3回の呼び出しを実際のモデルに対して行えます。ソースの[`examples/llm_pipeline.rs`](examples/llm_pipeline.rs)は、自分のバックエンドを作るときのテンプレートとしても使えます。

**2. HTTP経由でどんなツールからも使える。** `orchestrator --provider echo`を起動した状態で（echoモードは、モデルが受け取るのとまったく同じ形でプロンプトを返します）：

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

**3. 自分のRustコードから使う。** [`examples/quickstart.rs`](examples/quickstart.rs)、`cargo run --example quickstart`：

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

`Cargo.toml`に`tokio-prompt-orchestrator = "2"`と`tokio = { version = "1", features = ["rt-multi-thread", "macros"] }`が必要です。

## async-openai、genai、rig、towerをすでに使っている場合

今のクライアントはそのまま使えます。それぞれがcargo featureとして用意されていて、クライアントをworkerに変換します。これで、重複排除、サーキットブレーカー、リトライ、タイムアウト、デッドレターキューが既存のコードの手前に入ります。

| 使っているもの | 追加するもの | 使い方 |
|---|---|---|
| [async-openai](https://crates.io/crates/async-openai) | `features = ["async-openai"]` | `AsyncOpenAiWorker::new(client, "gpt-4o-mini")`：OpenAI、Azure、またはOpenAI互換の任意のサーバー（Ollama、vLLM、llama.cpp、LM Studio） |
| [genai](https://crates.io/crates/genai) | `features = ["genai"]` | `GenaiWorker::new(genai::Client::default(), "claude-sonnet-4-6")`：OpenAI、Anthropic、Gemini、Ollama、Groq、DeepSeek、xAIなど。モデル名で自動的に選ばれます |
| [rig](https://crates.io/crates/rig-core) | `features = ["rig"]` | `RigWorker::new(OpenAI::from_env()?.completion("gpt-5.2"))`：rigの任意のcompletionモデル（Rust 1.95+が必要） |
| [tower](https://crates.io/crates/tower) | `features = ["tower"]` | `ServiceWorker::new(svc)`は任意の`Service<String>`（towerのレイヤー込み）をworkerとして動かします。`WorkerService::new(worker)`はその逆です |

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

4つとも、モックのOpenAIサーバーに対してエンドツーエンドでテストしています（[`tests/integrations_tests.rs`](tests/integrations_tests.rs)）。応答、ストリーミング、そして拒否されたキーは決してリトライせず、429ではバックオフすることを確認しています。

### Feature flags

featureを何も有効にしない場合、ライブラリに含まれるのはパイプライン、worker、耐障害性の部品だけで、依存ツリーは167クレートです。それ以外はすべてオプトインです。

| Feature | 追加されるもの |
|---|---|
| `web-api` | HTTPサーバー：REST、SSE、WebSocket、OpenAI互換の`/v1/chat/completions` |
| `otel` | `OTEL_EXPORTER_OTLP_ENDPOINT`が設定されているとき、OTLP/HTTPでOpenTelemetryのスパンをエクスポート |
| `tiktoken` | OpenAIの正確なトークン数（`web-api`で有効） |
| `metrics-server`, `caching`, `rate-limiting` | `/metrics`エンドポイント、Redisによる結果キャッシュ、トークンバケット方式のレートリミッター |
| `full` | 上記すべてに加えて`cli`と`hot-reload` |
| `async-openai`, `genai`, `rig`, `tower` | 上で紹介した各インテグレーション |
| `tantivy` | `TantivyRetriever`と`--docs`：フォルダー内のドキュメントに基づいた回答（Rust 1.90+） |
| `fastembed` | `FastEmbedder`と`--semantic-dedup`：セマンティック重複排除用のローカル埋め込み（Rust 1.88+。Windowsでは動的Cランタイムが必要なため、`+crt-static`とは併用できません） |
| `tui`, `mcp`, `dashboard` | ターミナルダッシュボード、Claude DesktopとClaude Code向けのMCPサーバー、Webダッシュボード |
| `distributed` | Redisを使ったノード間の重複排除、NATSのワークキュー、リーダー選出 |
| `hot-reload`, `cli`, `schema`, `core-pinning` | 設定ファイルの監視、`replay`バイナリ、JSON Schemaのエクスポート、CPUピニング |
| `self-tune`, `self-modify`, `intelligence`, `evolution`, `self-improving` | 実験的な自己チューニング機能の各段階 |

## 3ステップで使う

1. **起動する。** `orchestrator --provider echo`ならキーは不要です。ターミナルに`>`プロンプトが表示され、Webアドレス`http://127.0.0.1:8080`も表示されます。
2. **プロンプトを送る。** ターミナルから入力するか、任意のアプリから`POST /api/v1/infer`と`GET /api/v1/result/<id>`で送ります（上の例2）。
3. **実際のモデルにつなぐ。** `orchestrator --reset`を実行すると、プロバイダーとキーを一度だけ聞かれて保存されます。直接渡すこともできます：`ANTHROPIC_API_KEY=sk-ant-... orchestrator --provider anthropic --model claude-sonnet-4-6`。

`orchestrator --help`ですべてのフラグを確認できます。うまく動かないときは[トラブルシューティング](docs/GUIDE.md#troubleshooting)を見てください。

## ドキュメント

| 読むもの | 内容 |
|---|---|
| [docs/GUIDE.md](docs/GUIDE.md) | サーバーの起動、HTTP API、ターミナルダッシュボード、Claude DesktopとClaude Code（MCP）、デプロイ、チューニング、トラブルシューティング、全サンプル |
| [docs/REFERENCE.md](docs/REFERENCE.md) | アーキテクチャ、耐障害性の構成要素、ベンチマーク、設定ファイル、環境変数、feature flags、API一覧、既知の問題 |
| [docs/MODULES.md](docs/MODULES.md) | すべてのオプションモジュール：プラグイン、セッション、テンプレート、A/Bテスト、セマンティック重複排除、キャッシュ、レートリミッター、cron、DLQの再実行など |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/configuration.md](docs/configuration.md) | パイプラインの設計、全設定項目とそのデフォルト値 |
| [WEB_API.md](WEB_API.md), [BENCHMARKS.md](BENCHMARKS.md), [CHANGELOG.md](CHANGELOG.md) | HTTPエンドポイント、ベンチマークの実行結果、リリースノート |
| [docs.rs](https://docs.rs/tokio-prompt-orchestrator) | Rust APIの完全なドキュメント |

コントリビュート歓迎です。[CONTRIBUTING.md](CONTRIBUTING.md)をご覧ください。MITライセンスです。[LICENSE](LICENSE)を参照してください。

## 作者に依頼する

**あなたのプロダクトにもこうしたエンジニアリングが必要ですか？** 少数ですがクライアントの開発を固定価格でお引き受けしています。LLM機能、iOSアプリ、パフォーマンス改善などです。[サービスと料金](https://mattbusel.vercel.app/) · [メール](mailto:mattbusel@gmail.com) · [LinkedIn](https://www.linkedin.com/in/matthewbusel/)
