/**
 * Dashboard page — uses authFetch() from auth.js for all API calls.
 */

async function loadDashboard() {
  var user = await checkAuth();
  if (!user) {
    window.location.href = "/login";
    return;
  }

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
          : '<button onclick="revokeKey(\'' + k.id + '\')" class="btn ghost sm danger">Revoke</button>') +
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

  el.innerHTML =
    '<div class="billing-row">' +
    '<div><strong>' + escapeHtml(billing.tier) + "</strong> " +
    '<span class="dim" style="font-family:var(--mono);font-size:11px">' + escapeHtml(tc.price_display) + "</span></div>" +
    '<div class="billing-limits">' + limitsHtml + "</div>" +
    (billing.tier === "Free"
      ? '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Upgrade →</a>'
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
      setTimeout(function () { btn.textContent = "Copy to clipboard"; }, 2000);
    }
  }
}

document.addEventListener("DOMContentLoaded", loadDashboard);
