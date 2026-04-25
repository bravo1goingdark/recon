/**
 * Dashboard page — uses authFetch() from auth.js for all API calls.
 *
 * Wiring note: we cannot use inline `onclick=` attributes under the
 * strict CSP (`script-src 'self'` with no unsafe-inline). Every
 * interactive element is bound once at DOMContentLoaded via
 * `addEventListener`, with the dynamic revoke buttons handled by a
 * single event-delegation listener on the #keys container.
 */

// Set by loadDashboard; read by the delete-account confirm-modal logic
// (needs the user's github_username to validate the typed confirmation).
var currentUser = null;

async function loadDashboard() {
  var user = await checkAuth();
  if (!user) {
    window.location.href = "/login";
    return;
  }
  // Promote to module-scope so openDeleteAccountModal / the confirm-input
  // validator can read the user's github_username.
  currentUser = user;

  renderProfile(user);

  var keysResp = await authFetch("/v1/dashboard/keys");
  var keys = [];
  if (keysResp.ok) {
    var data = await keysResp.json();
    keys = data.keys || [];
    renderKeys(keys);
    renderQuickstart(keys);
    toggleKeyButtons(keys);
  }

  var billingResp = await authFetch("/v1/billing/portal");
  if (billingResp.ok) {
    var billing = await billingResp.json();
    renderBilling(billing);
  }

  await loadRepos();

  // Razorpay redirects here with `?just_paid=1` after a successful
  // subscription auth. The webhook usually lands within a couple of
  // seconds but there's a window where the dashboard would still show
  // "Free". Poll /v1/billing/portal until the tier flips, then refresh
  // the page so the badge + sidebar reflect the upgrade.
  if (window.location.search.indexOf("just_paid=1") !== -1) {
    pollForTierUpgrade(user.tier);
  }
}

/**
 * After a successful Razorpay subscription auth, poll /v1/billing/portal
 * until the user's tier moves off `oldTier`. The webhook usually fires
 * within ~2s; we poll for up to 30s before giving up. Stops at the first
 * tier change OR when the user navigates away. Drops the `just_paid`
 * query param via history.replaceState so a refresh doesn't re-trigger.
 */
async function pollForTierUpgrade(oldTier) {
  var attempts = 0;
  var MAX_ATTEMPTS = 15; // 15 × 2s = 30s
  showUpgradePendingBanner();

  // Strip the just_paid flag so a manual refresh during/after the poll
  // doesn't re-arm this loop.
  try {
    var url = new URL(window.location.href);
    url.searchParams.delete("just_paid");
    window.history.replaceState({}, "", url.toString());
  } catch (_) {}

  var iv = setInterval(async function () {
    attempts++;
    var resp = await authFetch("/v1/billing/portal");
    if (resp.ok) {
      var billing = await resp.json();
      if (billing.tier !== oldTier) {
        clearInterval(iv);
        hideUpgradePendingBanner();
        // Full reload so the user sees the new tier badge + limits
        // without us having to surgically re-render every panel.
        window.location.reload();
        return;
      }
    }
    if (attempts >= MAX_ATTEMPTS) {
      clearInterval(iv);
      hideUpgradePendingBanner(
        "Payment received — tier upgrade is taking longer than usual. Refresh in a minute, or contact support if it doesn't appear.",
      );
    }
  }, 2000);
}

function showUpgradePendingBanner() {
  var existing = document.getElementById("upgrade-pending-banner");
  if (existing) return;
  var b = document.createElement("div");
  b.id = "upgrade-pending-banner";
  b.style.cssText =
    "position:fixed;top:0;left:0;right:0;background:var(--clay);color:var(--paper);padding:10px 16px;text-align:center;font-size:13px;z-index:1000;font-family:var(--mono)";
  b.textContent =
    "Payment received — confirming your upgrade…";
  document.body.appendChild(b);
}

