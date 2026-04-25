/* docs/Docs.html — sidebar scroll-spy + group-state persistence.
 *
 * Sidebar groups are <details class="group" open> by default. Users may
 * collapse groups they don't need; we remember the open/closed state in
 * localStorage so it survives navigation.
 *
 * Active link highlight: an IntersectionObserver tracks which <h2 id>
 * is currently in the viewport's reading band and toggles `.on` on the
 * matching sidebar link. Clicking a sidebar link scrolls to that section
 * and (if its parent group is collapsed) opens the group so the user
 * can see they landed in the right place.
 */
(function () {
  function $$(sel, root) {
    return Array.from((root || document).querySelectorAll(sel));
  }

  // ── Persist sidebar group open/closed state ────────────────────────
  var STORAGE_KEY = "recon-docs-sidebar-groups-v1";

  function loadGroupState() {
    try {
      return JSON.parse(localStorage.getItem(STORAGE_KEY) || "{}");
    } catch {
      return {};
    }
  }

  function saveGroupState() {
    var state = {};
    $$("aside.side details.group").forEach(function (d, i) {
      // Use the first <h4> text as a stable-ish key; if missing, fall
      // back to index. A heading rename invalidates the cached state,
      // which is fine — old preference reverts to the HTML default.
      var h = d.querySelector("h4");
      var key = h ? h.textContent.trim() : "group-" + i;
      state[key] = d.open;
    });
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(state));
    } catch {}
  }

  (function applyStoredState() {
    var state = loadGroupState();
    $$("aside.side details.group").forEach(function (d, i) {
      var h = d.querySelector("h4");
      var key = h ? h.textContent.trim() : "group-" + i;
      if (Object.prototype.hasOwnProperty.call(state, key)) d.open = !!state[key];
    });
  })();

  $$("aside.side details.group").forEach(function (d) {
    d.addEventListener("toggle", saveGroupState);
  });

  // ── Click handler: ensure the parent group is open when a link is
  //    clicked, so the user sees their target's group expand if needed. ─
  $$("aside.side a[href^='#']").forEach(function (a) {
    a.addEventListener("click", function () {
      var group = a.closest("details.group");
      if (group && !group.open) {
        group.open = true;
        saveGroupState();
      }
    });
  });

  // ── Scroll-spy: highlight whichever <h2 id> is currently in the
  //    viewport's reading band, and auto-open its sidebar group if the
  //    user happened to scroll into a section whose group is collapsed. ─
  var sectionIds = $$("main h2[id]").map(function (h) {
    return h.id;
  });
  var links = sectionIds.map(function (id) {
    return {
      id: id,
      anchor: document.querySelector('aside.side a[href="#' + id + '"]'),
    };
  });

  function clearActive() {
    links.forEach(function (l) {
      if (l.anchor) l.anchor.classList.remove("on");
    });
  }

  if ("IntersectionObserver" in window && sectionIds.length) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (e) {
          if (!e.isIntersecting) return;
          clearActive();
          var match = links.find(function (l) {
            return l.id === e.target.id;
          });
          if (!match || !match.anchor) return;
          match.anchor.classList.add("on");
          // If user scrolled into a section whose sidebar group is
          // collapsed, expand it so the highlight is visible.
          var group = match.anchor.closest("details.group");
          if (group && !group.open) {
            group.open = true;
            saveGroupState();
          }
        });
      },
      { rootMargin: "-96px 0px -70% 0px", threshold: 0 },
    );
    sectionIds.forEach(function (id) {
      var el = document.getElementById(id);
      if (el) io.observe(el);
    });
  }
})();
