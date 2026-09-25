// Draws the request flow of one real run of `cargo run --example llm_pipeline`.
// Every number and timestamp comes from data/llm_pipeline_trace.json.
// Used by the project site (animated) and by the banner/social renderers.
(function (root) {
  "use strict";

  var W = 1200;
  var X = { user: 118, stageA: 236, stageB: 384, gate: 530, dedup: 680, model: 870, out: 1090 };
  var LANE = [154, 208, 262];          // centre y of each question's bundle
  var USER_Y = [132, 186, 240, 294];    // alice, bob, carol, dave
  var ERIN_Y = 404;
  var DLQ_Y = 520;
  var TL_Y = 612;                      // timeline strip

  function fmt(n) { return (Math.round(n * 10) / 10).toString(); }
  function bundleY(q, u) { return LANE[q] - 9 + u * 6; }
  function outY(q, u) { return LANE[q] - 9 + u * 6; }
  function outageY(k) { return 372 + k * 7; }
  function dlqX(k) { return k < 5 ? 740 + k * 34 : 600 + (k - 5) * 34; }

  // One path per request, keyed by request id.
  function paths() {
    var p = {};
    for (var n = 1; n <= 12; n++) {
      var u = Math.floor((n - 1) / 3), q = (n - 1) % 3;
      var y0 = USER_Y[u], yb = bundleY(q, u), yo = outY(q, u);
      var id = "req-" + (n < 10 ? "0" : "") + n;
      var d = "M" + X.user + "," + y0 +
        " C" + (X.user + 80) + "," + y0 + " " + (X.stageA - 60) + "," + yb + " " + X.stageA + "," + yb +
        " L" + X.dedup + "," + yb;
      if (u === 0) {
        // first time this question is asked: goes all the way to the model
        d += " C" + (X.dedup + 60) + "," + yb + " " + (X.model - 90) + "," + LANE[q] + " " + (X.model - 30) + "," + LANE[q] +
          " L" + (X.model + 30) + "," + LANE[q] +
          " C" + (X.model + 90) + "," + LANE[q] + " " + (X.out - 90) + "," + yo + " " + X.out + "," + yo;
      } else {
        // repeat: answered by the dedup cache, never reaches the model
        var arc = 64 + q * 0;
        d += " C" + (X.dedup + 50) + "," + yb + " " + (X.dedup + 40) + "," + (104 - u * 5) + " " + (X.dedup + 120) + "," + (104 - u * 5) +
          " L" + (X.model + 60) + "," + (104 - u * 5) +
          " C" + (X.model + 150) + "," + (104 - u * 5) + " " + (X.out - 90) + "," + yo + " " + X.out + "," + yo;
      }
      p[id] = { d: d, q: q, u: u, kind: u === 0 ? "model" : "cached" };
    }
    for (var k = 0; k < 8; k++) {
      var id2 = "req-" + (13 + k);
      var yo2 = outageY(k);
      var d2 = "M" + X.user + "," + ERIN_Y +
        " C" + (X.user + 80) + "," + ERIN_Y + " " + (X.stageA - 60) + "," + yo2 + " " + X.stageA + "," + yo2;
      if (k < 5) {
        d2 += " L" + (X.model - 30) + "," + yo2 +
          " C" + (X.model - 30 + (dlqX(k) - X.model + 30) * 0.2) + "," + yo2 + " " + dlqX(k) + "," + (yo2 + 30) + " " + dlqX(k) + "," + (DLQ_Y - 14);
      } else {
        d2 += " L" + (X.gate - 6) + "," + yo2 +
          " C" + (X.gate + 40) + "," + yo2 + " " + dlqX(k) + "," + (yo2 + 40) + " " + dlqX(k) + "," + (DLQ_Y - 14);
      }
      p[id2] = { d: d2, k: k, kind: k < 5 ? "failed" : "shed" };
    }
    return p;
  }

  function esc(s) { return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;"); }

  // Build the SVG markup. Colours come from CSS custom properties so the
  // same drawing works in light and dark.
  function svg(data, opt) {
    opt = opt || {};
    var ev = data.events;
    var modelCalls = [], starts = {}, fails = [], dlq = [], cached = [], outputs = [];
    ev.forEach(function (e) {
      if (e.kind === "model_start") starts[e.q] = e.t;
      if (e.kind === "model_done") modelCalls.push({ q: e.q, a: starts[e.q], b: e.t });
      if (e.kind === "provider_503") fails.push(e);
      if (e.kind === "dlq") dlq.push(e);
      if (e.kind === "dedup_cached") cached.push(e);
      if (e.kind === "output") outputs.push(e);
    });
    var tEnd = dlq.length ? dlq[dlq.length - 1].t : 0;
    var lastOut = outputs.length ? outputs[outputs.length - 1].t : 0;
    var callMs = modelCalls.map(function (c) { return c.b - c.a; });
    var avgCall = callMs.reduce(function (a, b) { return a + b; }, 0) / (callMs.length || 1);
    var nShed = dlq.filter(function (d) { return d.reason === "circuit_breaker_open"; }).length;
    var nFail = dlq.length - nShed;
    var P = paths();
    var H = opt.noTimeline ? 552 : 672;
    var o = [];
    o.push('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ' + W + ' ' + H + '" class="flow" role="img" aria-labelledby="flow-title flow-desc">');
    o.push('<title id="flow-title">One real run of the llm_pipeline example</title>');
    o.push('<desc id="flow-desc">12 requests from 4 users ask 3 questions. The model is called 3 times; the 9 repeats are answered by the dedup cache. Then 8 requests arrive during a simulated outage: 5 fail with 503 and trip the circuit breaker, the next 3 are refused without calling the provider, and all 8 land in the dead-letter queue.</desc>');

    // column headers
    var heads = [
      [X.user, "sessions", "4 users + 1"],
      [(X.stageA + X.stageB) / 2, "retrieve · assemble", "stages 1 and 2"],
      [X.gate, "circuit breaker", "opens after 5 failures"],
      [X.dedup, "dedup", "12 in → 3 calls"],
      [X.model, "model", callMs.length + " calls · ~" + Math.round(avgCall) + " ms"],
      [X.out, "answers", outputs.length + " in " + Math.round(lastOut) + " ms"]
    ];
    heads.forEach(function (h, i) {
      var anchor = i === 0 ? "middle" : (i === heads.length - 1 ? "middle" : "middle");
      o.push('<text x="' + h[0] + '" y="22" class="h" text-anchor="' + anchor + '">' + esc(h[1]) + '</text>');
      o.push('<text x="' + h[0] + '" y="38" class="hs" text-anchor="' + anchor + '">' + esc(h[2]) + '</text>');
    });

    // stage band and gates
    o.push('<rect x="' + X.stageA + '" y="116" width="' + (X.stageB - X.stageA) + '" height="' + (outageY(7) - 100) + '" rx="6" class="band"/>');
    o.push('<line x1="' + X.gate + '" y1="116" x2="' + X.gate + '" y2="' + (outageY(7) + 16) + '" class="gate"/>');
    o.push('<rect x="' + (X.gate - 7) + '" y="' + (outageY(5) - 8) + '" width="14" height="' + (outageY(7) - outageY(5) + 16) + '" rx="3" class="gate-open" id="gate-open"/>');
    o.push('<rect x="' + (X.dedup - 7) + '" y="' + (LANE[0] - 22) + '" width="14" height="' + (LANE[2] - LANE[0] + 44) + '" rx="7" class="node"/>');
    o.push('<rect x="' + (X.model - 30) + '" y="' + (LANE[0] - 22) + '" width="60" height="' + (LANE[2] - LANE[0] + 44) + '" rx="8" class="model"/>');
    o.push('<rect x="' + (X.model - 30) + '" y="' + (outageY(0) - 10) + '" width="60" height="' + (outageY(4) - outageY(0) + 20) + '" rx="6" class="model-down" id="model-down"/>');
    o.push('<text x="' + (X.model + 40) + '" y="' + (outageY(2) + 4) + '" class="bad" id="down-label">503 × ' + fails.length + '</text>');
    o.push('<text x="' + X.gate + '" y="' + (outageY(7) + 36) + '" class="bad" text-anchor="middle" id="open-label">open: ' + nShed + ' refused</text>');
    o.push('<text x="' + (X.dedup + 128) + '" y="' + 86 + '" class="note">' + cached.length + ' repeats answered from cache</text>');

    // requests
    Object.keys(P).forEach(function (id) {
      var r = P[id];
      var cls = r.kind === "model" ? "rq q" + r.q + " main" : r.kind === "cached" ? "rq q" + r.q + " cached" : r.kind === "failed" ? "rq failed" : "rq shed";
      o.push('<path id="p-' + id + '" d="' + r.d + '" class="' + cls + '"/>');
    });

    // model call markers
    [0, 1, 2].forEach(function (q) {
      o.push('<circle cx="' + X.model + '" cy="' + LANE[q] + '" r="6" class="dot q' + q + '" id="model-q' + q + '"/>');
    });

    // session nodes
    ["alice", "bob", "carol", "dave"].forEach(function (u, i) {
      o.push('<circle cx="' + X.user + '" cy="' + USER_Y[i] + '" r="5" class="unode"/>');
      o.push('<text x="' + (X.user - 14) + '" y="' + (USER_Y[i] + 4) + '" class="lbl" text-anchor="end">' + u + '</text>');
    });
    o.push('<circle cx="' + X.user + '" cy="' + ERIN_Y + '" r="5" class="unode bad-node"/>');
    o.push('<text x="' + (X.user - 14) + '" y="' + (ERIN_Y + 4) + '" class="lbl" text-anchor="end">erin</text>');
    o.push('<text x="' + (X.user - 14) + '" y="' + (ERIN_Y + 20) + '" class="hs" text-anchor="end">during outage</text>');

    // answer dots
    for (var n = 1; n <= 12; n++) {
      var u = Math.floor((n - 1) / 3), q = (n - 1) % 3;
      o.push('<circle cx="' + X.out + '" cy="' + outY(q, u) + '" r="2.6" class="dot q' + q + '" id="ans-req-' + (n < 10 ? "0" : "") + n + '"/>');
    }
    ["Paris", "backpressure", "haiku"].forEach(function (t, q) {
      o.push('<text x="' + (X.out + 12) + '" y="' + (LANE[q] + 4) + '" class="lbl">' + t + '</text>');
    });

    // dead-letter queue
    o.push('<rect x="' + (dlqX(5) - 22) + '" y="' + (DLQ_Y - 14) + '" width="' + (dlqX(4) - dlqX(5) + 44) + '" height="30" rx="6" class="dlq"/>');
    dlq.forEach(function (d, i) {
      var shed = d.reason === "circuit_breaker_open";
      var k = parseInt(d.req.slice(4), 10) - 13;
      o.push('<rect x="' + (dlqX(k) - 11) + '" y="' + (DLQ_Y - 8) + '" width="22" height="18" rx="3" class="slot ' + (shed ? "slot-shed" : "slot-fail") + '" id="slot-' + d.req + '"/>');
    });
    o.push('<text x="' + (dlqX(5) - 34) + '" y="' + (DLQ_Y + 5) + '" class="h" text-anchor="end">dead-letter queue</text>');
    o.push('<text x="' + (dlqX(4) + 34) + '" y="' + (DLQ_Y + 5) + '" class="hs">' + nFail + ' × 503, ' + nShed + ' × breaker open</text>');

    // timeline strip with the real timestamps
    if (!opt.noTimeline) {
      var x0 = 118, x1 = 1150, span = Math.ceil(tEnd / 100) * 100;
      var sx = function (t) { return x0 + (t / span) * (x1 - x0); };
      o.push('<g class="tl">');
      o.push('<text x="' + (x0 - 14) + '" y="' + (TL_Y + 4) + '" class="h" text-anchor="end">time</text>');
      o.push('<line x1="' + x0 + '" y1="' + (TL_Y + 22) + '" x2="' + x1 + '" y2="' + (TL_Y + 22) + '" class="axis"/>');
      for (var t = 0; t <= span; t += 100) {
        o.push('<line x1="' + sx(t) + '" y1="' + (TL_Y + 22) + '" x2="' + sx(t) + '" y2="' + (TL_Y + 27) + '" class="axis"/>');
        o.push('<text x="' + sx(t) + '" y="' + (TL_Y + 42) + '" class="tick" text-anchor="middle">' + t + (t === span ? " ms" : "") + '</text>');
      }
      modelCalls.forEach(function (c) {
        o.push('<rect x="' + sx(c.a) + '" y="' + (TL_Y - 8) + '" width="' + (sx(c.b) - sx(c.a) - 2) + '" height="16" rx="3" class="bar q' + c.q + '"/>');
        o.push('<text x="' + ((sx(c.a) + sx(c.b)) / 2) + '" y="' + (TL_Y + 4) + '" class="barl" text-anchor="middle">model call ' + fmt(c.b - c.a) + ' ms</text>');
      });
      cached.forEach(function (c) {
        o.push('<line x1="' + sx(c.t) + '" y1="' + (TL_Y - 16) + '" x2="' + sx(c.t) + '" y2="' + (TL_Y + 14) + '" class="tk q' + c.q + '"/>');
      });
      dlq.forEach(function (d) {
        var shed = d.reason === "circuit_breaker_open";
        o.push('<line x1="' + sx(d.t) + '" y1="' + (TL_Y - 8) + '" x2="' + sx(d.t) + '" y2="' + (TL_Y + 8) + '" class="tk ' + (shed ? "shed" : "fail") + '"/>');
      });
      o.push('<text x="' + (sx(cached[0].t) - 6) + '" y="' + (TL_Y - 22) + '" class="hs" text-anchor="end">9 cache hits in ' + fmt(cached[cached.length - 1].t - cached[0].t) + ' ms</text>');
      o.push('<text x="' + (sx(dlq[0].t) + 4) + '" y="' + (TL_Y - 22) + '" class="hs bad">8 to DLQ</text>');
      o.push('<line id="playhead" x1="' + x0 + '" y1="' + (TL_Y - 30) + '" x2="' + x0 + '" y2="' + (TL_Y + 22) + '" class="playhead" style="opacity:0"/>');
      o.push('</g>');
    }
    o.push('</svg>');
    return o.join("");
  }

  root.PipelineFlow = { svg: svg, paths: paths, X: X, TL: { x0: 118, x1: 1150 } };
})(typeof window !== "undefined" ? window : this);
