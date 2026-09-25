"""Writes assets/architecture-light.svg and assets/architecture-dark.svg.

Everything in the drawing is read off src/stages.rs (spawn_pipeline):
channel capacities, the circuit breaker settings, the DLQ size and the
reasons a request can be dropped. Run from the repo root:
    python assets/src/architecture.py
"""

THEMES = {
    "light": dict(bg="#f5f3ee", panel="#fffdf8", ink="#17191c", muted="#676b72", faint="#d9d5cb",
                  band="#ebe7de", accent="#0d8f78", bad="#d8452f", gold="#c28a06"),
    "dark": dict(bg="#0f1113", panel="#15181b", ink="#ebe8e1", muted="#8d9199", faint="#2b3036",
                 band="#1b1f23", accent="#2cc7a6", bad="#ff6a4d", gold="#f0b429"),
}

MONO = "ui-monospace, 'Cascadia Mono', 'SF Mono', Menlo, Consolas, monospace"

STAGES = [
    ("1", "retrieve", "RAG context"),
    ("2", "assemble", "build prompt"),
    ("3", "inference", "breaker + timeout"),
    ("4", "post-process", "filter, format"),
    ("5", "stream", "output sink"),
]
# capacity of the channel feeding each stage, then the output channel
CAPS = [512, 512, 512, 1024, 512, 256]
DROPS = ["backpressure:rag", "backpressure:assemble",
         "circuit_breaker_open\ninference_failure\ninference_timeout\ndeadline_expired\nbackpressure:inference",
         "backpressure:post", None]


