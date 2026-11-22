/* HostingOS admin — content, support and SEO screens (strict CSP: no inline
   scripts). Loaded by those templates only.

   [data-serp-scope]            one SERP preview per language column:
     [data-cserp]               the preview box (data-suffix = title suffix)
     [data-cserp-title]         meta title input (falls back to the page title + suffix)
     [data-cserp-desc]          meta description (falls back to the excerpt / body)
     [data-slug-source]         page title input
     [data-slug-target]         slug input (data-url-prefix = absolute URL prefix);
                                filled from the title while the user has not typed in it
   [data-fill-into="#id"]       button: copies the value of [data-fill-from] into
                                the target textarea (AI reply draft → reply box)
   .trend__bar[data-h]          bar height in percent (CSSOM, CSP-safe) */
(function () {
  "use strict";
  var doc = document;

  var MAP = { "ç": "c", "ğ": "g", "ı": "i", "İ": "i", "ö": "o", "ş": "s", "ü": "u",
              "Ç": "c", "Ğ": "g", "Ö": "o", "Ş": "s", "Ü": "u", "â": "a", "î": "i", "û": "u" };
  function slugify(s) {
    return s.replace(/[çğıİöşüÇĞÖŞÜâîû]/g, function (c) { return MAP[c] || c; })
      .normalize("NFKD").replace(/[̀-ͯ]/g, "")
      .toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 120);
  }

  function plain(md) {
    return md.replace(/```[\s\S]*?```/g, " ").replace(/[#>*_`\[\]()!|-]+/g, " ")
      .replace(/\s+/g, " ").trim();
  }

  function cut(s, n) {
    return s.length > n ? s.slice(0, n - 1) + "…" : s;
  }

  function serp(scope) {
    var box = scope.querySelector("[data-cserp]");
    if (!box) return;
    var val = function (sel) {
      var el = scope.querySelector(sel);
      return el ? (el.value || "").trim() : "";
    };
    var title = val("[data-cserp-title]") || (val("[data-slug-source]") + (box.getAttribute("data-suffix") || ""));
    var desc = val("[data-cserp-desc]") || val("[data-serp-excerpt]") || plain(val("[data-serp-body]"));
    var slugEl = scope.querySelector("[data-slug-target]");
    var t = box.querySelector(".serp__title");
    var d = box.querySelector(".serp__desc");
    var u = box.querySelector(".serp__url");
    if (t) t.textContent = cut(title, 60);
    if (d) d.textContent = cut(desc, 160);
    if (u && slugEl) u.textContent = (slugEl.getAttribute("data-url-prefix") || "") + (slugEl.value || "…");
  }

  doc.querySelectorAll("[data-slug-target]").forEach(function (el) {
    if (el.value) el.setAttribute("data-touched", "1");
  });
  doc.querySelectorAll("[data-serp-scope]").forEach(serp);

  doc.addEventListener("input", function (ev) {
    var el = ev.target;
    if (!el.closest) return;
    var scope = el.closest("[data-serp-scope]");
    if (!scope) return;
    if (el.hasAttribute("data-slug-target")) {
      el.setAttribute("data-touched", el.value ? "1" : "");
    }
    if (el.hasAttribute("data-slug-source")) {
      var slug = scope.querySelector("[data-slug-target]");
      if (slug && !slug.getAttribute("data-touched")) slug.value = slugify(el.value);
    }
    serp(scope);
  });

  doc.querySelectorAll(".trend__bar[data-h]").forEach(function (b) {
    var h = parseInt(b.getAttribute("data-h"), 10);
    if (h >= 0 && h <= 100) b.style.height = h + "%";
  });

  /* AI reply draft → reply box (the reply is only ever sent by the agent). */
  doc.querySelectorAll("[data-fill-into]").forEach(function (b) { b.hidden = false; });
  doc.body.addEventListener("htmx:afterSwap", function (ev) {
    ev.target.querySelectorAll && ev.target.querySelectorAll("[data-fill-into]").forEach(function (b) { b.hidden = false; });
  });
  doc.addEventListener("click", function (ev) {
    var b = ev.target.closest && ev.target.closest("[data-fill-into]");
    if (!b) return;
    var from = doc.querySelector(b.getAttribute("data-fill-from"));
    var to = doc.querySelector(b.getAttribute("data-fill-into"));
    if (!from || !to) return;
    to.value = from.value;
    to.dispatchEvent(new Event("input", { bubbles: true }));
    to.focus();
    to.scrollIntoView({ block: "center" });
  });
})();
