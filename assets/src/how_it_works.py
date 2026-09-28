"""Writes assets/how-it-works.svg, the animated "how it works" diagram.

Everything in it is read off the code, not invented:
  - five stages and channel capacities: src/stages.rs, spawn_pipeline()
    (input 512, rag->assemble 512, assemble->inference 512,
     inference->post 1024, post->stream 512, output 256)
  - stage 3 order: deadline check, CircuitBreaker::call, tokio::time::timeout
    (default 120 s), then ModelWorker::infer
  - breaker: CircuitBreaker::new(5, 0.8, 60 s) in spawn_pipeline
  - DLQ: DeadLetterQueue::new(1000) and the reason strings pushed in stages.rs
  - web API: a full input queue answers 429 with Retry-After: 5 (web_api.rs)
  - Deduplicator, RetryPolicy and RateLimiter are NOT built into
    spawn_pipeline: you wrap your ModelWorker in them (examples/llm_pipeline.rs
    does this with Deduplicator). The drawing says so.

One 12 s loop: 8 s of healthy traffic, then a provider outage during which the
open breaker refuses calls and each refusal lands in the DLQ.
Run from the repo root:  python assets/src/how_it_works.py
"""

W, H = 1000, 660
CYCLE = 12.0
OPEN_AT = 8.0

BG, PANEL, INK, MUTED, LINE = "#0f1419", "#161c24", "#e8ecf1", "#9aa5b1", "#3a4452"
OK, BAD, GOLD, BLUE = "#34d399", "#f87171", "#fbbf24", "#60a5fa"
FONT = "-apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, 'Cascadia Mono', 'SF Mono', Menlo, Consolas, monospace"

o = []
a = o.append


def text(x, y, s, size=16, fill=INK, anchor="start", weight=400, mono=False, extra=""):
    fam = MONO if mono else FONT
    a(f'<text x="{x}" y="{y}" font-size="{size}" fill="{fill}" text-anchor="{anchor}" '
      f'font-weight="{weight}" font-family="{fam}" {extra}>{s}</text>')


def kt(*pairs):
    """keyTimes/values for a 12 s cycle from (seconds, value) pairs."""
    ts = ";".join(f"{t / CYCLE:.4f}" for t, _ in pairs)
    vs = ";".join(str(v) for _, v in pairs)
    return ts, vs


a(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" '
  f'aria-label="Animated diagram: requests pass through five bounded stages; stage 3 wraps the model call in a deadline check, '
  f'a circuit breaker and a timeout; dropped requests go to a dead-letter queue.">')
a(f'<rect width="{W}" height="{H}" rx="16" fill="{BG}"/>')
a('<defs>'
  f'<marker id="ar" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,1 L9,5 L0,9 z" fill="{MUTED}"/></marker>'
  f'<marker id="arr" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,1 L9,5 L0,9 z" fill="{BAD}"/></marker>'
  '</defs>')

text(40, 46, "How a request moves through tokio-prompt-orchestrator", 24, weight=700)
text(40, 74, "Five Tokio tasks joined by bounded channels. The numbers are the default channel sizes.", 16, MUTED)

# ── stage row ───────────────────────────────────────────────────────────────
Y, BH, BW = 150, 76, 150
XS = [40, 232, 424, 616, 808]
STAGES = [("1  Retrieve", "adds context"), ("2  Assemble", "builds the prompt"),
          ("3  Inference", "calls the model"), ("4  Post-process", "joins the tokens"),
          ("5  Stream", "hands it back")]
GAPS = ["512", "512", "1024", "512"]
MID = Y + BH / 2

# in / out labels above the row
text(40, 118, "IN  input_tx.send() or POST /api/v1/infer   (queue 512)", 15, GOLD, mono=True)
text(958, 118, "OUT  answer (queue 256)", 15, OK, "end", mono=True)
a(f'<path d="M 60 124 L 60 {Y - 4}" stroke="{GOLD}" stroke-width="2" marker-end="url(#ar)"/>')
a(f'<path d="M 938 {Y - 4} L 938 124" stroke="{OK}" stroke-width="2" marker-end="url(#ar)"/>')

