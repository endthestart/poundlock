// Applied synchronously before first paint so the chosen flavour never flashes.
// System (no stored key, or a stored "system") leaves the attribute unset so
// prefers-color-scheme in theme.css decides. Sign-out must not clear this key.
(function () {
  try {
    var value = localStorage.getItem("poundlock-theme");
    if (value && value !== "system") {
      document.documentElement.dataset.theme = value;
    }
  } catch (e) {
    // Storage unavailable (private mode, disabled cookies): fall back to
    // prefers-color-scheme via theme.css's :root:not([data-theme]) rules.
  }
})();
