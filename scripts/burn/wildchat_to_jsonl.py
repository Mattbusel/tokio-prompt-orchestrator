"""Turn WildChat-1M parquet shards into burn's request-log JSONL.

One record per model call: every user turn of every conversation is one
request whose body is the conversation up to and including that user turn
(what a chat app sends the API), answered by the next assistant turn.
Token counts are estimated from text length by burn (4 chars a token), so
no tokens are written. The request identity is a SHA-256 of model + the
message prefix, computed here so the JSONL stays small.

usage: python -E -P wildchat_to_jsonl.py OUT.jsonl SHARD.parquet [...]
"""
import hashlib
import json
import sys

import duckdb

out_path, shards = sys.argv[1], sys.argv[2:]
n = 0
with open(out_path, "w", encoding="utf-8") as out:
    for shard in shards:
        rel = duckdb.execute(
            "select model, hashed_ip, conversation from read_parquet(?)", [shard]
        )
        while True:
            rows = rel.fetchmany(2000)
            if not rows:
                break
            for model, ip, conv in rows:
                prefix = []
                chars = 0
                for i, m in enumerate(conv):
                    role, content = m["role"], m["content"] or ""
                    if role == "user":
                        prefix.append(["user", content])
                        chars += len(content)
                        reply = conv[i + 1] if i + 1 < len(conv) and conv[i + 1]["role"] == "assistant" else None
                        start = m["timestamp"] or (reply or {}).get("timestamp")
                        if start is None:
                            continue
                        end = reply["timestamp"] if reply and reply["timestamp"] else start
                        key = hashlib.sha256(
                            json.dumps([model, prefix], ensure_ascii=False).encode("utf-8")
                        ).hexdigest()[:24]
                        rec = {
                            "start_time": start.timestamp(),
                            "end_time": max(end.timestamp(), start.timestamp()),
                            "model": model,
                            "user": ip,
                            "request_hash": key,
                            # Token estimates from text (4 chars a token), as burn does.
                            "usage": {
                                "prompt_tokens": chars // 4,
                                "completion_tokens": len((reply or {}).get("content") or "") // 4,
                            },
                            "estimated_tokens": True,
                            "preview": content[:80],
                        }
                        out.write(json.dumps(rec, ensure_ascii=False) + "\n")
                        n += 1
                    elif role == "assistant":
                        prefix.append(["assistant", content])
                        chars += len(content)
print(n, "calls")