function hideUpgradePendingBanner(failureMessage) {
  var b = document.getElementById("upgrade-pending-banner");
  if (!b) return;
  if (failureMessage) {
    b.textContent = failureMessage;
    b.style.background = "var(--ink)";
    setTimeout(function () {
      if (b.parentNode) b.parentNode.removeChild(b);
    }, 8000);
  } else {
    b.parentNode.removeChild(b);
  }
}

/**
 * Fetch the user's registered-repo list (server-side max_repos
 * enforcement) and render it into the Repos tab. Pulled out as its own
 * function so the Remove handler can re-fetch after a delete.
 */
async function loadRepos() {
  var reposResp = await authFetch("/v1/dashboard/repos");
  if (!reposResp.ok) return;
  var data = await reposResp.json();
  renderRepos(data);
}

function renderProfile(user) {
  var el = document.getElementById("profile");
  if (!el) return;
  el.innerHTML =
    '<div style="display:flex;align-items:center;gap:14px">' +
    (user.avatar_url
      ? '<img src="' + escapeHtml(user.avatar_url) + '" width="40" height="40" style="border-radius:50%;border:2px solid var(--rule)">'
      : "") +
    "<div><h1 style=\"font-size:clamp(28px,4vw,40px);letter-spacing:-.03em\">" +
    escapeHtml(user.github_username) +
    '</h1></div><span class="tier-badge" style="margin-left:auto">' +
    escapeHtml(user.tier) +
    "</span></div>";
}

// localStorage key for the dismiss-quickstart flag. Once set, the
// "Get started" panel stays hidden across reloads so returning users
// aren't re-onboarded every time they open the dashboard.
var QUICKSTART_DISMISSED_KEY = "recon.quickstart.dismissed";

function isQuickstartDismissed() {
  try {
    return localStorage.getItem(QUICKSTART_DISMISSED_KEY) === "1";
  } catch (_) {
    return false;
  }
}

function dismissQuickstart() {
  try { localStorage.setItem(QUICKSTART_DISMISSED_KEY, "1"); } catch (_) {}
  var el = document.getElementById("quickstart");
  if (el) el.style.display = "none";
}

function renderQuickstart(keys) {
  var el = document.getElementById("quickstart");
  var keyEl = document.getElementById("qs-key");
  if (!el) return;
  if (isQuickstartDismissed()) {
    el.style.display = "none";
    return;
  }
  var active = keys.filter(function (k) { return !k.revoked; });
  if (active.length > 0 && keyEl) {
    // Show prefix with ellipsis — full key was shown once at generation time
    keyEl.textContent = active[0].key_prefix + "...";
    el.style.display = "block";
  }
}

function renderKeys(keys) {
  var el = document.getElementById("keys");
  if (!el) return;

  if (!keys || keys.length === 0) {
    el.innerHTML = '<p class="empty">No API keys yet. Generate one below.</p>';
    return;
  }

  // NOTE: the Revoke button uses `data-key-id` + a single delegated
  // listener on #keys. Don't reintroduce `onclick=` — CSP blocks it.
  el.innerHTML = keys
    .map(function (k) {
      return (
        '<div class="key-row' + (k.revoked ? " revoked" : "") + '">' +
        "<code>" + escapeHtml(k.key_prefix) + "...</code>" +
        "<span>" + escapeHtml(k.name) + "</span>" +
        '<span class="tier-badge sm">' + escapeHtml(k.tier) + "</span>" +
        '<span class="dim">' + new Date(k.created_at).toLocaleDateString() + "</span>" +
        (k.revoked
          ? '<span class="dim">revoked</span>'
          : '<button class="btn ghost sm danger" data-action="revoke-key" data-key-id="' + escapeHtml(k.id) + '">Revoke</button>') +
        "</div>"
      );
    })
    .join("");
}

/**
 * Render the user's registered-repo list (v0.2.0+).
 *
 * Each row shows a truncated fingerprint, first/last seen, and a
 * Remove button. Rows are session-tracked server-side via
 * /v1/dashboard/repos so the dashboard and the CLI see the same
 * authoritative state.
 */
