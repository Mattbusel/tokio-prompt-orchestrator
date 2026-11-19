"""Turn nebius/SWE-agent-trajectories parquet into burn's agent-step JSONL.

Every `ai` turn is one model call. Its input is everything before it in the
trajectory (system prompt included), its action is the last fenced code
block of the turn (the command SWE-agent runs), and its observation is the
next `user` turn (what the environment returned). Tokens are estimated at
4 characters a token. Steps get synthetic, increasing timestamps (one
second apart) since the dataset has none; only their order matters here.

usage: python -E -P sweagent_to_jsonl.py OUT.jsonl SHARD.parquet [...]
"""
import json
import re
import sys

import duckdb

FENCE = re.compile(r"```[a-zA-Z]*\n(.*?)```", re.S)

out_path, shards = sys.argv[1], sys.argv[2:]
n = traces = 0
t = 0.0
with open(out_path, "w", encoding="utf-8") as out:
    for shard in shards:
        rel = duckdb.execute(
            "select instance_id, model_name, trajectory from read_parquet(?)", [shard]
        )
        while True:
            rows = rel.fetchmany(500)
            if not rows:
                break
            for inst, model, traj in rows:
                traces += 1
                trace = f"{inst}#{traces}"
                chars = 0
                for i, m in enumerate(traj):
                    text = m["text"] or m["system_prompt"] or ""
                    if m["role"] == "ai":
                        blocks = FENCE.findall(text)
                        action = (blocks[-1] if blocks else text).strip()
                        obs = ""
                        if i + 1 < len(traj) and traj[i + 1]["role"] == "user":
                            obs = (traj[i + 1]["text"] or "").strip()
                        t += 1.0
                        rec = {
                            "ts": t,
                            "trace_id": trace,
                            "request_hash": f"{trace}:{i}",
                            "model": model,
                            "action": action,
                            "observation": obs,
                            "usage": {"prompt_tokens": chars // 4, "completion_tokens": len(text) // 4},
                            "estimated_tokens": True,
                            "preview": action[:80],
                        }
                        out.write(json.dumps(rec, ensure_ascii=False) + "\n")
                        n += 1
                    chars += len(text)
print(n, "steps in", traces, "trajectories")
