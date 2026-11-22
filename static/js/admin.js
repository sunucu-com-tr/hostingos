/* HostingOS admin behaviour (strict CSP: no inline scripts).

   Shell
   [data-cmdk-open]            opens the command palette (#ax-cmdk); also "/"
                               (when not typing) and Ctrl/Cmd+K. ↑/↓ move the
                               selection, Enter opens it, Esc closes.
   [data-cmdk]                 palette input (htmx loads #cmdk-results)
   [data-cmdk-local]           pages + quick actions shown for short queries
   [data-rail-toggle]          collapses the sidebar to an icon rail
                               (remembered; admin_boot.js applies it early)
   [data-tip="…"]              tooltip (top bar always, sidebar in rail mode)
   form[data-confirm="…"]      native <dialog> confirmation (#ax-confirm)
                               instead of window.confirm
   .form-actions--sticky       its form gets data-dirty after an edit (save
                               bar floats, "unsaved changes" shows)
   table rows with .row-link   the whole row opens the link
   details.row-menu            row action menu, positioned fixed so the table
                               never clips it
   .admin-grid--side > aside   data-sticky when it fits the viewport
   .bulkbar                    selected-row count for the enclosing form
   .table-wrap[aria-busy]      skeleton rows while a list filter / sort /
                               page link navigates

   Page helpers
   [data-check-all="name"]     toggles every checkbox named `name` in its form
   [data-count-max="n"]        live character counter for an input/textarea
                               (renders into the element [data-counter-for=id])
   [data-serp]                 live SERP preview: inputs with data-serp-title /
                               data-serp-desc / data-serp-url update the box
   [data-insert-into="#id"]    <select>: appends the chosen option's
                               data-body to the textarea, then resets
   [data-brand-preview]        colour inputs (data-brand-var="--x") update the
                               preview element's custom properties
   [data-show-when="name=value"] shows the element only when the named
                               control in the same form has that value */