function renderRepos(payload) {
  var el = document.getElementById("repos");
  if (!el) return;

  var repos = (payload && payload.repos) || [];
  var limit = (payload && payload.limit) || 1;
  var tier = (payload && payload.tier) || "Free";

  var header =
    '<div class="repos-header">' +
    "<span><b>" +
    repos.length +
    "</b> / " +
    limit +
    " repos used" +
    "</span>" +
    '<span class="dim">' +
    escapeHtml(tier) +
    " plan" +
    "</span>" +
    "</div>";

  if (repos.length === 0) {
    el.innerHTML =
      header +
      '<p class="empty">No repos registered yet. Run <code>recon init --mcp &lt;ide&gt;</code> in a project to register one.</p>';
    return;
  }

  // Delegated Remove buttons via [data-action='remove-repo'] +
  // data-fingerprint. CSP blocks inline onclick, so we wire one
  // listener on #repos in wireControls.
  var rows = repos
    .map(function (r) {
      return (
        '<div class="repo-row">' +
        '<code class="repo-fp" title="' +
        escapeHtml(r.fingerprint) +
        '">' +
        escapeHtml(r.fingerprint.slice(0, 16)) +
        "…</code>" +
        '<span class="dim">first ' +
        formatDate(r.first_seen_at) +
        "</span>" +
        '<span class="dim">last ' +
        formatDate(r.last_seen_at) +
        "</span>" +
        '<button class="btn ghost sm" data-action="remove-repo" data-fingerprint="' +
        escapeHtml(r.fingerprint) +
        '">Remove</button>' +
        "</div>"
      );
    })
    .join("");

  el.innerHTML = header + rows;
}

/**
 * Remove a server-side repo slot from the dashboard. Re-fetches the
 * list on success so the count + tier badge stay accurate.
 */
async function removeRepo(fingerprint) {
  if (!fingerprint) return;
  if (!confirm("Remove this repo from your account? Re-running `recon init` from that project will register it again (if you're under your tier limit).")) {
    return;
  }
  var resp = await authFetch("/v1/dashboard/repos/" + encodeURIComponent(fingerprint), {
    method: "DELETE",
  });
  if (!resp.ok && resp.status !== 204) {
    var msg = "Failed to remove repo.";
    try {
      var body = await resp.json();
      if (body && body.error) msg = body.error;
    } catch {}
    alert(msg);
    return;
  }
  await loadRepos();
}

