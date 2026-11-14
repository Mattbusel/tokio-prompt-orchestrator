"""tokio-prompt-orchestrator vs LiteLLM proxy, same mock upstream, same machine.

Usage:
    python run.py --orchestrator /path/to/orchestrator [--litellm litellm] [--quick]

Every proxy forwards to mock_upstream.py (fixed 50 ms answer), so the numbers measure
each proxy's own cost: added latency, throughput, memory, startup, and how many upstream
calls a burst of repeated prompts turns into. Results print as a Markdown table and are
written to results.json.
"""
import argparse, asyncio, json, os, platform, statistics, subprocess, sys, time, uuid
import aiohttp, psutil

HERE = os.path.dirname(os.path.abspath(__file__))
MOCK = "http://127.0.0.1:9500"
KEY = "sk-bench-0123456789abcdef0123456789abcdef"
HDR = {"Authorization": "Bearer " + KEY}
BODY = lambda prompt: {"model": "gpt-4o-mini", "messages": [{"role": "user", "content": prompt}]}


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def tree_rss(proc):
    try:
        ps = [proc] + proc.children(recursive=True)
        return sum(p.memory_info().rss for p in ps if p.is_running())
    except psutil.Error:
        return 0


async def wait_ready(url, timeout=120):
    t0 = time.perf_counter()
    async with aiohttp.ClientSession(headers=HDR) as s:
        while time.perf_counter() - t0 < timeout:
            try:
                async with s.post(url + "/v1/chat/completions", json=BODY("warmup " + uuid.uuid4().hex),
                                  timeout=aiohttp.ClientTimeout(total=5)) as r:
                    if r.status == 200:
                        await r.read()
                        return time.perf_counter() - t0
            except Exception:
                pass
            await asyncio.sleep(0.05)
    raise RuntimeError(f"{url} not ready after {timeout}s")


async def load(url, prompts, concurrency):
    lat, errors = [], 0
    q = asyncio.Queue()
    for p in prompts:
        q.put_nowait(p)
    conn = aiohttp.TCPConnector(limit=concurrency)
    async with aiohttp.ClientSession(connector=conn, headers=HDR) as s:
        async def worker():
            nonlocal errors
            while not q.empty():
                p = q.get_nowait()
                t = time.perf_counter()
                try:
                    async with s.post(url + "/v1/chat/completions", json=BODY(p),
                                      timeout=aiohttp.ClientTimeout(total=60)) as r:
                        await r.read()
                        if r.status != 200:
                            errors += 1
                            continue
                except Exception:
                    errors += 1
                    continue
                lat.append((time.perf_counter() - t) * 1000)
        t0 = time.perf_counter()
        await asyncio.gather(*[worker() for _ in range(concurrency)])
        wall = time.perf_counter() - t0
    return lat, errors, wall


async def mock_calls(reset=False):
    async with aiohttp.ClientSession() as s:
        if reset:
            async with s.post(MOCK + "/reset") as r:
                return (await r.json())["calls"]
        async with s.get(MOCK + "/count") as r:
            return (await r.json())["calls"]


async def bench(name, url, proc, n):
    out = {"name": name}
    if proc is not None:
        out["startup_s"] = round(await wait_ready(url), 2)
        await asyncio.sleep(1)
        out["idle_rss_mb"] = round(tree_rss(proc) / 2**20, 1)
    peak = [0]
    stop = asyncio.Event()

    async def sampler():
        while not stop.is_set():
            if proc is not None:
                peak[0] = max(peak[0], tree_rss(proc))
            await asyncio.sleep(0.1)
    st = asyncio.create_task(sampler())

    # A: one request at a time, unique prompts -> pure added latency
    lat, err, _ = await load(url, [f"seq {i} {uuid.uuid4().hex}" for i in range(n["seq"])], 1)
    out["seq_p50_ms"], out["seq_p99_ms"], out["seq_err"] = round(pct(lat, 50), 2), round(pct(lat, 99), 2), err

    # B: 64 in flight, unique prompts -> throughput
    lat, err, wall = await load(url, [f"par {i} {uuid.uuid4().hex}" for i in range(n["par"])], 64)
    out["rps_c64"] = round(len(lat) / wall, 1)
    out["c64_p50_ms"], out["c64_p99_ms"], out["c64_err"] = round(pct(lat, 50), 2), round(pct(lat, 99), 2), err

    # C: repeated prompts (an agent fleet asking the same things) -> upstream calls made
    await mock_calls(reset=True)
    rep = [f"repeat question {i % 50}" for i in range(n["rep"])]
    lat, err, wall = await load(url, rep, 32)
    out["rep_requests"], out["rep_upstream_calls"], out["rep_err"] = len(rep), await mock_calls(), err
    out["rep_wall_s"] = round(wall, 2)

    stop.set()
    await st
    if proc is not None:
        out["peak_rss_mb"] = round(peak[0] / 2**20, 1)
    return out


