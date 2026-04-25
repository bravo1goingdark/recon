/* docs/Docs.html — collapsible sections + sidebar/anchor sync.
 *
 * Each major section is a <details class="section" id="X">. This script:
 *  1. Opens the section matching the URL hash on page load (deep links).
 *  2. Opens a section when its sidebar/anchor link is clicked.
 *  3. Updates the URL hash + sidebar highlight as sections open/close.
 *  4. Wires the "Expand all / Collapse all" toggle.
 *
 * Graceful degradation: with JS off, every <details> still opens/closes
 * via the browser's native control. This script just adds nicer flow.
 */
(function () {
  function $$(sel, root) {
    return Array.from((root || document).querySelectorAll(sel));
  }

  function openById(id) {
    if (!id) return null;
    var el = document.getElementById(id);
    if (!el) return null;
    var details =
      el.tagName === "DETAILS" ? el : el.closest("details.section");
    if (details) details.open = true;
    return el;
  }

  function scrollIntoViewSmooth(el) {
    if (!el) return;
    requestAnimationFrame(function () {
      el.scrollIntoView({ behavior: "smooth", block: "start" });
    });
  }

  // ── Sidebar highlight ──────────────────────────────────────────────
  // Highlights the sidebar entry whose target is currently open. If
  // multiple are open (after Expand all), highlights the one most
  // recently opened by the user — tracked via a click handler below.
  var lastInteractedId = null;

  function refreshSidebar() {
    var openSections = $$("details.section[open]");
    var activeId =
      lastInteractedId &&
      document.getElementById(lastInteractedId) &&
      document.getElementById(lastInteractedId).open
        ? lastInteractedId
        : openSections.length
        ? openSections[0].id
        : null;
    $$("aside.side a").forEach(function (a) {
      a.classList.toggle(
        "on",
        activeId && a.getAttribute("href") === "#" + activeId,
      );
    });
  }

  // ── Anchor / hash handling ─────────────────────────────────────────
  document.addEventListener("click", function (e) {
    var link = e.target.closest("a[href^='#']");
    if (!link) return;
    var id = link.getAttribute("href").slice(1);
    if (!id) return;
    var el = openById(id);
    if (!el) return;
    lastInteractedId = el.id || lastInteractedId;
    // Let the browser handle the smooth scroll via :target; ensure the
    // section had time to expand before scroll lands.
    scrollIntoViewSmooth(el);
    refreshSidebar();
  });

  window.addEventListener("hashchange", function () {
    var id = location.hash.slice(1);
    var el = openById(id);
    if (el) {
      lastInteractedId = el.id || lastInteractedId;
      scrollIntoViewSmooth(el);
      refreshSidebar();
    }
  });

  // ── Toggle event keeps sidebar in sync as users open/close manually ─
  document.addEventListener(
    "toggle",
    function (e) {
      var t = e.target;
      if (!(t instanceof HTMLDetailsElement)) return;
      if (!t.classList.contains("section")) return;
      if (t.open) lastInteractedId = t.id;
      refreshSidebar();
    },
    true,
  );

  // ── Expand / Collapse all ──────────────────────────────────────────
  var btn = document.getElementById("expand-all");
  if (btn) {
    function syncBtn() {
      var sections = $$("details.section");
      var anyClosed = sections.some(function (d) {
        return !d.open;
      });
      btn.textContent = anyClosed ? "Expand all" : "Collapse all";
    }
    btn.addEventListener("click", function () {
      var sections = $$("details.section");
      var anyClosed = sections.some(function (d) {
        return !d.open;
      });
      sections.forEach(function (d) {
        d.open = anyClosed;
      });
      syncBtn();
      refreshSidebar();
    });
    document.addEventListener("toggle", syncBtn, true);
    syncBtn();
  }

  // ── Initial state ──────────────────────────────────────────────────
  // If the URL has a hash, open that section. Otherwise open the first
  // (Quickstart) so the page isn't a wall of closed accordions on first
  // visit.
  if (location.hash) {
    var initial = openById(location.hash.slice(1));
    if (initial) {
      lastInteractedId = initial.id || initial.closest("details.section")?.id;
      scrollIntoViewSmooth(initial);
    }
  } else {
    var first = document.querySelector("details.section");
    if (first) {
      first.open = true;
      lastInteractedId = first.id;
    }
  }
  refreshSidebar();
})();
