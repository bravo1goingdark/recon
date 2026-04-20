/**
 * Dashboard page logic — API keys management, billing status.
 */

const API = "https://api.recon.dev";

async function loadDashboard() {
  // Check auth
  const meResp = await fetch(`${API}/v1/auth/me`, { credentials: "include" });
  if (!meResp.ok) {
    window.location.href = "/login";
    return;
  }
  const user = await meResp.json();
  renderProfile(user);

  // Load API keys
  const keysResp = await fetch(`${API}/v1/dashboard/keys`, {
    credentials: "include",
  });
  if (keysResp.ok) {
    const data = await keysResp.json();
    renderKeys(data.keys);
  }

  // Load billing
  const billingResp = await fetch(`${API}/v1/billing/portal`, {
    credentials: "include",
  });
  if (billingResp.ok) {
    const billing = await billingResp.json();
    renderBilling(billing);
  }
}

function renderProfile(user) {
  const el = document.getElementById("profile");
  if (!el) return;
  el.innerHTML = `
    <div class="profile-card">
      ${user.avatar_url ? `<img src="${user.avatar_url}" alt="" width="48" height="48" class="avatar">` : ""}
      <div>
        <strong>${escapeHtml(user.github_username)}</strong>
        <span class="tier-badge">${escapeHtml(user.tier)}</span>
      </div>
    </div>`;
}

function renderKeys(keys) {
  const el = document.getElementById("keys");
  if (!el) return;

  if (keys.length === 0) {
    el.innerHTML = `<p class="dim">No API keys yet.</p>`;
  } else {
    el.innerHTML = keys
      .map(
        (k) => `
      <div class="key-row ${k.revoked ? "revoked" : ""}">
        <code>${escapeHtml(k.key_prefix)}...</code>
        <span class="dim">${escapeHtml(k.name)}</span>
        <span class="tier-badge sm">${escapeHtml(k.tier)}</span>
        <span class="dim">${new Date(k.created_at).toLocaleDateString()}</span>
        ${k.revoked ? '<span class="dim">revoked</span>' : `<button onclick="revokeKey('${k.id}')" class="btn ghost sm">Revoke</button>`}
      </div>`,
      )
      .join("");
  }
}

function renderBilling(billing) {
  const el = document.getElementById("billing");
  if (!el) return;

  const tc = billing.tier_config;
  el.innerHTML = `
    <div class="billing-card">
      <strong>${escapeHtml(billing.tier)}</strong>
      <span class="dim">${escapeHtml(tc.price_display)}</span>
      <div class="limits">
        <span>${tc.limits.max_repos} repos</span>
        <span>${tc.limits.max_files.toLocaleString()} files</span>
        <span>${(tc.limits.max_loc / 1000).toLocaleString()}K LOC</span>
      </div>
      ${billing.tier === "Free" ? '<a href="/pricing" class="btn primary sm">Upgrade</a>' : ""}
    </div>`;
}

async function generateKey() {
  const resp = await fetch(`${API}/v1/dashboard/keys`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "Default" }),
  });

  if (!resp.ok) {
    alert("Failed to generate key");
    return;
  }

  const data = await resp.json();
  showKeyModal(data.key);
  loadDashboard();
}

async function revokeKey(id) {
  if (!confirm("Revoke this API key? This cannot be undone.")) return;

  await fetch(`${API}/v1/dashboard/keys/${id}`, {
    method: "DELETE",
    credentials: "include",
  });
  loadDashboard();
}

async function logout() {
  await fetch(`${API}/v1/auth/logout`, {
    method: "POST",
    credentials: "include",
  });
  window.location.href = "/";
}

function showKeyModal(key) {
  const modal = document.getElementById("key-modal");
  const keyEl = document.getElementById("new-key-value");
  if (modal && keyEl) {
    keyEl.textContent = key;
    modal.style.display = "flex";
  }
}

function closeKeyModal() {
  const modal = document.getElementById("key-modal");
  if (modal) modal.style.display = "none";
}

function copyKey() {
  const keyEl = document.getElementById("new-key-value");
  if (keyEl) {
    navigator.clipboard.writeText(keyEl.textContent || "");
    const btn = document.getElementById("copy-btn");
    if (btn) {
      btn.textContent = "Copied!";
      setTimeout(() => (btn.textContent = "Copy"), 2000);
    }
  }
}

function escapeHtml(s) {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

document.addEventListener("DOMContentLoaded", loadDashboard);
