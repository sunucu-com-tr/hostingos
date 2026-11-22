/* Runs synchronously in <head> (tiny): applies the saved theme before first
   paint and marks that JavaScript is available. The server renders the
   tenant's default mode as data-theme (customer pages); a visitor's saved
   choice wins unless the tenant locked the mode (data-mode-locked) or a
   staff member is previewing a default mode (data-theme-preview). */
(function () {
  var root = document.documentElement;
  root.classList.add("js");
  if (root.hasAttribute("data-mode-locked") || root.hasAttribute("data-theme-preview")) {
    return;
  }
  try {
    var saved = localStorage.getItem("hos-theme");
    if (saved === "light" || saved === "dark") {
      root.setAttribute("data-theme", saved);
    }
  } catch (e) {
    /* storage disabled: tenant default or the system preference */
  }
})();
