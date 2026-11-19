# You pay for the same prompt twice: what `burn` found in public data

November 19, 2026. Every number below comes from running `burn` (tokio-prompt-orchestrator 3.0.0) on public data. Token counts in both datasets are estimated at 4 characters per token, because neither dataset records them; the counts of repeated calls are exact.

## 1. Agents pay again for the same mistake

**Data:** [nebius/SWE-agent-trajectories](https://huggingface.co/datasets/nebius/SWE-agent-trajectories), shard `train-00000-of-00012` (6,669 SWE-agent runs on real GitHub issues, 174,815 model calls). Each `ai` turn is one call; its input is everything before it, its action is the command it ran, and its observation is what came back.

**What `burn` counts:** a step that repeated the previous action and got the identical observation back. The call was paid for and the agent learned nothing.

| | |
|---|---|
| Calls that repeated the last action and got the same result | **12,533 of 174,815 (7.2%)** |
| Share of input and output tokens they used | **10.9%** (they come late, in long contexts) |
| Runs with at least one | 1,176 of 6,669 (18%) |
| Most common repeated observation | "Your proposed edit has introduced new syntax error(s)", the same rejected edit sent again: 6,238 of the 12,533 |
| Cost at Llama 3.1 70B rates ($0.72 / $0.72 per 1M, the model family these runs used) | $154 of $1,406 |
| Same tokens at Claude Sonnet 4.5 list price ($3 / $15) | $662 of $6,090 |

The harness had the information to refuse the call: the action and the error were byte-for-byte the ones it had just seen.

Reproduce:

```sh
python scripts/burn/sweagent_to_jsonl.py steps.jsonl train-00000-of-00012.parquet
burn --price swe-agent-llama-70b=0.72,0.72 --price swe-agent-llama-8b=0.72,0.72 --price swe-agent-llama-405b=0.72,0.72 steps.jsonl
```

## 2. Chat apps pay again for the same request

**Data:** [allenai/WildChat-1M](https://huggingface.co/datasets/allenai/WildChat-1M), shards 0 and 1 of 14: 334,731 real calls (gpt-3.5-turbo-0301 and gpt-4-0314) from one public chat app on one API key, April to May 2023. A request is the conversation up to and including a user turn, timed by its answer.

| Window | Identical requests after an answer | Cost at 2023 list prices |
|---|---|---|
| 5 minutes, whole app (one shared cache) | **5,097** | **$62.53 of $4,245 (1.5%)** |
| 5 minutes, same user only | 4,605 | $55.43 (1.3%) |
| 1 hour, whole app | 8,380 | $99.39 (2.3%) |

Prices: gpt-3.5-turbo $2 / $2 per 1M tokens, gpt-4 (8K) $30 / $60, OpenAI's list prices at the time. The single most repeated gpt-4 request was sent 17 times, 11 of them within 5 minutes of an identical answer.

Reproduce:

```sh
python scripts/burn/wildchat_to_jsonl.py calls.jsonl train-00000-of-00014.parquet train-00001-of-00014.parquet
burn --price gpt-3.5-turbo-0301=2,2 --price gpt-4-0314=30,60 calls.jsonl
burn --per-user --price gpt-3.5-turbo-0301=2,2 --price gpt-4-0314=30,60 calls.jsonl
```

This is the smaller number of the two, and it is still money spent on answers the app already had. Most of these are users pressing regenerate or resubmitting; a client that wants a new sample can say so (`x-orchestrator-fresh: 1`), and everything else should be answered from the cache.

## 3. Retry code in agent frameworks: capped by count, not by money

`burn audit` scans retry code for three patterns: a tenacity retry with no stop, a retry on every exception (so a 400, 401 or context-length error is sent again), and a retry loop whose catch-all never exits. It is a pattern scan; each finding below was read and confirmed by hand at the commit shown.

| Framework | Where | What happens |
|---|---|---|
| MetaGPT | `metagpt/actions/write_code.py:94`, `write_code_review.py:147`, `action_node.py:423` | `@retry(stop=stop_after_attempt(6))` with no `retry=` filter around an LLM call: any exception, a 400 included, re-sends the same prompt up to 6 times. It sits on `provider/openai_api.py:165` (6 attempts on `APIConnectionError`, which includes `APITimeoutError`) and the OpenAI SDK's default 2 retries: one code-writing step can make 6 x 6 x 3 = 108 requests during a timeout outage. |
| LlamaIndex (Perplexity LLM) | `llama-index-llms-perplexity/.../perplexity/base.py:217` (and 243, 274, 306, 324, 379, 430, 485) | `@retry(stop=stop_after_attempt(self.max_retries), wait=wait_fixed(1))`, no exception filter, `max_retries` defaults to 10 (line 106), and `_complete` raises on every HTTP error (line 207): a bad key or a bad model name is sent 10 times, one second apart. |
| LlamaIndex (SiliconFlow LLM) | `llama-index-llms-siliconflow/.../siliconflow/base.py:144` | `retry=tenacity.retry_if_exception_type(Exception)`: every error is retried, 4xx included. |
| LangChain | `libs/core/langchain_core/runnables/base.py:2157` | `Runnable.with_retry()` defaults to `retry_if_exception_type=(Exception,)`, 3 attempts; on a chat model whose SDK already retries twice, that is up to 9 requests per call, and a 400 is sent 3 times. The docstring says so ("By default all exceptions are retried"). |
| LiteLLM | `litellm/main.py:6903-6914` | `completion_with_retries` retries every exception (`retry_if_exception(_is_not_resent_failure)`), 3 attempts by default, a 401 or 400 included. It does turn the SDK's own retries off (line 6898), so the attempts do not multiply. |
| AutoGPT platform | `autogpt_platform/backend/backend/util/request.py:611, 655-665` | `retry_max_attempts` defaults to `None`, and then the tenacity retry on throttled statuses has no `stop`: a 429 is retried with no attempt limit, waits growing to 300 s. |
| Instructor | `instructor/v2/core/retry.py:207-216, 361` | Validation failures are re-asked with the failed answer and the error appended: each retry is a full call with a longer prompt than the last, capped by count (`max_retries`) and never by cost. |

The scan of 26 frameworks (shallow clones of their default branches on November 19, 2026) flagged 130 sites, 102 of them outside tests and examples; the table is the subset read and confirmed by hand on LLM call paths. It found no such pattern in the library code of aider, OpenHands, SWE-agent, Haystack, Letta, AutoGen, smolagents, Semantic Kernel and rig. CAMEL's chat agent, agno's agent runner, smolagents and LlamaIndex's document-context extractor catch every exception in their retry loops but filter inside the catch (rate limits only, or a retry predicate), which `burn audit` does not flag.

None of the frameworks above caps retries in dollars. A retry count says nothing about money: 3 retries of a 2,000-token prompt are a fraction of a cent, 3 retries of a 200,000-token agent context are dollars, and during an outage every request in the fleet does it at once.

## What 3.0 changes

- An identical request inside the 5-minute window is answered from the cache, whatever its temperature. Ask for a fresh sample with `x-orchestrator-fresh: 1`.
- Every retry is priced at the model's list price and paid from a dollar budget ($0.50 an hour plus 10% of real spend by default) before it is sent. A 4xx is never retried.
- The spend cap reserves each call's worst case before it is sent.