for i, (name, sub) in enumerate(STAGES):
    x = XS[i]
    hot = i == 2
    a(f'<rect x="{x}" y="{Y}" width="{BW}" height="{BH}" rx="10" fill="{PANEL}" '
      f'stroke="{BLUE if hot else LINE}" stroke-width="{2.4 if hot else 1.5}"/>')
    text(x + BW / 2, Y + 32, name, 18, weight=700, anchor="middle")
    text(x + BW / 2, Y + 56, sub, 15, MUTED, anchor="middle")
    if i < 4:
        x0, x1 = x + BW + 4, XS[i + 1] - 4
        a(f'<line x1="{x0}" y1="{MID}" x2="{x1}" y2="{MID}" stroke="{MUTED}" stroke-width="2" marker-end="url(#ar)"/>')
        text((x0 + x1) / 2, MID - 10, GAPS[i], 14, GOLD, "middle", mono=True)

# ── stage 3 zoom panel ──────────────────────────────────────────────────────
PY, PH = 268, 168
a(f'<path d="M {XS[2]} {Y + BH} L 40 {PY} M {XS[2] + BW} {Y + BH} L 960 {PY}" stroke="{BLUE}" stroke-width="1.2" stroke-dasharray="4 5" fill="none" opacity=".7"/>')
a(f'<rect x="40" y="{PY}" width="920" height="{PH}" rx="12" fill="{PANEL}" stroke="{BLUE}" stroke-width="1.6"/>')
text(60, PY + 28, "Inside stage 3, for every request", 16, BLUE, weight=700)

SY, SH = PY + 44, 104
steps = [
    (60, 176, "Deadline", ["passed already?", "then drop it"]),
    (262, 222, "Circuit breaker", ["5 failures: OPEN,", "refuse for 60 s, then", "let 1 probe through"]),
    (510, 150, "Timeout", ["120 s per call", "(configurable)"]),
]
for x, w, title, lines in steps:
    a(f'<rect x="{x}" y="{SY}" width="{w}" height="{SH}" rx="9" fill="{BG}" stroke="{LINE}" stroke-width="1.4"/>')
    text(x + 14, SY + 26, title, 17, weight=700)
    for j, ln in enumerate(lines):
        text(x + 14, SY + 50 + j * 20, ln, 15, MUTED)
# breaker state badge, animated
bx, by = 262 + 222 - 86, SY + 12
ts, vs = kt((0, 1), (OPEN_AT - 0.01, 1), (OPEN_AT, 0), (CYCLE, 0))
a(f'<g><rect x="{bx}" y="{by}" width="74" height="22" rx="11" fill="{OK}" opacity=".18"/>'
  f'<text x="{bx + 37}" y="{by + 16}" font-size="13" font-weight="700" fill="{OK}" text-anchor="middle" font-family="{MONO}">CLOSED</text>'
  f'<animate attributeName="opacity" dur="{CYCLE}s" repeatCount="indefinite" calcMode="discrete" keyTimes="{ts}" values="{vs}"/></g>')
ts, vs = kt((0, 0), (OPEN_AT - 0.01, 0), (OPEN_AT, 1), (CYCLE, 1))
a(f'<g opacity="0"><rect x="{bx}" y="{by}" width="74" height="22" rx="11" fill="{BAD}" opacity=".2"/>'
  f'<text x="{bx + 37}" y="{by + 16}" font-size="13" font-weight="700" fill="{BAD}" text-anchor="middle" font-family="{MONO}">OPEN</text>'
  f'<animate attributeName="opacity" dur="{CYCLE}s" repeatCount="indefinite" calcMode="discrete" keyTimes="{ts}" values="{vs}"/></g>')

# worker box with the optional wrappers
WX, WW = 688, 256
a(f'<rect x="{WX}" y="{SY}" width="{WW}" height="{SH}" rx="9" fill="{BG}" stroke="{OK}" stroke-width="1.6"/>')
text(WX + 14, SY + 26, "Your ModelWorker", 17, weight=700)
text(WX + 14, SY + 50, "Anthropic, OpenAI, llama.cpp,", 15, MUTED)
text(WX + 14, SY + 70, "vLLM, echo, or your own", 15, MUTED)
text(WX + 14, SY + 92, "+ optional wrappers, see below", 14, GOLD)
for x0, x1 in [(236, 262), (484, 510), (660, 688)]:
    a(f'<line x1="{x0 + 2}" y1="{SY + SH / 2}" x2="{x1 - 3}" y2="{SY + SH / 2}" stroke="{MUTED}" stroke-width="2" marker-end="url(#ar)"/>')

# ── optional wrappers strip ─────────────────────────────────────────────────
OY = PY + PH + 16
text(40, OY + 20, "Optional, you wrap your worker in them:", 15, GOLD, weight=700)
for x, name, why in [(380, "Deduplicator", "same prompt, one call"), (580, "RetryPolicy", "backoff + jitter"), (780, "RateLimiter", "caps the request rate")]:
    text(x, OY + 12, name, 15, INK, weight=700)
    text(x, OY + 30, why, 13, MUTED)