function renderBilling(billing) {
  var el = document.getElementById("billing");
  if (!el) return;

  var tc = billing.tier_config;
  var limitsHtml =
    tc.limits.max_repos + " repos" +
    " · " + tc.limits.max_files.toLocaleString() + " files" +
    " · " + (tc.limits.max_loc / 1000).toLocaleString() + "K LOC";

  // Pick the status line + right-hand CTA based on subscription state.
  // The states the dashboard has to reflect:
  //   1. Free + no sub                              → Subscribe CTA
  //   2. Subscribed + cancel_at_period_end OR
  //      status='cancelled'                          → "Access until DATE" + Resubscribe
  //   3. Subscribed + status='halted'                 → "Payment failed" + Resubscribe
  //   4. Subscribed + status in (active, authenticated, pending, created)
  //      AND cancel_at_period_end=0                   → status copy + Cancel button
  //
  // The Cancel button has to render for the in-between states too —
  // 'created' / 'authenticated' / 'pending' is the window between the user
  // clicking Subscribe and Razorpay confirming the first charge. The
  // backend's POST /v1/billing/cancel accepts all of these, so locking the
  // UI out of them stranded users with no self-serve cancel path.
  var statusLine = "";
  var actionHtml = "";
  var sub = billing.subscription;

  // States we treat as "subscription is live or about to become live" and
  // therefore eligible for a Cancel button. Mirrors the WHERE-status list
  // in /v1/billing/cancel and /v1/billing/subscribe's stack-prevention
  // guard so the three never disagree about what "active-ish" means.
  var ACTIVEISH = ["created", "authenticated", "active", "pending"];

  if (sub && sub.tier !== "Free") {
    var endDate = sub.current_period_end
      ? formatDate(sub.current_period_end)
      : "unknown";

    if (
      ACTIVEISH.indexOf(sub.status) !== -1 &&
      !sub.cancel_at_period_end
    ) {
      // status copy varies a bit so the user can tell whether their first
      // charge has landed yet. The Cancel CTA is the same in all cases.
      var statusText;
      if (sub.status === "active") {
        statusText = "Next billing: " + endDate;
      } else if (sub.status === "pending") {
        statusText = "Awaiting payment confirmation";
      } else {
        statusText = "Awaiting first charge from Razorpay";
      }
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px">' +
        escapeHtml(statusText) +
        "</div>";
      actionHtml =
        '<button id="cancel-sub-btn" class="btn ghost sm" style="margin-left:auto">Cancel subscription</button>';
    } else if (sub.cancel_at_period_end || sub.status === "cancelled") {
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px;color:var(--clay)">Cancelled — access until ' +
        escapeHtml(endDate) +
        "</div>";
      actionHtml =
        '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Resubscribe →</a>';
    } else if (sub.status === "halted") {
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px;color:var(--clay)">Payment failed — access until ' +
        escapeHtml(endDate) +
        ". Update card at Razorpay and resubscribe.</div>";
      actionHtml =
        '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Resubscribe →</a>';
    }
  } else if (billing.tier === "Free") {
    actionHtml =
      '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Subscribe →</a>';
  }

  el.innerHTML =
    '<div class="billing-row">' +
    '<div><strong>' + escapeHtml(billing.tier) + "</strong>" +
    statusLine +
    "</div>" +
    '<div class="billing-limits">' + limitsHtml + "</div>" +
    actionHtml +
    "</div>";

  // Wire the cancel button if it was rendered this pass.
  var cancelBtn = document.getElementById("cancel-sub-btn");
  if (cancelBtn) {
    // Opens the modal; the modal's confirm button actually runs the
    // cancel call (wired in wireControls).
    cancelBtn.addEventListener("click", openCancelSubModal);
  }
}

/**
 * Format a UTC ISO date or unix-second-ish value into a local YYYY-MM-DD
 * string. Worker returns ISO-8601 from D1; fall back gracefully for
 * anything else.
 */
function formatDate(value) {
  try {
    var d = new Date(value);
    if (isNaN(d.getTime())) return String(value);
    return d.toISOString().slice(0, 10);
  } catch (_) {
    return String(value);
  }
}

// ─── Cancel subscription ─────────────────────────────────────────────────
// openCancelSubModal pops the in-page modal; the confirm button (wired in
// wireControls) runs the actual cancel call. Browser confirm() was the
// previous UX — jarring, not themable, and shows the URL bar in some
// browsers which looks unprofessional.

function openCancelSubModal() {
  var modal = document.getElementById("cancel-sub-modal");
  if (modal) modal.style.display = "flex";
}

function closeCancelSubModal() {
  var modal = document.getElementById("cancel-sub-modal");
  if (modal) modal.style.display = "none";
}

async function cancelSubscription() {
  closeCancelSubModal();

  var btn = document.getElementById("cancel-sub-btn");
  if (btn) {
    btn.textContent = "Cancelling…";
    btn.disabled = true;
  }

  var resp = await authFetch("/v1/billing/cancel", { method: "POST" });
  if (!resp.ok) {
    var err = await resp.json().catch(function () {
      return { error: "Cancel failed (" + resp.status + ")" };
    });
    alert(err.error || "Cancel failed");
    if (btn) {
      btn.textContent = "Cancel subscription";
      btn.disabled = false;
    }
    return;
  }

  loadDashboard();
}

// ─── API key lifecycle ──────────────────────────────────────────────────
// One active key per account. The worker rejects a second POST /keys with
// 409 (see dashboardRoutes.post("/keys", ...) in worker/src/routes/dashboard.ts).
// Here we keep the UI's Generate / Rotate buttons in sync with key state
// so the user never clicks Generate to hit a 409.

