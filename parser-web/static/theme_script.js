(function () {
  var theme = null;
  try {
    theme = localStorage.getItem("theme");
  } catch (_) {}
  document.documentElement.setAttribute("data-bs-theme", theme || "light");
})();
