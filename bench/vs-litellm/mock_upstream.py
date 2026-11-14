"""Fake OpenAI server: answers every chat completion after a fixed delay and counts calls.

GET /count returns {"calls": N}; POST /reset sets it to 0. Used by run.py so both proxies
talk to the same upstream and their own overhead is the only difference.
"""
import asyncio, json, os, sys, time
from aiohttp import web

DELAY = float(os.environ.get("MOCK_DELAY_MS", "50")) / 1000
calls = 0

async def chat(request):
    global calls
    await request.read()
    calls += 1
    await asyncio.sleep(DELAY)
    return web.json_response({
        "id": f"chatcmpl-mock{calls}", "object": "chat.completion", "created": int(time.time()),
        "model": "gpt-4o-mini",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "The answer is forty-two, give or take."}}],
        "usage": {"prompt_tokens": 12, "completion_tokens": 9, "total_tokens": 21},
    })

async def count(_):
    return web.json_response({"calls": calls})

async def reset(_):
    global calls
    calls = 0
    return web.json_response({"calls": 0})

async def models(_):
    return web.json_response({"object": "list", "data": [{"id": "gpt-4o-mini", "object": "model"}]})

app = web.Application()
app.add_routes([web.post("/v1/chat/completions", chat), web.get("/v1/models", models),
                web.get("/count", count), web.post("/reset", reset)])
web.run_app(app, host="127.0.0.1", port=int(sys.argv[1]) if len(sys.argv) > 1 else 9500, print=None,
            access_log=None)