// Holds the id of the key targeted by the revoke-key modal. Set when the
// user clicks a row's Revoke button; read by revokeKeyConfirmed.
// Null → modal wasn't opened via a row, so it's the "Rotate" path instead.
var pendingRevokeKeyId = null;
// True when the Rotate button was clicked: after revoke succeeds, we
// automatically call generate() so the user gets a replacement key.
var pendingRotate = false;

function toggleKeyButtons(keys) {
  var active = (keys || []).filter(function (k) { return !k.revoked; });
  var gen = document.getElementById("generate-key-btn");
  var rot = document.getElementById("rotate-key-btn");
  var hint = document.getElementById("key-hint");
  if (active.length === 0) {
    if (gen) gen.style.display = "";
    if (rot) rot.style.display = "none";
    if (hint) hint.style.display = "none";
  } else {
    if (gen) gen.style.display = "none";
    if (rot) rot.style.display = "";
    if (hint) hint.style.display = "";
  }
}

async function doGenerateKey() {
  var resp = await authFetch("/v1/dashboard/keys", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "Default" }),
  });

  if (!resp.ok) {
    var err = await resp.json().catch(function () {
      return { error: "Failed to generate key" };
    });
    alert(err.error || "Failed to generate key");
    return;
  }

  var data = await resp.json();
  showKeyModal(data.key);
  loadDashboard();
}

function openRevokeKeyModal(keyId, options) {
  pendingRevokeKeyId = keyId;
  pendingRotate = !!(options && options.rotate);
  var titleEl = document.getElementById("revoke-key-title");
  var bodyEl = document.getElementById("revoke-key-body");
  var confirmBtn = document.getElementById("revoke-key-confirm-btn");
  if (pendingRotate) {
    if (titleEl) titleEl.textContent = "Rotate API key?";
    if (bodyEl) bodyEl.textContent =
      "We'll revoke your current key and immediately issue a replacement. " +
      "Any running recon serve using the old key loses access within 15 minutes.";
    if (confirmBtn) confirmBtn.textContent = "Rotate now";
  } else {
    if (titleEl) titleEl.textContent = "Revoke API key?";
    if (bodyEl) bodyEl.textContent =
      "Any running recon serve using this key will lose access within 15 minutes, " +
      "and the key can't be used for new logins. This cannot be undone.";
    if (confirmBtn) confirmBtn.textContent = "Revoke";
  }
  var modal = document.getElementById("revoke-key-modal");
  if (modal) modal.style.display = "flex";
}

function closeRevokeKeyModal() {
  pendingRevokeKeyId = null;
  pendingRotate = false;
  var modal = document.getElementById("revoke-key-modal");
  if (modal) modal.style.display = "none";
}

async function revokeKeyConfirmed() {
  var id = pendingRevokeKeyId;
  var rotate = pendingRotate;
  closeRevokeKeyModal();
  if (!id) return;

  var resp = await authFetch("/v1/dashboard/keys/" + encodeURIComponent(id), {
    method: "DELETE",
  });
  if (!resp.ok && resp.status !== 404) {
    // 404 → row already gone; treat as success.
    alert("Failed to revoke key");
    return;
  }

  if (rotate) {
    // Generate the replacement immediately. If this fails (worker
    // transient), loadDashboard will render the zero-keys state and
    // the user can click Generate manually.
    await doGenerateKey();
    return;
  }
  loadDashboard();
}

// Called by the row-level Revoke button (event delegation in wireControls).
function openRevokeForRowKey(id) {
  openRevokeKeyModal(id, { rotate: false });
}

// Called by the "Rotate key" button below the keys list.
async function rotateKey() {
  // Need to find the (one) active key's id to pass into the modal.
  var resp = await authFetch("/v1/dashboard/keys");
  if (!resp.ok) {
    alert("Failed to load key state");
    return;
  }
  var data = await resp.json();
  var active = (data.keys || []).filter(function (k) { return !k.revoked; });
  if (active.length === 0) {
    // Edge case: somebody deleted the row via another tab before the
    // click. Just fall through to generating a fresh one.
    doGenerateKey();
    return;
  }
  openRevokeKeyModal(active[0].id, { rotate: true });
}

