/* OS picker on /index.html#install — Linux / macOS / Windows.
 *
 * Lives as an external file (not inline <script>) because the site's
 * Content-Security-Policy is `script-src 'self' https://checkout.razorpay.com`
 * with no `unsafe-inline`. Inline scripts are silently blocked.
 */
(function () {
  var tabs = document.querySelectorAll(".os-tabs button[data-os]");
  var panes = document.querySelectorAll(".os-pane[data-os]");
  if (!tabs.length) return;

  // Best-effort platform detection on first load. Falls back to Linux.
  var ua = navigator.userAgent || "";
  var detected = /Win/i.test(ua)
    ? "windows"
    : /Mac/i.test(ua)
      ? "macos"
      : "linux";

  function activate(os) {
    tabs.forEach(function (t) {
      var on = t.dataset.os === os;
      t.classList.toggle("active", on);
      t.setAttribute("aria-selected", on ? "true" : "false");
    });
    panes.forEach(function (p) {
      p.classList.toggle("active", p.dataset.os === os);
    });
  }

  tabs.forEach(function (t) {
    t.addEventListener("click", function () {
      activate(t.dataset.os);
    });
  });

  activate(detected);
})();
