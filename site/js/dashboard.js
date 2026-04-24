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
    renderStats(user, keys);
    renderQuickstart(keys);
  }

  var billingResp = await authFetch("/v1/billing/portal");
  if (billingResp.ok) {
    var billing = await billingResp.json();
    renderBilling(billing);
  }
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

function renderStats(user, keys) {
  var tierEl = document.getElementById("stat-tier");
  var keysEl = document.getElementById("stat-keys");
  if (tierEl) tierEl.innerHTML = escapeHtml(user.tier);
  var active = keys.filter(function (k) { return !k.revoked; }).length;
  if (keysEl) keysEl.innerHTML = active + ' <span class="unit">active</span>';
}

function renderQuickstart(keys) {
  var el = document.getElementById("quickstart");
  var keyEl = document.getElementById("qs-key");
  if (!el) return;
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

function renderBilling(billing) {
  var el = document.getElementById("billing");
  if (!el) return;

  var tc = billing.tier_config;
  var limitsHtml =
    tc.limits.max_repos + " repos" +
    " · " + tc.limits.max_files.toLocaleString() + " files" +
    " · " + (tc.limits.max_loc / 1000).toLocaleString() + "K LOC";

  // Pick the status line + right-hand CTA based on subscription state.
  // Four states the dashboard has to reflect:
  //   1. Free + no sub  → show Subscribe CTA
  //   2. Paid + active + not cancelled → show "Next billing: DATE" + Cancel
  //   3. Paid + cancel_at_period_end → show "Ends DATE" + Resubscribe
  //   4. Paid + cancelled/halted/expired → show "Access until DATE" + Resubscribe
  var statusLine = "";
  var actionHtml = "";
  var sub = billing.subscription;

  if (sub && sub.tier !== "Free") {
    var endDate = sub.current_period_end
      ? formatDate(sub.current_period_end)
      : "unknown";

    if (sub.status === "active" && !sub.cancel_at_period_end) {
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px">Next billing: ' +
        escapeHtml(endDate) +
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
    cancelBtn.addEventListener("click", cancelSubscription);
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

async function cancelSubscription() {
  if (!confirm(
    "Cancel your subscription?\n\n" +
    "You'll keep full access until the end of the current billing period. " +
    "After that, your account drops to the Free tier.\n\n" +
    "You can resubscribe any time.",
  )) {
    return;
  }

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

  // Refresh the dashboard to show the new "Cancelled — access until" state.
  loadDashboard();
}

async function generateKey() {
  var resp = await authFetch("/v1/dashboard/keys", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "Default" }),
  });

  if (!resp.ok) {
    alert("Failed to generate key");
    return;
  }

  var data = await resp.json();
  showKeyModal(data.key);
  loadDashboard();
}

async function revokeKey(id) {
  if (!confirm("Revoke this API key? This cannot be undone.")) return;
  var resp = await authFetch("/v1/dashboard/keys/" + encodeURIComponent(id), {
    method: "DELETE",
  });
  if (!resp.ok && resp.status !== 404) {
    // 404 can happen if the row was already gone — treat as success.
    alert("Failed to revoke key");
    return;
  }
  loadDashboard();
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

function wireControls() {
  var gen = document.getElementById("generate-key-btn");
  if (gen) gen.addEventListener("click", generateKey);

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

  // Event delegation: one listener for every Revoke button rendered
  // into #keys by renderKeys(), including ones added on subsequent
  // reloads after generateKey().
  var keysContainer = document.getElementById("keys");
  if (keysContainer) {
    keysContainer.addEventListener("click", function (e) {
      var btn = e.target.closest("[data-action='revoke-key']");
      if (!btn) return;
      var id = btn.getAttribute("data-key-id");
      if (id) revokeKey(id);
    });
  }
}

document.addEventListener("DOMContentLoaded", function () {
  wireControls();
  loadDashboard();
});
