/* Optional. The site works without this: it only adds the theme toggle. */
(function () {
  var root = document.documentElement;
  var KEY = "ch-theme";
  function stored() {
    try { var t = localStorage.getItem(KEY); return t === "light" || t === "dark" ? t : null; } catch (e) { return null; }
  }
  function effective() {
    var t = root.getAttribute("data-theme");
    if (t) return t;
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  function syncPictures(theme) {
    var s = document.querySelectorAll("source[data-scheme]");
    for (var i = 0; i < s.length; i++) {
      s[i].media = s[i].getAttribute("data-scheme") === theme ? "all" : "not all";
    }
  }
  var saved = stored();
  if (saved) root.setAttribute("data-theme", saved);
  document.addEventListener("DOMContentLoaded", function () {
    if (saved) syncPictures(saved);
    var btn = document.getElementById("theme-toggle");
    if (!btn) return;
    function label() {
      btn.textContent = effective() === "dark" ? "Light mode" : "Dark mode";
    }
    btn.hidden = false;
    label();
    btn.addEventListener("click", function () {
      var next = effective() === "dark" ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try { localStorage.setItem(KEY, next); } catch (e) {}
      syncPictures(next);
      label();
    });
  });
})();