def svg(t):
    W, H = 1200, 560
    o = []
    a = o.append
    a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" '
      f'font-family="{MONO}" role="img" aria-label="Five bounded stages; dropped requests go to the dead-letter queue">')
    a(f'<rect width="{W}" height="{H}" rx="14" fill="{t["bg"]}"/>')
    a(f'<defs><marker id="ar" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
      f'<path d="M0,1 L9,5 L0,9 z" fill="{t["ink"]}"/></marker>'
      f'<marker id="arb" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">'
      f'<path d="M0,1 L9,5 L0,9 z" fill="{t["bad"]}"/></marker></defs>')

    a(f'<text x="40" y="44" font-size="15" font-weight="700" fill="{t["ink"]}">spawn_pipeline(worker)</text>')
    a(f'<text x="40" y="64" font-size="12" fill="{t["muted"]}">five Tokio tasks joined by bounded mpsc channels; a full channel sheds to the DLQ instead of growing</text>')

    y, bw, bh = 150, 150, 70
    xs = [200 + i * 186 for i in range(5)]
    # input
    a(f'<text x="40" y="{y + 32}" font-size="12" fill="{t["ink"]}">PromptRequest</text>')
    a(f'<text x="40" y="{y + 48}" font-size="11" fill="{t["muted"]}">input_tx</text>')
    for i, (n, name, sub) in enumerate(STAGES):
        x = xs[i]
        x_prev_end = 146 if i == 0 else xs[i - 1] + bw
        # channel
        cx0, cx1 = x_prev_end + 6, x - 6
        a(f'<line x1="{cx0}" y1="{y + bh / 2}" x2="{cx1}" y2="{y + bh / 2}" stroke="{t["ink"]}" stroke-width="1.6" marker-end="url(#ar)"/>')
        a(f'<text x="{(cx0 + cx1) / 2}" y="{y + bh / 2 - 9}" font-size="11" text-anchor="middle" fill="{t["muted"]}">{CAPS[i]}</text>')
        hot = name == "inference"
        a(f'<rect x="{x}" y="{y}" width="{bw}" height="{bh}" rx="9" fill="{t["panel"]}" stroke="{t["accent"] if hot else t["ink"]}" stroke-width="{2.2 if hot else 1.4}"/>')
        a(f'<text x="{x + 14}" y="{y + 28}" font-size="11" fill="{t["muted"]}">stage {n}</text>')
        a(f'<text x="{x + 14}" y="{y + 48}" font-size="15" font-weight="700" fill="{t["ink"]}">{name}</text>')
        a(f'<text x="{x + 14}" y="{y + 63}" font-size="10.5" fill="{t["muted"]}">{sub}</text>')
    # output
    xe = xs[-1] + bw
    a(f'<line x1="{xe + 6}" y1="{y + bh / 2}" x2="{xe + 60}" y2="{y + bh / 2}" stroke="{t["ink"]}" stroke-width="1.6" marker-end="url(#ar)"/>')
    a(f'<text x="{xe + 33}" y="{y + bh / 2 - 9}" font-size="11" text-anchor="middle" fill="{t["muted"]}">{CAPS[-1]}</text>')
    a(f'<text x="{xe + 8}" y="{y + bh + 22}" font-size="12" fill="{t["ink"]}">PostOutput</text>')
    a(f'<text x="{xe + 8}" y="{y + bh + 38}" font-size="11" fill="{t["muted"]}">output_rx</text>')
    a(f'<text x="200" y="{y - 22}" font-size="11" fill="{t["muted"]}">numbers are channel capacities (spawn_pipeline defaults)</text>')

    # inference detail
    ix = xs[2]
    dy = y + bh + 26
    a(f'<rect x="{ix - 80}" y="{dy}" width="{bw + 160}" height="150" rx="10" fill="{t["band"]}"/>')
    a(f'<line x1="{ix + bw / 2}" y1="{y + bh}" x2="{ix + bw / 2}" y2="{dy}" stroke="{t["accent"]}" stroke-width="1.6" stroke-dasharray="3 3"/>')
    rows = [
        ("deadline check", t["ink"]),
        ("CircuitBreaker: opens after 5 failures,", t["ink"]),
        ("  probes again after 60 s", t["muted"]),
        ("timeout, then your ModelWorker", t["ink"]),
    ]
    for j, (s, c) in enumerate(rows):
        a(f'<text x="{ix - 64}" y="{dy + 26 + j * 18}" font-size="11.5" fill="{c}">{s}</text>')
    a(f'<text x="{ix - 64}" y="{dy + 110}" font-size="10.5" fill="{t["gold"]}">wrap the worker with building blocks:</text>')
    a(f'<text x="{ix - 64}" y="{dy + 127}" font-size="10.5" fill="{t["muted"]}">Deduplicator, RetryPolicy, CacheLayer,</text>')
    a(f'<text x="{ix - 64}" y="{dy + 142}" font-size="10.5" fill="{t["muted"]}">RateLimiterRegistry, LoadBalancedWorker</text>')

    # workers
    wx = xs[4] + 10
    a(f'<line x1="{ix + bw + 80}" y1="{dy + 76}" x2="{wx - 12}" y2="{dy + 76}" stroke="{t["accent"]}" stroke-width="1.4" stroke-dasharray="3 3" marker-end="url(#ar)"/>')
    a(f'<text x="{wx}" y="{dy + 26}" font-size="12" font-weight="700" fill="{t["ink"]}">ModelWorker backends</text>')
    for j, s in enumerate(["AnthropicWorker", "OpenAiWorker", "LlamaCppWorker", "VllmWorker", "EchoWorker (no key)", "or impl ModelWorker"]):
        a(f'<text x="{wx}" y="{dy + 46 + j * 17}" font-size="11.5" fill="{t["muted"] if j == 5 else t["ink"]}">{s}</text>')

    # DLQ
    qy = 470
    a(f'<rect x="200" y="{qy}" width="{xs[-1] + bw - 200}" height="52" rx="10" fill="none" stroke="{t["bad"]}" stroke-width="1.6"/>')
    a(f'<text x="218" y="{qy + 22}" font-size="13" font-weight="700" fill="{t["bad"]}">DeadLetterQueue</text>')
    a(f'<text x="218" y="{qy + 40}" font-size="11" fill="{t["muted"]}">ring buffer of 1000; handles.dlq.drain() to inspect or replay</text>')
    for i in (0, 1, 3):
        x = xs[i] + bw / 2
        a(f'<line x1="{x}" y1="{y + bh + 2}" x2="{x}" y2="{qy - 4}" stroke="{t["bad"]}" stroke-width="1.3" stroke-dasharray="4 3" marker-end="url(#arb)"/>')
        a(f'<text x="{x + 6}" y="{qy - 12}" font-size="10" fill="{t["bad"]}">{DROPS[i]}</text>')
    x = xs[2] + bw / 2
    a(f'<line x1="{x}" y1="{dy + 150}" x2="{x}" y2="{qy - 4}" stroke="{t["bad"]}" stroke-width="1.3" stroke-dasharray="4 3" marker-end="url(#arb)"/>')
    a(f'<text x="{x + 6}" y="{qy - 40}" font-size="10" fill="{t["bad"]}">breaker open, failure,</text>')
    a(f'<text x="{x + 6}" y="{qy - 26}" font-size="10" fill="{t["bad"]}">timeout, deadline,</text>')
    a(f'<text x="{x + 6}" y="{qy - 12}" font-size="10" fill="{t["bad"]}">backpressure</text>')
    a('</svg>')
    return "\n".join(o)


if __name__ == "__main__":
    for name, t in THEMES.items():
        with open(f"assets/architecture-{name}.svg", "w", encoding="utf-8") as f:
            f.write(svg(t))
    print("wrote assets/architecture-{light,dark}.svg")
