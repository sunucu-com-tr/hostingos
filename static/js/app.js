/* HostingOS UI behaviour. No inline scripts anywhere (strict CSP): pages
   express intent with data-* attributes and this file wires them up.

   data-theme-toggle            toggles light / dark (persisted)
   data-dialog-open="<id>"      opens <dialog id> as a modal
   data-dialog-close            closes the enclosing <dialog>
   data-sidebar-toggle          opens / closes the panel sidebar (mobile)
   data-copy="<selector|text>"  copies text to the clipboard
   form[data-confirm="…"]       asks before submitting
   data-autosubmit              submits the parent form on change
   data-demo-fill="<form id>"   fills + submits a login form (public demo)
   data-demo-reset-at="<unix>"  live "next reset in N min" countdown (public demo)
   .toast                       auto-dismissed after 6 s; HX-Trigger "hos:toast" adds new ones
   .table-wrap                  gets data-fits when its table fits (sticky header)
   htmx requests get the X-CSRF-Token header from <meta name="csrf-token">. */
(function () {
  "use strict";
  var doc = document;
  var root = doc.documentElement;

  function csrfToken() {
    var m = doc.querySelector('meta[name="csrf-token"]');
    return m ? m.getAttribute("content") : "";
  }

  /* ---------------------------------------------------------------- theme */
  function effectiveTheme() {
    var t = root.getAttribute("data-theme");
    if (t === "light" || t === "dark") return t;
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches
      ? "dark"
      : "light";
  }
  doc.addEventListener("click", function (ev) {
    var btn = ev.target.closest("[data-theme-toggle]");
    if (!btn) return;
    var next = effectiveTheme() === "dark" ? "light" : "dark";
    root.setAttribute("data-theme", next);
    try {
      localStorage.setItem("hos-theme", next);
    } catch (e) {}
    btn.setAttribute("aria-pressed", next === "dark" ? "true" : "false");
  });

  /* ---------------------------------------------------------------- dialogs */
  doc.addEventListener("click", function (ev) {
    var opener = ev.target.closest("[data-dialog-open]");
    if (opener) {
      var d = doc.getElementById(opener.getAttribute("data-dialog-open"));
      if (d && typeof d.showModal === "function") {
        ev.preventDefault();
        d.showModal();
      }
      return;
    }
    var closer = ev.target.closest("[data-dialog-close]");
    if (closer) {
      var dlg = closer.closest("dialog");
      if (dlg) {
        ev.preventDefault();
        dlg.close();
      }
      return;
    }
    if (ev.target.tagName === "DIALOG" && ev.target.open) {
      var r = ev.target.getBoundingClientRect();
      var inside =
        ev.clientX >= r.left && ev.clientX <= r.right && ev.clientY >= r.top && ev.clientY <= r.bottom;
      if (!inside) ev.target.close();
    }
  });

  /* ---------------------------------------------------------------- sidebar */
  function setSidebar(open) {
    doc.body.classList.toggle("sidebar-open", open);
    var backdrop = doc.querySelector(".sidebar-backdrop");
    if (backdrop) backdrop.hidden = !open;
    doc.querySelectorAll("[data-sidebar-toggle]").forEach(function (b) {
      b.setAttribute("aria-expanded", open ? "true" : "false");
    });
  }
  doc.addEventListener("click", function (ev) {
    if (ev.target.closest("[data-sidebar-toggle]")) {
      setSidebar(!doc.body.classList.contains("sidebar-open"));
    } else if (ev.target.classList && ev.target.classList.contains("sidebar-backdrop")) {
      setSidebar(false);
    }
  });

  /* ---------------------------------------------------------------- menus */
  doc.addEventListener("click", function (ev) {
    doc.querySelectorAll("details[data-menu][open]").forEach(function (d) {
      if (!d.contains(ev.target)) d.removeAttribute("open");
    });
  });
  doc.addEventListener("keydown", function (ev) {
    if (ev.key !== "Escape") return;
    doc.querySelectorAll("details[data-menu][open]").forEach(function (d) {
      d.removeAttribute("open");
      var s = d.querySelector("summary");
      if (s) s.focus();
    });
    if (doc.body.classList.contains("sidebar-open")) setSidebar(false);
  });

  /* ---------------------------------------------------------------- copy */
  doc.addEventListener("click", function (ev) {
    var btn = ev.target.closest("[data-copy]");
    if (!btn || !navigator.clipboard) return;
    var spec = btn.getAttribute("data-copy");
    var el = spec && spec.charAt(0) === "#" ? doc.querySelector(spec) : null;
    var text = el ? el.textContent : spec;
    navigator.clipboard.writeText((text || "").trim()).then(function () {
      var label = btn.getAttribute("data-copied") || "✓";
      var old = btn.textContent;
      btn.textContent = label;
      setTimeout(function () {
        btn.textContent = old;
      }, 1500);
    });
  });

  /* ------------------------------------------------------------ demo login */
  /* Public demo (docs/DEMO.md): data-demo-fill="<form id>" with
     data-email / data-password fills that login form and submits it. */
  doc.addEventListener("click", function (ev) {
    var btn = ev.target.closest("[data-demo-fill]");
    if (!btn) return;
    var form = doc.getElementById(btn.getAttribute("data-demo-fill"));
    if (!form) return;
    var email = form.querySelector('input[name="email"]');
    var password = form.querySelector('input[name="password"]');
    if (!email || !password) return;
    ev.preventDefault();
    email.value = btn.getAttribute("data-email") || "";
    password.value = btn.getAttribute("data-password") || "";
    if (typeof form.requestSubmit === "function") form.requestSubmit();
    else form.submit();
  });

  /* Countdown to the next demo reset: data-template holds the localized
     text with {minutes}, data-soon the text once the time is up. */
  function demoCountdown() {
    doc.querySelectorAll("[data-demo-reset-at]").forEach(function (el) {
      var at = parseInt(el.getAttribute("data-demo-reset-at"), 10);
      if (!at) return;
      var left = Math.ceil((at * 1000 - Date.now()) / 60000);
      if (left <= 0) {
        el.textContent = el.getAttribute("data-soon") || "";
      } else {
        el.textContent = (el.getAttribute("data-template") || "").replace("{minutes}", String(left));
      }
    });
  }
  if (doc.querySelector("[data-demo-reset-at]")) {
    demoCountdown();
    setInterval(demoCountdown, 15000);
  }

  /* ---------------------------------------------------------------- forms */
  doc.addEventListener("submit", function (ev) {
    var form = ev.target;
    var msg = form.getAttribute && form.getAttribute("data-confirm");
    if (msg && !window.confirm(msg)) {
      ev.preventDefault();
      return;
    }
    var btn = form.querySelector('button[type="submit"]:not([data-no-busy])');
    if (btn && !form.hasAttribute("hx-post") && !form.hasAttribute("hx-get")) {
      btn.setAttribute("aria-busy", "true");
    }
  });
  doc.addEventListener("change", function (ev) {
    if (ev.target.hasAttribute && ev.target.hasAttribute("data-autosubmit") && ev.target.form) {
      ev.target.form.requestSubmit ? ev.target.form.requestSubmit() : ev.target.form.submit();
    }
  });

  /* ---------------------------------------------------------------- toasts */
  var ICONS = { success: "check-circle", error: "alert-triangle", warning: "alert-triangle", info: "info" };
  function spriteUrl() {
    var meta = doc.querySelector('meta[name="hos-icons"]');
    return meta ? meta.getAttribute("content") : "/static/img/icons.svg";
  }
  function dismiss(t) {
    if (!t || t.hasAttribute("data-leaving")) return;
    t.setAttribute("data-leaving", "");
    setTimeout(function () {
      t.remove();
    }, 220);
  }
  function arm(t) {
    if (t.hasAttribute("data-armed")) return;
    t.setAttribute("data-armed", "");
    var timer = setTimeout(function () {
      dismiss(t);
    }, 6000);
    t.addEventListener("mouseenter", function () {
      clearTimeout(timer);
    });
    var close = t.querySelector("[data-toast-close]");
    if (close)
      close.addEventListener("click", function () {
        dismiss(t);
      });
  }
  function toast(kind, text) {
    var box = doc.querySelector(".toasts");
    if (!box) {
      box = doc.createElement("div");
      box.className = "toasts";
      box.setAttribute("aria-live", "polite");
      doc.body.appendChild(box);
    }
    var t = doc.createElement("div");
    t.className = "toast toast--" + kind;
    t.setAttribute("role", kind === "error" ? "alert" : "status");
    var svg = doc.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "icon");
    svg.setAttribute("aria-hidden", "true");
    var use = doc.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", spriteUrl() + "#" + (ICONS[kind] || "info"));
    svg.appendChild(use);
    var p = doc.createElement("p");
    p.className = "toast__text";
    p.textContent = text;
    var b = doc.createElement("button");
    b.type = "button";
    b.className = "btn btn--ghost btn--sm btn--icon";
    b.setAttribute("data-toast-close", "");
    b.setAttribute("aria-label", "×");
    b.textContent = "×";
    t.appendChild(svg);
    t.appendChild(p);
    t.appendChild(b);
    box.appendChild(t);
    arm(t);
  }
  doc.querySelectorAll(".toast").forEach(arm);
  doc.body.addEventListener("hos:toast", function (ev) {
    var list = (ev.detail && (ev.detail.value || ev.detail)) || [];
    if (!Array.isArray(list)) list = [list];
    list.forEach(function (m) {
      if (m && m.text) toast(m.kind || "info", m.text);
    });
  });

  /* ---------------------------------------------------------------- tables
     A .table-wrap scrolls sideways only when its table is wider than the
     column. When it fits, it is marked data-fits (overflow: clip in CSS), so
     the header can stick under the page chrome instead of inside the wrap.
     Same box either way: no layout change. */
  var wrapObserver =
    "ResizeObserver" in window
      ? new ResizeObserver(function (entries) {
          entries.forEach(function (e) {
            var w = e.target.closest ? e.target.closest(".table-wrap") : null;
            if (w) fit(w);
          });
        })
      : null;
  function fit(w) {
    var fits = w.scrollWidth <= w.clientWidth + 1;
    if (fits !== w.hasAttribute("data-fits")) {
      if (fits) w.setAttribute("data-fits", "");
      else w.removeAttribute("data-fits");
    }
  }
  function watchTables(scope) {
    (scope || doc).querySelectorAll(".table-wrap").forEach(function (w) {
      fit(w);
      if (wrapObserver && !w.hasAttribute("data-watched")) {
        w.setAttribute("data-watched", "");
        wrapObserver.observe(w);
        var t = w.querySelector("table");
        if (t) wrapObserver.observe(t);
      }
    });
  }
  watchTables(doc);

  /* ---------------------------------------------------------- header fit
     The storefront menu sits inline on wide screens. When it does not fit
     (many pages, long labels, a narrow window) the header is marked
     data-cramped and the mobile menu takes over (layout.css). */
  var siteHeader = doc.querySelector(".site-header");
  var siteNav = siteHeader ? siteHeader.querySelector(".site-nav") : null;
  function fitHeader() {
    siteHeader.removeAttribute("data-cramped");
    if (window.getComputedStyle(siteNav).display === "none") return;
    if (siteNav.scrollWidth > siteNav.clientWidth + 1) {
      siteHeader.setAttribute("data-cramped", "");
    }
  }
  if (siteNav) {
    fitHeader();
    if ("ResizeObserver" in window) new ResizeObserver(fitHeader).observe(siteHeader);
    else window.addEventListener("resize", fitHeader);
    if (doc.fonts && doc.fonts.ready) doc.fonts.ready.then(fitHeader);
  }

  /* ---------------------------------------------------------------- htmx */
  doc.body.addEventListener("htmx:afterSettle", function (ev) {
    var el = ev.detail && ev.detail.elt;
    watchTables(el && el.parentNode ? el.parentNode : doc);
  });
  doc.body.addEventListener("htmx:configRequest", function (ev) {
    var token = csrfToken();
    if (token) ev.detail.headers["X-CSRF-Token"] = token;
  });
  doc.body.addEventListener("htmx:responseError", function (ev) {
    var xhr = ev.detail && ev.detail.xhr;
    if (xhr && !xhr.getResponseHeader("HX-Trigger")) {
      toast("error", doc.documentElement.getAttribute("data-error-text") || "Error");
    }
  });
  doc.body.addEventListener("htmx:sendError", function () {
    toast("error", doc.documentElement.getAttribute("data-offline-text") || "Network error");
  });
})();
