// Replays the recorded run on the flow drawing. Each dot is one request.
// A dot moves at a fixed on-screen speed but is held at each stage until the
// real recorded timestamp for that step, so the clock readout is the run's
// own clock (slowed down), not an invented one.
(function () {
  "use strict";

  function el(id) { return document.getElementById(id); }

  function start(host, data, ui) {
    host.innerHTML = PipelineFlow.svg(data);
    var svg = host.querySelector("svg");
    var NS = "http://www.w3.org/2000/svg";
    var ev = data.events;
    var X = PipelineFlow.X;
    var reduce = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

    var modelStart = {}, modelDone = {}, out = {}, dlq = {}, fail503 = {}, sendT = {};
    var cachedQueue = [];
    ev.forEach(function (e) {
      if (e.kind === "send") sendT[e.req] = e.t;
      if (e.kind === "model_start") modelStart[e.q] = e.t;
      if (e.kind === "model_done") modelDone[e.q] = e.t;
      if (e.kind === "output") out[e.req] = e.t;
      if (e.kind === "dlq") dlq[e.req] = e;
      if (e.kind === "provider_503") fail503[e.req] = e.t;
      if (e.kind === "dedup_cached") cachedQueue.push(e.t);
    });
    var tEnd = Math.max.apply(null, ev.map(function (e) { return e.t; }));
    var failTimes = Object.keys(dlq).filter(function (k) { return dlq[k].reason !== "circuit_breaker_open"; })
      .map(function (k) { return dlq[k].t; }).sort(function (a, b) { return a - b; });
    var breakerOpensAt = failTimes[failTimes.length - 1];

    // length along a path where x first reaches a value
    function lenAtX(path, x) {
      var L = path.getTotalLength(), lo = 0, hi = L;
      for (var i = 0; i < 30; i++) {
        var mid = (lo + hi) / 2;
        if (path.getPointAtLength(mid).x < x) lo = mid; else hi = mid;
      }
      return hi;
    }

    var parts = [];
    var cachedIdx = 0;
    for (var n = 1; n <= 20; n++) {
      var id = "req-" + (n < 10 ? "0" : "") + n;
      var path = el("p-" + id);
      if (!path) continue;
      var L = path.getTotalLength();
      var wps = [];
      var cls;
      if (n <= 12) {
        var u = Math.floor((n - 1) / 3), q = (n - 1) % 3;
        cls = "pkt q" + q;
        if (u === 0) {
          wps.push([lenAtX(path, X.dedup), modelStart[q]]);
          wps.push([lenAtX(path, X.model), modelStart[q]]);
          wps.push([lenAtX(path, X.model + 1), modelDone[q]]);
          wps.push([L, out[id]]);
        } else {
          wps.push([lenAtX(path, X.dedup), cachedQueue[cachedIdx++]]);
          wps.push([L, out[id]]);
        }
      } else {
        cls = "pkt bad";
        var k = n - 13;
        if (k < 5) {
          wps.push([lenAtX(path, X.gate), 0]);
          wps.push([lenAtX(path, X.model - 31), fail503[id]]);
          wps.push([L, dlq[id].t]);
        } else {
          wps.push([lenAtX(path, X.gate - 7), dlq[id].t]);
          wps.push([L, dlq[id].t]);
        }
      }
      var c = document.createElementNS(NS, "circle");
      c.setAttribute("r", n <= 12 ? 4 : 3.6);
      c.setAttribute("class", cls);
      svg.appendChild(c);
      parts.push({ id: id, path: path, L: L, wps: wps, pos: 0, t0: sendT[id], c: c, done: false });
    }

    function setState(clock) {
      var open = clock >= breakerOpensAt;
      el("gate-open").style.opacity = open ? 1 : 0;
      el("open-label").style.opacity = open ? 1 : 0;
      el("down-label").style.opacity = clock >= (fail503["req-13"] || 1e9) ? 1 : 0;
      var ph = el("playhead");
      if (ph) {
        var span = Math.ceil(tEnd / 100) * 100, T = PipelineFlow.TL;
        var x = T.x0 + (Math.min(clock, tEnd) / span) * (T.x1 - T.x0);
        ph.setAttribute("x1", x); ph.setAttribute("x2", x);
        ph.style.opacity = clock > 0 && clock <= tEnd ? 1 : 0;
      }
      el("model-down").style.opacity = clock >= (fail503["req-13"] || 1e9) ? 1 : 0;
      [0, 1, 2].forEach(function (q) {
        var busy = clock >= modelStart[q] && clock < modelDone[q];
        el("model-q" + q).setAttribute("r", busy ? 9 : 6);
      });
      Object.keys(out).forEach(function (r) { el("ans-" + r).style.opacity = clock >= out[r] ? 1 : 0.15; });
      Object.keys(dlq).forEach(function (r) { el("slot-" + r).style.opacity = clock >= dlq[r].t ? 1 : 0.15; });
      if (ui.clock) ui.clock.textContent = Math.min(clock, tEnd).toFixed(1).padStart(6, " ") + " ms";
      if (ui.breaker) {
        ui.breaker.textContent = open ? "open" : "closed";
        ui.breaker.className = open ? "st bad" : "st ok";
      }
      if (ui.calls) {
        var calls = [0, 1, 2].filter(function (q) { return clock >= modelStart[q]; }).length;
        var answered = Object.keys(out).filter(function (r) { return clock >= out[r]; }).length;
        var dl = Object.keys(dlq).filter(function (r) { return clock >= dlq[r].t; }).length;
        ui.calls.textContent = calls;
        ui.answers.textContent = answered;
        ui.dlq.textContent = dl;
      }
    }

    var SPEED = 700;      // svg units per second of screen time
    var RATE = 0.2;       // 1 s of screen time = 200 ms of the recorded run
    var clock = 0, last = null, running = false, raf = null;

    function place(p) {
      var pt = p.path.getPointAtLength(p.pos);
      p.c.setAttribute("cx", pt.x);
      p.c.setAttribute("cy", pt.y);
      p.c.style.opacity = p.done ? 0 : (clock >= p.t0 ? 1 : 0);
    }

    function step(dt) {
      clock += dt * 1000 * RATE;
      parts.forEach(function (p) {
        if (clock < p.t0 || p.done) { place(p); return; }
        var budget = SPEED * dt;
        while (budget > 0 && p.wps.length) {
          var w = p.wps[0];
          var gap = w[0] - p.pos;
          if (gap <= budget) {
            p.pos = w[0];
            budget -= gap;
            if (clock >= w[1]) { p.wps.shift(); } else { budget = 0; }
          } else { p.pos += budget; budget = 0; }
        }
        if (!p.wps.length) p.done = true;
        place(p);
      });
      setState(clock);
    }

    function frame(ts) {
      if (!running) return;
      if (last === null) last = ts;
      var dt = Math.min(0.05, (ts - last) / 1000);
      last = ts;
      step(dt);
      if (parts.every(function (p) { return p.done; }) && clock > tEnd) {
        running = false;
        if (ui.button) ui.button.textContent = "Replay";
        return;
      }
      raf = requestAnimationFrame(frame);
    }

    function reset() {
      clock = 0; last = null;
      cachedIdx = 0;
      parts = parts.map(function (p) {
        p.c.remove();
        return null;
      });
      host.innerHTML = "";
      start(host, data, ui);
    }

    function play() {
      running = true; last = null;
      if (ui.button) ui.button.textContent = "Playing…";
      raf = requestAnimationFrame(frame);
    }

    if (ui.button) {
      ui.button.onclick = function () {
        if (running) return;
        if (clock > 0) { reset(); return; }
        play();
      };
    }

    if (reduce) {
      // no motion: show the final state of the run
      clock = tEnd + 1;
      parts.forEach(function (p) { p.done = true; place(p); });
      setState(clock);
      if (ui.button) ui.button.textContent = "Replay";
      ui.button && (ui.button.onclick = function () { reduce = false; clock = 0; reset(); });
      return;
    }

    parts.forEach(place);
    setState(0);
    // start when scrolled into view
    if ("IntersectionObserver" in window) {
      var io = new IntersectionObserver(function (es) {
        if (es[0].isIntersecting && !running && clock === 0) { play(); io.disconnect(); }
      }, { threshold: 0.35 });
      io.observe(host);
    } else { play(); }
  }

  window.PipelineReplay = { start: start };
})();