def start(cmd, env=None):
    e = dict(os.environ)
    e.update(env or {})
    return subprocess.Popen(cmd, env=e, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, cwd=HERE)


def stop(p):
    try:
        pp = psutil.Process(p.pid)
        for c in pp.children(recursive=True):
            c.kill()
        pp.kill()
    except psutil.Error:
        pass
    p.wait(timeout=10)


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--orchestrator", required=True)
    ap.add_argument("--litellm", default="litellm")
    ap.add_argument("--quick", action="store_true")
    a = ap.parse_args()
    n = {"seq": 100, "par": 1000, "rep": 500} if a.quick else {"seq": 300, "par": 4000, "rep": 2000}

    mock = start([sys.executable, os.path.join(HERE, "mock_upstream.py"), "9500"])
    await wait_ready(MOCK)
    results = []
    try:
        results.append(await bench("direct to mock (no proxy)", MOCK, None, n))

        tpo = start([a.orchestrator, "--provider", "openai", "--model", "gpt-4o-mini", "--port", "8080",
                     "--concurrency", "256", "--log-level", "error"],
                    {"OPENAI_API_KEY": "sk-mock", "ORCHESTRATOR_UPSTREAM_URL": MOCK + "/v1", "ORCHESTRATOR_API_KEY": KEY})
        p = psutil.Process(tpo.pid)
        try:
            results.append(await bench("tokio-prompt-orchestrator", "http://127.0.0.1:8080", p, n))
        finally:
            stop(tpo)

        for label, cfg, extra in [("LiteLLM proxy", "litellm.yaml", []),
                                  ("LiteLLM proxy, 4 workers", "litellm.yaml", ["--num_workers", "4"]),
                                  ("LiteLLM proxy + local cache", "litellm-cache.yaml", [])]:
            ll = start([a.litellm, "--config", os.path.join(HERE, cfg), "--port", "4000"] + extra,
                       {"LITELLM_MASTER_KEY": KEY, "LITELLM_LOG": "ERROR", "LITELLM_TELEMETRY": "False", "PYTHONUTF8": "1", "PYTHONIOENCODING": "utf-8"})
            p = psutil.Process(ll.pid)
            try:
                results.append(await bench(label, "http://127.0.0.1:4000", p, n))
            finally:
                stop(ll)
    finally:
        stop(mock)

    import importlib.metadata as md
    meta = {"date": time.strftime("%Y-%m-%d"), "os": platform.platform(), "host_cpus": psutil.cpu_count(),
            "cpu_quota": os.environ.get("BENCH_CPUS", "unset"),
            "python": platform.python_version(), "litellm": md.version("litellm"),
            "mock_delay_ms": os.environ.get("MOCK_DELAY_MS", "50"), "requests": n}
    json.dump({"meta": meta, "results": results}, open(os.path.join(HERE, "results.json"), "w"), indent=2)
    cols = ["startup_s", "idle_rss_mb", "peak_rss_mb", "seq_p50_ms", "seq_p99_ms", "rps_c64", "c64_p99_ms",
            "rep_upstream_calls", "rep_wall_s", "seq_err", "c64_err", "rep_err"]
    print(json.dumps(meta))
    print("| proxy | " + " | ".join(cols) + " |")
    print("|---" * (len(cols) + 1) + "|")
    for r in results:
        print(f"| {r['name']} | " + " | ".join(str(r.get(c, "-")) for c in cols) + " |")


if __name__ == "__main__":
    asyncio.run(main())
