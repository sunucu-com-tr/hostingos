/* Appearance settings page (strict CSP: no inline scripts).

   - Live preview: every change of the form rebuilds the staff-only preview
     query (crate::theme::Choice::preview_query) and points the iframe at
     the selected storefront page; nothing is saved.
   - Template switch: the "Template default (…)" labels, their specimen
     faces and the colour placeholders follow the newly picked template.
   - The iframe renders the storefront at 1280 px (or 390 px) and is scaled
     down to the panel with a CSS transform (CSSOM, not an inline style). */
(function () {
  "use strict";
  var root = document.querySelector("[data-appearance]");
  if (!root) return;
  var form = document.getElementById("appearance-form");
  var frame = root.querySelector("[data-preview-frame]");
  var wrap = root.querySelector("[data-preview-wrap]");
  var pageSel = root.querySelector("[data-preview-page]");
  var openLink = root.querySelector("[data-preview-open]");
  var defaultLabel = root.getAttribute("data-default-label") || "{value}";
  var HEX = /^#?([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;
  var device = 1280;
  var timer = null;

  function value(name) {
    var el = form.elements[name];
    if (!el) return "";
    if (el instanceof RadioNodeList || (el.length && !el.tagName)) {
      for (var i = 0; i < el.length; i++) if (el[i].checked) return el[i].value;
      return "";
    }
    if (el.type === "checkbox") return el.checked ? "1" : "0";
    return el.value || "";
  }

  function hex(name) {
    var v = value(name).trim();
    return HEX.test(v) ? v.replace("#", "").toLowerCase() : "";
  }

  function query() {
    var p = new URLSearchParams();
    p.set("theme_preview", "1");
    p.set("tpl", value("template"));
    p.set("brand", hex("brand_color"));
    p.set("accent", hex("accent_color"));
    p.set("fb", value("font_body"));
    p.set("fh", value("font_heading"));
    p.set("r", value("radius"));
    p.set("m", value("mode"));
    p.set("tg", form.elements.mode_toggle && form.elements.mode_toggle.checked ? "1" : "0");
    return p.toString();
  }

  function pageHref() {
    var opt = pageSel && pageSel.options[pageSel.selectedIndex];
    return (opt && opt.getAttribute("data-href")) || "/";
  }

  function refresh() {
    var src = pageHref() + "?" + query();
    if (openLink) openLink.href = src;
    if (frame && frame.getAttribute("src") !== src) frame.setAttribute("src", src);
  }

  function schedule() {
    clearTimeout(timer);
    timer = setTimeout(refresh, 350);
  }

  function fit() {
    if (!wrap || !frame) return;
    var w = wrap.clientWidth;
    if (!w) return;
    var scale = Math.min(1, w / device);
    var h = device === 1280 ? 900 : 780;
    // Beside the form (sticky column) the frame fills most of the viewport
    // height, so more of the page is visible without scrolling the iframe.
    if (window.matchMedia && window.matchMedia("(min-width: 100rem)").matches) {
      var target = Math.max(360, Math.min(window.innerHeight - 220, 860));
      h = Math.round(target / scale);
    }
    frame.style.width = device + "px";
    frame.style.height = h + "px";
    frame.style.transform = "scale(" + scale + ")";
    wrap.style.height = Math.round(h * scale) + "px";
    wrap.setAttribute("data-scaled", "");
  }

  function selectedTemplate() {
    var checked = form.querySelector('input[name="template"]:checked');
    return checked || form.querySelector('input[name="template"]');
  }

  function swapSpecimen(el, key) {
    el.className = el.className.replace(/\bfont-specimen--[a-z0-9-]+/, "font-specimen--" + key);
  }

  function applyTemplateDefaults() {
    var t = selectedTemplate();
    if (!t) return;
    var d = t.dataset;
    root.querySelectorAll('[data-default-font="body"]').forEach(function (el) {
      swapSpecimen(el, d.bodyKey);
      if (el.classList.contains("font-option__name")) el.textContent = defaultLabel.replace("{value}", d.body);
    });
    root.querySelectorAll('[data-default-font="heading"]').forEach(function (el) {
      swapSpecimen(el, d.headingKey);
      if (el.classList.contains("font-option__name")) el.textContent = defaultLabel.replace("{value}", d.heading);
    });
    var r = root.querySelector('[data-default-label-for="radius"]');
    if (r) r.textContent = defaultLabel.replace("{value}", d.radius);
    var m = root.querySelector('[data-default-label-for="mode"]');
    if (m) m.textContent = defaultLabel.replace("{value}", d.mode);
    [["brand_color", d.brand], ["accent_color", d.accent]].forEach(function (pair) {
      var text = form.elements[pair[0]];
      if (!text) return;
      text.placeholder = pair[1];
      var picker = form.querySelector('[data-color-sync="' + text.id + '"]');
      if (picker && !HEX.test(text.value.trim())) picker.value = pair[1];
    });
  }

  form.addEventListener("change", function (ev) {
    if (ev.target && ev.target.name === "template") applyTemplateDefaults();
    schedule();
  });
  form.addEventListener("input", schedule);
  form.addEventListener("reset", function () {
    setTimeout(function () {
      applyTemplateDefaults();
      refresh();
    }, 0);
  });
  if (pageSel) pageSel.addEventListener("change", refresh);
  document.querySelectorAll("[data-preview-device]").forEach(function (r) {
    r.addEventListener("change", function () {
      device = parseInt(r.value, 10) || 1280;
      wrap.setAttribute("data-device", String(device));
      fit();
    });
  });
  if (window.ResizeObserver && wrap) new ResizeObserver(fit).observe(wrap);
  window.addEventListener("resize", fit);
  fit();
})();
