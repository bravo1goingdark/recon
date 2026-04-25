/* Click-to-copy for command blocks across the marketing site.
 *
 * External file (not inline) because the deployed CSP is
 * `script-src 'self' https://checkout.razorpay.com` — inline scripts
 * are silently blocked.
 *
 * Conventions on the page:
 *   - Every <pre> on /index.html and /Docs.html gets a top-right "Copy"
 *     button via this script. innerText is what lands on the clipboard,
 *     so inline <span class="c"># comments</span> come along — that's
 *     fine in a shell (comments are no-ops) and matches what the user
 *     visually expects to copy.
 *   - Any element marked with `data-copy` (e.g. the IDE matrix flag
 *     buttons) becomes itself the click target. No injected button —
 *     the whole control copies on click.
 *
 * Visual feedback: the button label flips to "Copied" for 1.6 s, then
 * reverts. Failures (e.g. `clipboard` API unavailable in older Safari
 * private mode) flip to "Failed" without throwing.
 */
(function () {
  if (!navigator.clipboard) return;

  function flash(el, msg, restore, ms) {
    var prev = el.textContent;
    el.textContent = msg;
    el.classList.add("copied");
    el.setAttribute("aria-live", "polite");
    setTimeout(function () {
      el.textContent = restore != null ? restore : prev;
      el.classList.remove("copied");
    }, ms || 1600);
  }

  async function writeText(text) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch (_) {
      return false;
    }
  }

  // ── 1. <pre> blocks → inject top-right copy button ─────────────────
  document.querySelectorAll("pre").forEach(function (pre) {
    if (!pre.innerText.trim()) return;
    if (pre.dataset.copyBound === "1") return;
    pre.dataset.copyBound = "1";

    // Make the pre a positioning context for the absolute button.
    // Inline style so we don't need a class everywhere; harmless if
    // the existing CSS already sets position.
    var prevPos = getComputedStyle(pre).position;
    if (prevPos === "static") pre.style.position = "relative";

    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-btn";
    btn.textContent = "Copy";
    btn.setAttribute("aria-label", "Copy code block");
    // Block this button from selection — keeps Cmd+A on the pre clean.
    btn.style.userSelect = "none";

    btn.addEventListener("click", async function (e) {
      e.stopPropagation();
      // Strip the button's own text from what we copy by reading the
      // pre's innerText *without* the button — simpler than stripping
      // post-hoc: temporarily detach, copy, reattach.
      btn.style.display = "none";
      var text = pre.innerText;
      btn.style.display = "";
      var ok = await writeText(text);
      flash(btn, ok ? "Copied" : "Failed", "Copy");
    });

    pre.appendChild(btn);
  });

  // ── 2. data-copy elements → the element itself is the copy target ──
  document.querySelectorAll("[data-copy]").forEach(function (el) {
    if (el.dataset.copyBound === "1") return;
    el.dataset.copyBound = "1";
    var payload = el.getAttribute("data-copy") || el.innerText;
    el.style.cursor = "pointer";
    el.setAttribute("role", "button");
    el.setAttribute("tabindex", "0");
    el.setAttribute("title", "Click to copy");

    function go() {
      writeText(payload).then(function (ok) {
        flash(el, ok ? "Copied" : "Failed");
      });
    }
    el.addEventListener("click", go);
    el.addEventListener("keydown", function (e) {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        go();
      }
    });
  });
})();