function showKeyModal(key) {
  var modal = document.getElementById("key-modal");
  var keyEl = document.getElementById("new-key-value");
  var loginCmdEl = document.getElementById("login-cmd");
  if (modal && keyEl) {
    keyEl.textContent = key;
    if (loginCmdEl) loginCmdEl.textContent = "recon login " + key;
    modal.style.display = "flex";
  }
}

function closeKeyModal() {
  var modal = document.getElementById("key-modal");
  if (modal) modal.style.display = "none";
}

function copyKey() {
  var keyEl = document.getElementById("new-key-value");
  if (keyEl) {
    navigator.clipboard.writeText(keyEl.textContent || "");
    var btn = document.getElementById("copy-btn");
    if (btn) {
      btn.textContent = "Copied!";
      setTimeout(function () { btn.textContent = "Copy key"; }, 2000);
    }
  }
}

function copyLoginCmd() {
  var el = document.getElementById("login-cmd");
  if (el) {
    navigator.clipboard.writeText(el.textContent || "");
    var btn = document.getElementById("copy-login-btn");
    if (btn) {
      btn.textContent = "Copied!";
      setTimeout(function () { btn.textContent = "Copy command"; }, 2000);
    }
  }
}

/**
 * Bind all interactive elements once on page load.
 * Static buttons get direct listeners; the dynamic revoke buttons
 * (generated by renderKeys into #keys innerHTML) use a single
 * delegated listener on the container.
 */
function openDeleteAccountModal() {
  if (!currentUser) return;
  var expectedEl = document.getElementById("delete-expected-username");
  if (expectedEl) expectedEl.textContent = currentUser.github_username;
  var input = document.getElementById("delete-confirm-input");
  var confirmBtn = document.getElementById("delete-confirm-btn");
  if (input) {
    input.value = "";
    input.focus();
  }
  if (confirmBtn) confirmBtn.disabled = true;
  var modal = document.getElementById("delete-account-modal");
  if (modal) modal.style.display = "flex";
}

function closeDeleteAccountModal() {
  var modal = document.getElementById("delete-account-modal");
  if (modal) modal.style.display = "none";
}

async function deleteAccount() {
  var btn = document.getElementById("delete-confirm-btn");
  if (btn) {
    btn.textContent = "Deleting…";
    btn.disabled = true;
  }

  var resp = await authFetch("/v1/dashboard/account", { method: "DELETE" });
  if (!resp.ok) {
    var err = await resp.json().catch(function () {
      return { error: "Delete failed (" + resp.status + ")" };
    });
    alert(err.error || "Delete failed");
    if (btn) {
      btn.textContent = "Delete permanently";
      btn.disabled = false;
    }
    return;
  }

  // Session is gone server-side — clear any client state and redirect
  // to home. Logout helper from auth.js does the local cleanup.
  if (typeof logout === "function") {
    logout();
  } else {
    window.location.href = "/";
  }
}

// ─── Tabs ────────────────────────────────────────────────────────────────
// Three round-icon tabs at the top of the dashboard (Keys / Billing /
// Danger). One-panel-visible-at-a-time so the dashboard fits one screen
// instead of the old long vertical stack. Uses native `hidden` attribute
// on the panels; the active button gets a clay border + `.active` class.

function activateTab(name) {
  document.querySelectorAll(".tab-icon").forEach(function (btn) {
    var isActive = btn.getAttribute("data-tab") === name;
    btn.classList.toggle("active", isActive);
    btn.setAttribute("aria-selected", isActive ? "true" : "false");
  });
  document.querySelectorAll(".tab-panel").forEach(function (panel) {
    panel.hidden = panel.id !== "panel-" + name;
  });
}

function wireTabs() {
  document.querySelectorAll(".tab-icon").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var name = btn.getAttribute("data-tab");
      if (name) activateTab(name);
    });
  });
}

