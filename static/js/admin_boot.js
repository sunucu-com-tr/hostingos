/* Admin shell, runs synchronously in <head> (tiny, external: strict CSP).
   Applies the remembered sidebar state (full / icon rail) and the keyboard
   hint flavour (⌘K on Apple devices, Ctrl K elsewhere) before the first
   paint, so the shell never jumps on load. admin.js owns the toggle. */
(function () {
  var root = document.documentElement;
  try {
    if (localStorage.getItem("hos-admin-rail") === "1") {
      root.setAttribute("data-rail", "");
    }
  } catch (e) {
    /* storage disabled: full sidebar */
  }
  if (/Mac|iPhone|iPad|iPod/.test(navigator.platform || navigator.userAgent || "")) {
    root.setAttribute("data-mac", "");
  }
})();
