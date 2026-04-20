/**
 * Dashboard page — uses authFetch() from auth.js for all API calls.
 */

async function loadDashboard() {
  // auth.js already ran captureTokenFromFragment() before DOMContentLoaded
  var user = await checkAuth();
  if (!user) {
    window.location.href = "/login";
    return;
  }
  renderProfile(user);

  var keysResp = await authFetch("/v1/dashboard/keys");
  if (keysResp.ok) {
    var data = await keysResp.json();
    renderKeys(data.keys);
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
    '<div class="profile-card">' +
    (user.avatar_url
      ? '<img src="' + escapeHtml(user.avatar_url) + '" width="48" height="48" class="avatar">'
      : "") +
    '<div class="profile-info"><strong>' +
    escapeHtml(user.github_username) +
    '</strong><span class="tier-badge">' +
    escapeHtml(user.tier) +
    "</span></div></div>";
}

function renderKeys(keys) {
  var el = document.getElementById("keys");
  if (!el) return;

  if (!keys || keys.length === 0) {
    el.innerHTML =
      '<p class="dim" style="font-family:var(--mono);font-size:12px">No API keys yet. Generate one below.</p>';
    return;
  }

  el.innerHTML = keys
    .map(function (k) {
      return (
        '<div class="key-row' +
        (k.revoked ? " revoked" : "") +
        '"><code>' +
        escapeHtml(k.key_prefix) +
        "...</code><span class=\"dim\">" +
        escapeHtml(k.name) +
        '</span><span class="tier-badge sm">' +
        escapeHtml(k.tier) +
        '</span><span class="spacer"></span><span class="dim">' +
        new Date(k.created_at).toLocaleDateString() +
        "</span>" +
        (k.revoked
          ? '<span class="dim">revoked</span>'
          : '<button onclick="revokeKey(\'' +
            k.id +
            '\')" class="btn ghost sm danger">Revoke</button>') +
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
    tc.limits.max_repos +
    " repos · " +
    tc.limits.max_files.toLocaleString() +
    " files · " +
    (tc.limits.max_loc / 1000).toLocaleString() +
    "K LOC";

  el.innerHTML =
    '<div class="billing-card"><div><strong>' +
    escapeHtml(billing.tier) +
    '</strong> <span class="dim" style="font-family:var(--mono);font-size:12px">' +
    escapeHtml(tc.price_display) +
    '</span></div><div class="billing-limits">' +
    limitsHtml +
    "</div>" +
    (billing.tier === "Free"
      ? '<a href="/pricing" class="btn primary sm">Upgrade →</a>'
      : "") +
    "</div>";
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
  await authFetch("/v1/dashboard/keys/" + id, { method: "DELETE" });
  loadDashboard();
}

function showKeyModal(key) {
  var modal = document.getElementById("key-modal");
  var keyEl = document.getElementById("new-key-value");
  if (modal && keyEl) {
    keyEl.textContent = key;
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
      setTimeout(function () {
        btn.textContent = "Copy to clipboard";
      }, 2000);
    }
  }
}

document.addEventListener("DOMContentLoaded", loadDashboard);
