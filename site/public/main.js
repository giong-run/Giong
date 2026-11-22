// giong.run: small behaviours. Every page reads fine without any of them.
(function () {
  "use strict";
  var root = document.documentElement;

  // 1. Light or dark. The page follows the system until someone picks; the choice is
  //    remembered in this browser only.
  var themeBtn = document.querySelector(".theme");
  if (themeBtn) {
    themeBtn.hidden = false;
    themeBtn.addEventListener("click", function () {
      var set = root.getAttribute("data-theme");
      var dark = set ? set === "dark" : !window.matchMedia("(prefers-color-scheme: light)").matches;
      var next = dark ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try { localStorage.setItem("giong-theme", next); } catch (e) { /* private window */ }
    });
  }

  // 2. The phone menu closes when a link in it is followed or Escape is pressed.
  var menu = document.querySelector(".menu");
  if (menu) {
    menu.addEventListener("click", function (e) { if (e.target.closest("a")) menu.open = false; });
    document.addEventListener("keydown", function (e) { if (e.key === "Escape") menu.open = false; });
    document.addEventListener("click", function (e) { if (menu.open && !menu.contains(e.target)) menu.open = false; });
  }

  // 3. Copy buttons.
  document.querySelectorAll("[data-copy]").forEach(function (btn) {
    var label = btn.textContent;
    btn.addEventListener("click", function () {
      var el = document.getElementById(btn.getAttribute("data-copy"));
      if (!el) return;
      var text = el.textContent.replace(/\s+#[^\n]*/g, "").trim();
      function done() {
        btn.textContent = btn.getAttribute("data-done") || label;
        btn.classList.add("done");
        setTimeout(function () { btn.textContent = label; btn.classList.remove("done"); }, 1600);
      }
      function select() {
        var r = document.createRange();
        r.selectNodeContents(el);
        var s = window.getSelection();
        s.removeAllRanges();
        s.addRange(r);
      }
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, select);
      } else {
        select();
      }
    });
  });

  // 3b. Documentation: "Copy for agent", "Copy prompt" and each section's copy button
  //     copy the Markdown kept in a <script type="text/markdown">; every code block in
  //     a page made from a README gets a copy button of its own.
  function copyText(text, btn) {
    var label = btn.querySelector("span") || btn;
    var was = label.textContent;
    function done() {
      label.textContent = btn.getAttribute("data-done") || "copied";
      btn.classList.add("done");
      setTimeout(function () { label.textContent = was; btn.classList.remove("done"); }, 1600);
    }
    function fallback() {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      try { if (document.execCommand("copy")) done(); } catch (e) { /* nothing more to try */ }
      document.body.removeChild(area);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, fallback);
    } else {
      fallback();
    }
  }
  document.querySelectorAll("[data-md]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var src = document.getElementById(btn.getAttribute("data-md"));
      if (src) copyText(src.textContent.replace(/<\\\//g, "</"), btn);
    });
  });
  document.querySelectorAll(".prose pre.code").forEach(function (pre) {
    if (pre.parentNode.classList.contains("code-copy")) return;
    var wrap = document.createElement("div");
    wrap.className = "code-copy";
    pre.parentNode.insertBefore(wrap, pre);
    wrap.appendChild(pre);
    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy";
    btn.setAttribute("data-done", "copied");
    btn.innerHTML = "<span>copy</span>";
    btn.addEventListener("click", function () { copyText(pre.textContent.replace(/\n$/, ""), btn); });
    wrap.appendChild(btn);
  });

  // The version menu closes on a click outside it or Escape.
  var ver = document.querySelector(".ver-menu");
  if (ver) {
    document.addEventListener("click", function (e) { if (ver.open && !ver.contains(e.target)) ver.open = false; });
    document.addEventListener("keydown", function (e) { if (e.key === "Escape") ver.open = false; });
  }

  // The documentation menu starts folded on a phone.
  var side = document.querySelector(".side-menu");
  if (side && window.matchMedia("(max-width: 860px)").matches) side.open = false;

  // 4. Code tabs. Without JavaScript every sample is shown, one under another.
  document.querySelectorAll('[role="tablist"]').forEach(function (list) {
    var tabs = Array.prototype.slice.call(list.querySelectorAll('[role="tab"]'));
    function select(tab, focus) {
      tabs.forEach(function (t) {
        var on = t === tab;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        var panel = document.getElementById(t.getAttribute("aria-controls"));
        if (panel) panel.hidden = !on;
      });
      if (focus) tab.focus();
    }
    tabs.forEach(function (tab, i) {
      tab.addEventListener("click", function () { select(tab, false); });
      tab.addEventListener("keydown", function (e) {
        var step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[e.key];
        if (!step) return;
        e.preventDefault();
        select(tabs[(i + step + tabs.length) % tabs.length], true);
      });
    });
    if (tabs.length) select(tabs[0], false);
  });

  // 5. The table of contents marks the section being read.
  var toc = document.querySelector(".toc");
  if (toc && "IntersectionObserver" in window) {
    var links = {};
    toc.querySelectorAll('a[href^="#"]').forEach(function (a) { links[a.getAttribute("href").slice(1)] = a; });
    var seen = new Set();
    var spy = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) { if (e.isIntersecting) seen.add(e.target.id); else seen.delete(e.target.id); });
      var first = Object.keys(links).find(function (id) { return seen.has(id); });
      Object.keys(links).forEach(function (id) { links[id].classList.toggle("is-on", id === first); });
    }, { rootMargin: "-20% 0px -60% 0px" });
    Object.keys(links).forEach(function (id) { var el = document.getElementById(id); if (el) spy.observe(el); });
  }

  // 6. The drawings move only while they are on screen.
  if ("IntersectionObserver" in window) {
    var live = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) { e.target.setAttribute("data-live", e.isIntersecting ? "1" : "0"); });
    });
    document.querySelectorAll(".ill-wrap").forEach(function (el) { live.observe(el); });
  }

  // 7. The tick. Operations queue on the ring; one io_uring_enter submits them
  //    all and reaps their completions. The second row counts what the same
  //    work costs when every operation is its own system call.
  var tick = document.getElementById("tick");
  var still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (!tick || still) return;

  var segs = Array.prototype.slice.call(tick.querySelectorAll(".seg"));
  var epoll = Array.prototype.slice.call(tick.querySelectorAll("[data-marks-epoll] i"));
  var giong = tick.querySelector("[data-marks-giong] i");
  var ops = tick.querySelector("[data-ops]");
  var sys = tick.querySelector("[data-sys]");
  var unit = tick.querySelector("[data-unit]");
  var nGiong = tick.querySelector("[data-n-giong]");
  var nEpoll = tick.querySelector("[data-n-epoll]");
  var N = segs.length;
  var head = 14;          // the page ships with segments 0..13 lit
  var timer = null;

  function show(queued, calls) {
    ops.textContent = queued;
    sys.textContent = calls;
    unit.textContent = tick.getAttribute(calls === 1 ? "data-unit1" : "data-unitn");
    nGiong.textContent = calls;
    nEpoll.textContent = queued;
    giong.classList.toggle("on", calls > 0);
    epoll.forEach(function (m, i) { m.classList.toggle("on", i < queued); });
  }
  function later(fn, ms) { timer = setTimeout(fn, ms); }

  function fill(start, want) {
    // Tasks run and queue their operations: nothing has entered the kernel.
    var k = 0;
    (function next() {
      if (k === want) return later(function () { enter(start, want); }, 420);
      segs[(start + k) % N].className.baseVal = "seg on";
      k += 1;
      show(k, 0);
      later(next, 70 + Math.random() * 130);
    })();
  }
  function enter(start, count) {
    // One system call: everything is submitted, every completion comes back.
    tick.classList.add("enter");
    show(count, 1);
    for (var k = 0; k < count; k++) segs[(start + k) % N].className.baseVal = "seg done";
    later(function () { drain(start, count); }, 900);
  }
  function drain(start, count) {
    var k = 0;
    (function next() {
      if (k === count) {
        tick.classList.remove("enter");
        head = (start + count) % N;
        return later(run, 500);
      }
      segs[(start + k) % N].className.baseVal = "seg";
      k += 1;
      later(next, 28);
    })();
  }
  function run() {
    show(0, 0);
    fill(head, 8 + Math.floor(Math.random() * 13));   // 8 to 20 operations a tick
  }

  // Start from the state the page ships with: hold it, then flush it.
  var io = "IntersectionObserver" in window ? new IntersectionObserver(function (entries) {
    var visible = entries[0].isIntersecting;
    if (visible && timer === null) later(function () { enter(0, 14); }, 1800);
    if (!visible && timer !== null && !tick.classList.contains("enter")) {
      // Off screen: stop spending CPU. An idle runtime should cost nothing.
      clearTimeout(timer);
      timer = null;
      segs.forEach(function (s, i) { s.className.baseVal = i < 14 ? "seg on" + (i < 3 ? " a" + (i + 1) : "") : "seg"; });
      head = 14;
      show(14, 1);
    }
  }, { threshold: 0.2 }) : null;
  if (io) io.observe(tick); else later(function () { enter(0, 14); }, 1800);
})();

