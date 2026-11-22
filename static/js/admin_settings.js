/* HostingOS admin settings behaviour (strict CSP: no inline scripts).

   [data-color-sync="id"]  <input type="color"> kept in sync with the text
                           input #id (both directions). Typing a valid hex
                           code updates the picker and re-fires its `input`
                           event so the [data-brand-preview] box (admin.js)
                           follows as well. */
(function () {
  "use strict";
  var doc = document;
  var HEX6 = /^#[0-9a-fA-F]{6}$/;
  var HEX3 = /^#[0-9a-fA-F]{3}$/;

  function expand(v) {
    v = (v || "").trim();
    if (HEX3.test(v)) {
      return "#" + v[1] + v[1] + v[2] + v[2] + v[3] + v[3];
    }
    return v;
  }

  function pickers() {
    return Array.prototype.slice.call(doc.querySelectorAll("[data-color-sync]"));
  }

  doc.addEventListener("input", function (ev) {
    var el = ev.target;
    if (!el || !el.getAttribute) return;
    var target = el.getAttribute("data-color-sync");
    if (target) {
      var text = doc.getElementById(target);
      if (text && text.value.toLowerCase() !== el.value.toLowerCase()) text.value = el.value;
      return;
    }
    if (!el.id) return;
    pickers().forEach(function (picker) {
      if (picker.getAttribute("data-color-sync") !== el.id) return;
      var v = expand(el.value);
      if (HEX6.test(v) && picker.value.toLowerCase() !== v.toLowerCase()) {
        picker.value = v.toLowerCase();
        picker.dispatchEvent(new Event("input", { bubbles: true }));
      }
    });
  });
})();