function wireControls() {
  wireTabs();

  // API key buttons. Generate is visible when the user has zero active
  // keys; Rotate replaces it when a key exists. toggleKeyButtons (called
  // from loadDashboard) manages the display: toggle.
  var gen = document.getElementById("generate-key-btn");
  if (gen) gen.addEventListener("click", doGenerateKey);

  var rot = document.getElementById("rotate-key-btn");
  if (rot) rot.addEventListener("click", rotateKey);

  // Revoke-key modal
  var revConfirm = document.getElementById("revoke-key-confirm-btn");
  if (revConfirm) revConfirm.addEventListener("click", revokeKeyConfirmed);
  var revCancel = document.getElementById("revoke-key-cancel-btn");
  if (revCancel) revCancel.addEventListener("click", closeRevokeKeyModal);

  // Cancel-subscription modal
  var cancelConfirm = document.getElementById("cancel-sub-confirm-btn");
  if (cancelConfirm) cancelConfirm.addEventListener("click", cancelSubscription);
  var cancelCancel = document.getElementById("cancel-sub-cancel-btn");
  if (cancelCancel) cancelCancel.addEventListener("click", closeCancelSubModal);

  // Delete-account modal
  var delBtn = document.getElementById("delete-account-btn");
  if (delBtn) delBtn.addEventListener("click", openDeleteAccountModal);

  var delCancel = document.getElementById("delete-cancel-btn");
  if (delCancel) delCancel.addEventListener("click", closeDeleteAccountModal);

  var delConfirm = document.getElementById("delete-confirm-btn");
  if (delConfirm) delConfirm.addEventListener("click", deleteAccount);

  // Only enable the confirm button once the user has typed their exact
  // username. Prevents accidental enter-to-submit nukes.
  var delInput = document.getElementById("delete-confirm-input");
  if (delInput) {
    delInput.addEventListener("input", function () {
      if (!delConfirm) return;
      delConfirm.disabled =
        !currentUser ||
        delInput.value.trim() !== currentUser.github_username;
    });
  }

  var logoutLink = document.getElementById("logout-link");
  if (logoutLink) {
    logoutLink.addEventListener("click", function (e) {
      e.preventDefault();
      // `logout` is defined in auth.js — shared across pages.
      if (typeof logout === "function") logout();
    });
  }

  var closeBtn = document.getElementById("close-key-modal-btn");
  if (closeBtn) closeBtn.addEventListener("click", closeKeyModal);

  var copyBtn = document.getElementById("copy-btn");
  if (copyBtn) copyBtn.addEventListener("click", copyKey);

  var copyLoginBtn = document.getElementById("copy-login-btn");
  if (copyLoginBtn) copyLoginBtn.addEventListener("click", copyLoginCmd);

  // Dismiss "Get started" panel. Persists via localStorage so the panel
  // doesn't reappear on every reload after the user has onboarded.
  var qsClose = document.getElementById("quickstart-close");
  if (qsClose) qsClose.addEventListener("click", dismissQuickstart);

  // Event delegation: one listener for every Revoke button rendered
  // into #keys by renderKeys(). Opens the revoke-key modal instead of
  // the old browser confirm() dialog.
  var keysContainer = document.getElementById("keys");
  if (keysContainer) {
    keysContainer.addEventListener("click", function (e) {
      var btn = e.target.closest("[data-action='revoke-key']");
      if (!btn) return;
      var id = btn.getAttribute("data-key-id");
      if (id) openRevokeForRowKey(id);
    });
  }

  // Same pattern for the Repos tab: one listener for every Remove
  // button rendered by renderRepos().
  var reposContainer = document.getElementById("repos");
  if (reposContainer) {
    reposContainer.addEventListener("click", function (e) {
      var btn = e.target.closest("[data-action='remove-repo']");
      if (!btn) return;
      var fp = btn.getAttribute("data-fingerprint");
      if (fp) removeRepo(fp);
    });
  }
}

document.addEventListener("DOMContentLoaded", function () {
  wireControls();
  loadDashboard();
});