/* A link to a folded milestone on the roadmap (#05--http2-and-adapters-done) opens it. */
(function () {
  function open() {
    var id = decodeURIComponent(location.hash.slice(1));
    var el = id && document.getElementById(id);
    if (!el || !el.closest) return;
    var d = el.closest("details");
    while (d) { d.open = true; d = d.parentElement && d.parentElement.closest("details"); }
    if (el.tagName === "DETAILS") el.scrollIntoView();
  }
  open();
  window.addEventListener("hashchange", open);
})();

/* Bars that grow when they come into view, the terminal that types the measured build,
   and the roadmap's track scrolled to where giong is now. Without JavaScript, or for
   readers who ask for reduced motion, everything is shown as it is. */
(function () {
  "use strict";
  var still = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var watch = "IntersectionObserver" in window;

  var reveal = document.querySelectorAll("[data-reveal]");
  if (!watch || still) {
    reveal.forEach(function (el) { el.classList.add("in"); });
  } else {
    var seen = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) { e.target.classList.add("in"); seen.unobserve(e.target); }
      });
    }, { threshold: 0.25 });
    reveal.forEach(function (el) { seen.observe(el); });
  }

  var term = document.getElementById("typed");
  if (term && watch && !still) {
    var lines = Array.prototype.slice.call(term.querySelectorAll(".tl"));
    term.classList.add("armed");
    var started = false;
    var t = new IntersectionObserver(function (entries) {
      if (!entries[0].isIntersecting || started) return;
      started = true;
      t.disconnect();
      var i = 0;
      (function next() {
        if (i >= lines.length) return;
        var line = lines[i++];
        var text = line.querySelector(".tl-t");
        line.classList.add("shown");
        if (!text) return setTimeout(next, line.classList.contains("ok") ? 650 : 140);
        var full = text.textContent;
        var n = 0;
        text.textContent = "";
        line.classList.add("typing");
        (function type() {
          if (n < full.length) {
            n += 1;
            text.textContent = full.slice(0, n);
            return setTimeout(type, 18 + Math.random() * 40);
          }
          line.classList.remove("typing");
          setTimeout(next, 380);
        })();
      })();
    }, { threshold: 0.5 });
    t.observe(term);
  }

  var here = document.querySelector('.journey [aria-current="step"]');
  var wrap = here && here.closest(".journey-wrap");
  if (wrap) wrap.scrollLeft = Math.max(0, here.offsetLeft - wrap.clientWidth * 0.6);
})();