(function () {
  "use strict";
  var doc = document;
  var root = doc.documentElement;
  var shell = doc.getElementById("ax");

  function closest(el, sel) {
    return el && el.closest ? el.closest(sel) : null;
  }
  function isTyping(el) {
    var tag = (el && el.tagName ? el.tagName : "").toLowerCase();
    return tag === "input" || tag === "textarea" || tag === "select" || (el && el.isContentEditable);
  }
  function shellText(name, fallback) {
    return (shell && shell.getAttribute("data-" + name)) || fallback || "";
  }

  /* ---------------------------------------------------------------- command palette */
  var palette = doc.getElementById("ax-cmdk");
  var paletteInput = palette ? palette.querySelector("[data-cmdk]") : null;
  var paletteLocal = palette ? palette.querySelector("[data-cmdk-local]") : null;
  var paletteResults = doc.getElementById("cmdk-results");

  function paletteItems() {
    if (!palette) return [];
    var scope = paletteLocal && !paletteLocal.hidden ? paletteLocal : paletteResults;
    if (!scope) return [];
    return Array.prototype.filter.call(scope.querySelectorAll(".cmdk__item"), function (a) {
      return !a.hidden;
    });
  }
  function select(list, idx) {
    if (!palette) return;
    palette.querySelectorAll('.cmdk__item[aria-selected="true"]').forEach(function (a) {
      a.removeAttribute("aria-selected");
    });
    if (!list.length) {
      if (paletteInput) paletteInput.removeAttribute("aria-activedescendant");
      return;
    }
    var i = Math.max(0, Math.min(idx, list.length - 1));
    var item = list[i];
    item.setAttribute("aria-selected", "true");
    if (!item.id) item.id = "cmdk-opt-" + Math.random().toString(36).slice(2, 8);
    if (paletteInput) paletteInput.setAttribute("aria-activedescendant", item.id);
    item.scrollIntoView({ block: "nearest" });
  }
  function selectedIndex(list) {
    for (var i = 0; i < list.length; i++) if (list[i].getAttribute("aria-selected") === "true") return i;
    return -1;
  }
  function filterLocal() {
    if (!paletteLocal || !paletteInput) return;
    var q = paletteInput.value.trim().toLocaleLowerCase();
    var server = q.length >= 2;
    paletteLocal.hidden = server;
    if (paletteResults && !server) paletteResults.innerHTML = "";
    var groups = paletteLocal.querySelectorAll(".cmdk__group");
    paletteLocal.querySelectorAll(".cmdk__item").forEach(function (a) {
      a.hidden = q.length === 1 && a.textContent.toLocaleLowerCase().indexOf(q) === -1;
    });
    groups.forEach(function (g) {
      var n = g.nextElementSibling;
      var any = false;
      while (n && !n.classList.contains("cmdk__group")) {
        if (n.classList.contains("cmdk__item") && !n.hidden) any = true;
        n = n.nextElementSibling;
      }
      g.hidden = !any;
    });
    select(paletteItems(), 0);
  }
  function openPalette() {
    if (!palette || typeof palette.showModal !== "function") return false;
    if (!palette.open) palette.showModal();
    if (paletteInput) {
      paletteInput.focus();
      paletteInput.select();
    }
    filterLocal();
    return true;
  }
  if (palette) {
    palette.addEventListener("close", function () {
      if (paletteInput) paletteInput.value = "";
      if (paletteResults) paletteResults.innerHTML = "";
      filterLocal();
    });
    palette.addEventListener("mousemove", function (ev) {
      var item = closest(ev.target, ".cmdk__item");
      if (!item) return;
      var list = paletteItems();
      var i = list.indexOf(item);
      if (i !== -1 && item.getAttribute("aria-selected") !== "true") select(list, i);
    });
  }
  doc.addEventListener("click", function (ev) {
    var opener = closest(ev.target, "[data-cmdk-open]");
    if (opener && !ev.metaKey && !ev.ctrlKey && !ev.shiftKey && openPalette()) ev.preventDefault();
  });
  doc.addEventListener("keydown", function (ev) {
    if (!palette) return;
    var k = ev.key;
    if ((ev.ctrlKey || ev.metaKey) && (k === "k" || k === "K")) {
      ev.preventDefault();
      if (palette.open) palette.close();
      else openPalette();
      return;
    }
    if (k === "/" && !palette.open && !isTyping(ev.target) && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
      if (doc.querySelector("dialog[open]")) return;
      ev.preventDefault();
      openPalette();
    }
  });
  if (paletteInput) {
    paletteInput.addEventListener("input", filterLocal);
    paletteInput.addEventListener("keydown", function (ev) {
      var list = paletteItems();
      var idx = selectedIndex(list);
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        select(list, idx + 1);
      } else if (ev.key === "ArrowUp") {
        ev.preventDefault();
        select(list, idx - 1);
      } else if (ev.key === "Home" && ev.ctrlKey) {
        select(list, 0);
      } else if (ev.key === "Enter") {
        var item = list[idx >= 0 ? idx : 0];
        if (item && item.getAttribute("href")) {
          ev.preventDefault();
          window.location.href = item.getAttribute("href");
        }
      }
    });
  }
  doc.body.addEventListener("htmx:afterSwap", function (ev) {
    if (ev.target === paletteResults) {
      if (paletteInput && paletteInput.value.trim().length < 2) paletteResults.innerHTML = "";
      select(paletteItems(), 0);
    }
  });

  /* ---------------------------------------------------------------- rail */
  function railOn() {
    return root.hasAttribute("data-rail");
  }
  function syncRail() {
    var on = railOn();
    doc.querySelectorAll("[data-rail-toggle]").forEach(function (b) {
      b.setAttribute("aria-pressed", on ? "true" : "false");
      var label = on ? shellText("expand-text") : shellText("collapse-text");
      b.setAttribute("aria-label", label);
      b.setAttribute("data-tip", label);
      var span = b.querySelector(".ax-nav__label");
      if (span) span.textContent = label;
    });
  }
  doc.addEventListener("click", function (ev) {
    if (!closest(ev.target, "[data-rail-toggle]")) return;
    if (railOn()) root.removeAttribute("data-rail");
    else root.setAttribute("data-rail", "");
    try {
      localStorage.setItem("hos-admin-rail", railOn() ? "1" : "0");
    } catch (e) {}
    hideTip();
    syncRail();
  });
  syncRail();

  /* ---------------------------------------------------------------- tooltips */
  var tip = null;
  var tipTimer = 0;
  var desktop = window.matchMedia ? window.matchMedia("(min-width: 64rem)") : null;
  function tipFor(el) {
    var t = closest(el, "[data-tip]");
    if (!t) return null;
    if (closest(t, ".ax-side")) return railOn() && (!desktop || desktop.matches) ? t : null;
    return closest(t, ".ax-top") ? t : null;
  }
  function hideTip() {
    clearTimeout(tipTimer);
    if (tip) tip.removeAttribute("data-show");
  }
  function showTip(t) {
    if (!tip) {
      tip = doc.createElement("div");
      tip.className = "ax-tooltip";
      tip.setAttribute("role", "tooltip");
      tip.id = "ax-tooltip";
      doc.body.appendChild(tip);
    }
    tip.textContent = t.getAttribute("data-tip");
    var r = t.getBoundingClientRect();
    var side = !!closest(t, ".ax-side");
    tip.style.left = "0px";
    tip.style.top = "0px";
    var w = tip.offsetWidth;
    var h = tip.offsetHeight;
    var x;
    var y;
    if (side) {
      x = r.right + 10;
      y = r.top + r.height / 2 - h / 2;
    } else {
      x = Math.min(Math.max(8, r.left + r.width / 2 - w / 2), window.innerWidth - w - 8);
      y = r.bottom + 8;
    }
    tip.style.left = Math.round(x) + "px";
    tip.style.top = Math.round(y) + "px";
    tip.setAttribute("data-show", "");
  }
  doc.addEventListener("pointerover", function (ev) {
    if (ev.pointerType === "touch") return;
    var t = tipFor(ev.target);
    if (!t) return;
    clearTimeout(tipTimer);
    tipTimer = setTimeout(function () {
      showTip(t);
    }, 300);
  });
  doc.addEventListener("pointerout", function (ev) {
    var t = tipFor(ev.target);
    if (t && !t.contains(ev.relatedTarget)) hideTip();
  });
  doc.addEventListener("focusin", function (ev) {
    var t = tipFor(ev.target);
    if (t && t.matches(":focus-visible")) showTip(t);
    else hideTip();
  });
  doc.addEventListener("focusout", hideTip);
  window.addEventListener("scroll", hideTip, { passive: true });
  doc.addEventListener("click", hideTip);

  /* ---------------------------------------------------------------- confirm dialog */
  var confirmDlg = doc.getElementById("ax-confirm");
  var DANGER = /(delete|remove|revoke|terminate|cancel|deactivate|destroy|purge|reset|sil|kaldir)/i;
  var pending = null;
  doc.addEventListener(
    "submit",
    function (ev) {
      var form = ev.target;
      if (!confirmDlg || typeof confirmDlg.showModal !== "function") return;
      if (!form.hasAttribute || !form.hasAttribute("data-confirm")) return;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      var submitter = ev.submitter || null;
      pending = { form: form, submitter: submitter };
      var text = doc.getElementById("ax-confirm-text");
      if (text) text.textContent = form.getAttribute("data-confirm");
      var ok = confirmDlg.querySelector("[data-confirm-ok]");
      var label = submitter ? (submitter.getAttribute("aria-label") || submitter.textContent || "").trim() : "";
      if (ok) ok.textContent = label && label.length <= 28 ? label : ok.getAttribute("data-default-label");
      var action = (form.getAttribute("action") || "") + " " + (submitter ? submitter.className : "");
      var danger = DANGER.test(action) || (submitter && /btn--danger/.test(submitter.className));
      confirmDlg.setAttribute("data-tone", danger ? "danger" : "primary");
      if (ok) ok.className = danger ? "btn btn--danger" : "btn btn--primary";
      confirmDlg.returnValue = "";
      confirmDlg.showModal();
      var cancel = confirmDlg.querySelector("[data-confirm-cancel]");
      if (cancel) cancel.focus();
    },
    true
  );
  if (confirmDlg) {
    confirmDlg.addEventListener("close", function () {
      var p = pending;
      pending = null;
      if (!p || confirmDlg.returnValue !== "ok") return;
      var msg = p.form.getAttribute("data-confirm");
      p.form.removeAttribute("data-confirm");
      if (p.form.requestSubmit) {
        if (p.submitter && p.submitter.form === p.form) p.form.requestSubmit(p.submitter);
        else p.form.requestSubmit();
      } else {
        p.form.submit();
      }
      setTimeout(function () {
        p.form.setAttribute("data-confirm", msg);
      }, 0);
    });
  }

  /* ---------------------------------------------------------------- unsaved changes */
  function trackable(form) {
    return form && form.querySelector && form.querySelector(".form-actions--sticky");
  }
  function markDirty(ev) {
    var form = ev.target && ev.target.form;
    if (!form || !trackable(form) || ev.target.hasAttribute("data-no-dirty")) return;
    form.setAttribute("data-dirty", "");
  }
  doc.addEventListener("input", markDirty);
  doc.addEventListener("change", markDirty);
  doc.addEventListener("reset", function (ev) {
    var form = ev.target;
    setTimeout(function () {
      form.removeAttribute("data-dirty");
      doc.querySelectorAll("[data-count-max]").forEach(count);
    }, 0);
  });
  doc.addEventListener("submit", function (ev) {
    if (ev.defaultPrevented) return;
    if (ev.target.removeAttribute) ev.target.removeAttribute("data-dirty");
  });
  window.addEventListener("beforeunload", function (ev) {
    if (doc.querySelector("form[data-dirty]")) {
      ev.preventDefault();
      ev.returnValue = "";
    }
  });

  /* ---------------------------------------------------------------- row click */
  doc.addEventListener("click", function (ev) {
    if (ev.defaultPrevented || ev.button !== 0) return;
    var row = closest(ev.target, ".ax-content .table tbody tr");
    if (!row || closest(ev.target, "a, button, input, select, textarea, label, summary, details, form")) return;
    var sel = window.getSelection ? String(window.getSelection()) : "";
    if (sel) return;
    var link = row.querySelector(".row-link");
    if (!link || !link.getAttribute("href")) return;
    if (ev.metaKey || ev.ctrlKey) window.open(link.href, "_blank", "noopener");
    else window.location.href = link.href;
  });

  /* ---------------------------------------------------------------- menus */
  function menuItems(details) {
    return Array.prototype.slice.call(details.querySelectorAll(".menu__panel a, .menu__panel button"));
  }
  function placeRowMenu(details) {
    var panel = details.querySelector(".menu__panel");
    var summary = details.querySelector("summary");
    if (!panel || !summary) return;
    panel.setAttribute("data-fixed", "");
    var r = summary.getBoundingClientRect();
    var w = panel.offsetWidth;
    var h = panel.offsetHeight;
    var top = r.bottom + 4;
    if (top + h > window.innerHeight - 8) top = Math.max(8, r.top - h - 4);
    panel.style.left = Math.round(Math.max(8, r.right - w)) + "px";
    panel.style.top = Math.round(top) + "px";
  }
  doc.addEventListener(
    "toggle",
    function (ev) {
      var d = ev.target;
      if (!d.matches || !d.matches("details[data-menu]")) return;
      if (d.open) {
        doc.querySelectorAll("details[data-menu][open]").forEach(function (o) {
          if (o !== d && !o.contains(d)) o.removeAttribute("open");
        });
        if (d.classList.contains("row-menu")) placeRowMenu(d);
        if (d.hasAttribute("data-kbd-open")) {
          d.removeAttribute("data-kbd-open");
          var first = menuItems(d)[0];
          if (first) first.focus();
        }
      }
    },
    true
  );
  window.addEventListener(
    "scroll",
    function () {
      doc.querySelectorAll("details.row-menu[open]").forEach(function (d) {
        d.removeAttribute("open");
      });
    },
    { passive: true }
  );
  doc.addEventListener("keydown", function (ev) {
    var d = closest(ev.target, "details[data-menu]");
    if (!d) return;
    if (ev.target.tagName === "SUMMARY" && (ev.key === "Enter" || ev.key === " " || ev.key === "ArrowDown")) {
      if (!d.open) d.setAttribute("data-kbd-open", "");
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        if (!d.open) d.open = true;
        else {
          var f = menuItems(d)[0];
          if (f) f.focus();
        }
      }
      return;
    }
    if (!d.open || (ev.key !== "ArrowDown" && ev.key !== "ArrowUp" && ev.key !== "Home" && ev.key !== "End")) return;
    var items = menuItems(d);
    var i = items.indexOf(doc.activeElement);
    ev.preventDefault();
    if (ev.key === "Home") i = 0;
    else if (ev.key === "End") i = items.length - 1;
    else i = ev.key === "ArrowDown" ? (i + 1) % items.length : (i - 1 + items.length) % items.length;
    if (items[i]) items[i].focus();
  });

  /* ---------------------------------------------------------------- sticky side panel */
  function stickAside() {
    var limit = window.innerHeight - 96;
    doc.querySelectorAll(".admin-grid--side > aside, .admin-grid--side > [data-side]").forEach(function (a) {
      var fits = a.offsetHeight > 0 && a.offsetHeight < limit;
      if (fits !== a.hasAttribute("data-sticky")) {
        if (fits) a.setAttribute("data-sticky", "");
        else a.removeAttribute("data-sticky");
      }
    });
  }
  stickAside();
  window.addEventListener("resize", stickAside);
  if ("ResizeObserver" in window) {
    var ro = new ResizeObserver(stickAside);
    doc.querySelectorAll(".admin-grid--side > aside").forEach(function (a) {
      ro.observe(a);
    });
  }

  /* ---------------------------------------------------------------- bulk select */
  function bulkSync(form) {
    var bar = form && form.querySelector(".bulkbar");
    if (!bar) return;
    var n = form.querySelectorAll('tbody input[type="checkbox"]:checked').length;
    var out = bar.querySelector(".bulkbar__count");
    if (out) out.textContent = shellText("selected-text", "{count}").replace("{count}", String(n));
    if (n > 0) bar.setAttribute("data-has-selection", "");
    else bar.removeAttribute("data-has-selection");
  }
  doc.addEventListener("change", function (ev) {
    var all = closest(ev.target, "[data-check-all]");
    if (all) {
      var name = all.getAttribute("data-check-all");
      var scope = all.form || doc;
      scope.querySelectorAll('input[type="checkbox"][name="' + name + '"]').forEach(function (c) {
        c.checked = all.checked;
      });
    }
    if (ev.target.type === "checkbox" && ev.target.form) bulkSync(ev.target.form);
  });
  doc.querySelectorAll(".bulkbar").forEach(function (b) {
    bulkSync(b.closest("form"));
  });

  /* ---------------------------------------------------------------- counters */
  function count(el) {
    var max = parseInt(el.getAttribute("data-count-max"), 10) || 0;
    var out = doc.querySelector('[data-counter-for="' + el.id + '"]');
    if (!out) return;
    var n = (el.value || "").length;
    out.textContent = n + " / " + max;
    out.setAttribute("data-state", n > max ? "over" : n > max * 0.9 ? "warn" : "ok");
  }
  doc.querySelectorAll("[data-count-max]").forEach(count);
  doc.addEventListener("input", function (ev) {
    if (ev.target.hasAttribute && ev.target.hasAttribute("data-count-max")) count(ev.target);
  });

  /* ---------------------------------------------------------------- SERP preview */
  function serp(box) {
    var form = box.closest("form") || doc;
    var pick = function (attr) {
      var el = form.querySelector("[" + attr + "]");
      return el ? (el.value || el.getAttribute("data-fallback") || "").trim() : "";
    };
    var title = pick("data-serp-title") || box.getAttribute("data-default-title") || "";
    var desc = pick("data-serp-desc") || box.getAttribute("data-default-desc") || "";
    var url = pick("data-serp-url") || box.getAttribute("data-default-url") || "";
    var t = box.querySelector(".serp__title");
    var d = box.querySelector(".serp__desc");
    var u = box.querySelector(".serp__url");
    if (t) t.textContent = title.length > 60 ? title.slice(0, 59) + "…" : title;
    if (d) d.textContent = desc.length > 160 ? desc.slice(0, 159) + "…" : desc;
    if (u && url) u.textContent = url;
  }
  doc.querySelectorAll("[data-serp]").forEach(serp);
  doc.addEventListener("input", function (ev) {
    var form = closest(ev.target, "form");
    if (form) form.querySelectorAll("[data-serp]").forEach(serp);
  });

  /* ---------------------------------------------------------------- canned responses */
  doc.addEventListener("change", function (ev) {
    var sel = ev.target;
    if (!sel.hasAttribute || !sel.hasAttribute("data-insert-into")) return;
    var opt = sel.options[sel.selectedIndex];
    var target = doc.querySelector(sel.getAttribute("data-insert-into"));
    if (opt && target && opt.getAttribute("data-body")) {
      var body = opt.getAttribute("data-body");
      target.value = target.value ? target.value.replace(/\s*$/, "") + "\n\n" + body : body;
      target.focus();
      target.dispatchEvent(new Event("input", { bubbles: true }));
    }
    sel.selectedIndex = 0;
  });

  /* ---------------------------------------------------------------- brand preview */
  function brand(input) {
    var box = doc.querySelector("[data-brand-preview]");
    var v = input.getAttribute("data-brand-var");
    if (box && v && /^#[0-9a-fA-F]{6}$/.test(input.value)) box.style.setProperty(v, input.value);
  }
  doc.querySelectorAll("[data-brand-var]").forEach(brand);
  doc.addEventListener("input", function (ev) {
    if (ev.target.hasAttribute && ev.target.hasAttribute("data-brand-var")) brand(ev.target);
  });

  /* ---------------------------------------------------------------- conditional fields */
  function toggles(scope) {
    (scope || doc).querySelectorAll("[data-show-when]").forEach(function (el) {
      var spec = el.getAttribute("data-show-when").split("=");
      var form = el.closest("form") || doc;
      var ctl = form.querySelector('[name="' + spec[0] + '"]');
      if (!ctl) return;
      var values = spec[1].split("|");
      el.hidden = values.indexOf(ctl.value) === -1;
    });
  }
  toggles();
  doc.addEventListener("change", function () {
    toggles();
  });
  doc.body.addEventListener("htmx:afterSwap", function (ev) {
    toggles(ev.target);
    if (ev.target.querySelectorAll) ev.target.querySelectorAll("[data-count-max]").forEach(count);
  });

  /* ---------------------------------------------------------------- list loading state
     A filter submit, sort header, pagination or status chip on a list page
     navigates; until the next page paints, the table shows a skeleton
     (aria-busy on .table-wrap, styled in admin.css). Restored from the
     back/forward cache, the flag is cleared again. */
  function listBusy() {
    doc.querySelectorAll(".ax-content .table-wrap").forEach(function (w) {
      w.setAttribute("aria-busy", "true");
    });
  }
  doc.addEventListener("submit", function (ev) {
    var f = ev.target;
    if (f && f.matches && f.matches("form.filters") && !ev.defaultPrevented) listBusy();
  });
  doc.addEventListener("click", function (ev) {
    if (ev.defaultPrevented || ev.button !== 0 || ev.metaKey || ev.ctrlKey || ev.shiftKey) return;
    var a = closest(ev.target, ".pagination a, .table thead a, .chip[href], .ax-chip, .ax-chips__clear");
    if (a && a.getAttribute("href")) listBusy();
  });
  window.addEventListener("pageshow", function () {
    doc.querySelectorAll('.table-wrap[aria-busy="true"]').forEach(function (w) {
      w.removeAttribute("aria-busy");
    });
  });
})();