# ── dead-letter queue ───────────────────────────────────────────────────────
DY, DH = 500, 128
a(f'<rect x="40" y="{DY}" width="920" height="{DH}" rx="12" fill="#1f1416" stroke="{BAD}" stroke-width="1.6"/>')
text(60, DY + 30, "Dead-letter queue", 18, BAD, weight=700)
text(232, DY + 30, "a 1000-entry ring buffer: nothing is dropped silently, every entry has a reason", 15, INK)
reasons = [("backpressure:&lt;stage&gt;", "next channel was full"), ("deadline_expired", "too late to run"),
           ("circuit_breaker_open", "failed fast"), ("inference_timeout:120s", "model too slow"),
           ("inference_failure:&lt;error&gt;", "provider error")]
for j, (r, why) in enumerate(reasons):
    col, row = j % 3, j // 3
    x, y = 60 + col * 300, DY + 64 + row * 40
    text(x, y, r, 15, BAD, mono=True)
    text(x, y + 18, why, 13, MUTED)
text(660, DY + 104, "read it: handles.dlq.drain()", 13, INK, mono=True)
text(660, DY + 122, "or GET /api/v1/debug/dlq", 13, INK, mono=True)

# ── moving requests ─────────────────────────────────────────────────────────
MAIN = f"M 60 124 L 60 {MID} L 938 {MID} L 938 124"


def dot(path, begin, dur, color, r=7, fade_in=0.0):
    """A dot that follows `path` once per cycle, starting at `begin` seconds."""
    t0, t1 = begin / CYCLE, (begin + dur) / CYCLE
    kp = "0;0;1;1"
    ktimes = f"0;{t0:.4f};{t1:.4f};1"
    op_t = f"0;{max(t0 - 0.0001, 0):.4f};{t0:.4f};{t1:.4f};{min(t1 + 0.0001, 1):.4f};1"
    a(f'<circle r="{r}" fill="{color}" opacity="0">'
      f'<animateMotion dur="{CYCLE}s" repeatCount="indefinite" path="{path}" keyPoints="{kp}" keyTimes="{ktimes}" calcMode="linear"/>'
      f'<animate attributeName="opacity" dur="{CYCLE}s" repeatCount="indefinite" keyTimes="{op_t}" values="0;0;1;1;0;0" calcMode="discrete"/>'
      '</circle>')


# healthy phase: requests go all the way through, and each one's model call
# runs through the stage-3 steps
for k in range(6):
    dot(MAIN, 0.2 + k * 1.1, 3.0, OK)
    dot(f"M 118 {SY + SH / 2} L 816 {SY + SH / 2}", 0.2 + k * 1.1 + 1.1, 1.2, OK, r=6)
# outage: requests reach stage 3, the open breaker refuses them, they drop to the DLQ
for k in range(3):
    b = OPEN_AT + 0.1 + k * 1.1
    dot(f"M 60 124 L 60 {MID} L 499 {MID}", b, 1.3, GOLD)
    dot(f"M 118 {SY + SH / 2} L 373 {SY + SH / 2} L 373 {DY + 44}", b + 1.3, 0.9, BAD, r=6)

# phase caption
ts, vs = kt((0, 1), (OPEN_AT - 0.01, 1), (OPEN_AT, 0), (CYCLE, 0))
a(f'<g><text x="40" y="{H - 10}" font-size="14" fill="{OK}" font-family="{FONT}">Provider healthy: requests flow through all five stages.</text>'
  f'<animate attributeName="opacity" dur="{CYCLE}s" repeatCount="indefinite" calcMode="discrete" keyTimes="{ts}" values="{vs}"/></g>')
ts, vs = kt((0, 0), (OPEN_AT - 0.01, 0), (OPEN_AT, 1), (CYCLE, 1))
a(f'<g opacity="0"><text x="40" y="{H - 10}" font-size="14" fill="{BAD}" font-family="{FONT}">Provider down: after 5 failures the breaker is OPEN, calls fail fast into the dead-letter queue.</text>'
  f'<animate attributeName="opacity" dur="{CYCLE}s" repeatCount="indefinite" calcMode="discrete" keyTimes="{ts}" values="{vs}"/></g>')

a("</svg>")
open("assets/how-it-works.svg", "w", encoding="utf-8").write("\n".join(o) + "\n")
print("wrote assets/how-it-works.svg")
