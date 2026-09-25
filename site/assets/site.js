(function () {
  "use strict";
  var root = document.documentElement;

  // theme: system by default, a button flips and remembers it
  function effective() {
    var t = root.getAttribute("data-theme");
    if (t) return t;
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  function syncImages() {
    var img = document.getElementById("arch-img");
    if (!img) return;
    var pic = img.parentNode;
    var src = pic.querySelector("source");
    if (root.getAttribute("data-theme") && src) src.remove();
    if (root.getAttribute("data-theme")) img.src = "assets/architecture-" + effective() + ".svg";
  }
  var btn = document.getElementById("theme");
  if (btn) btn.addEventListener("click", function () {
    var next = effective() === "dark" ? "light" : "dark";
    root.setAttribute("data-theme", next);
    try { localStorage.setItem("tpo-theme", next); } catch (e) {}
    syncImages();
  });
  syncImages();

  // copy buttons
  document.querySelectorAll("button.copy").forEach(function (b) {
    b.addEventListener("click", function () {
      var text = b.getAttribute("data-copy");
      if (!text) {
        var t = document.getElementById(b.getAttribute("data-target"));
        text = t ? t.textContent : "";
      }
      var done = function () { b.textContent = "Copied"; setTimeout(function () { b.textContent = "Copy"; }, 1400); };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, function () { b.textContent = "Select and copy"; });
      } else { b.textContent = "Select and copy"; }
    });
  });

  // the real stdout of the example
  fetch("data/llm_pipeline_output.txt").then(function (r) { return r.text(); }).then(function (t) {
    var pre = document.getElementById("llm-out");
    if (!pre) return;
    var esc = function (s) { return s.replace(/&/g, "&amp;").replace(/</g, "&lt;"); };
    pre.innerHTML = '<span class="p">$</span> <span class="c">cargo run --example llm_pipeline</span>\n' +
      esc(t.replace(/\s+$/, ""))
        .replace(/^( {3}DLQ .*)$/gm, '<span class="e">$1</span>')
        .replace(/^( {3}-&gt; .*|   -> .*)$/gm, '<span class="c">$1</span>');
  }).catch(function () {});

  // the replay
  fetch("data/llm_pipeline_trace.json").then(function (r) { return r.json(); }).then(function (d) {
    PipelineReplay.start(document.getElementById("flow"), d, {
      clock: document.getElementById("r-clock"),
      calls: document.getElementById("r-calls"),
      answers: document.getElementById("r-ans"),
      dlq: document.getElementById("r-dlq"),
      breaker: document.getElementById("r-br"),
      button: document.getElementById("r-play")
    });
  }).catch(function () {});
})();
