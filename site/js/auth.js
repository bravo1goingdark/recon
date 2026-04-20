/**
 * Shared auth state helper — loaded on every page.
 * Checks session via /v1/auth/me and updates the nav.
 */

const API = "/api";

async function checkAuth() {
  try {
    const resp = await fetch(`/api/v1/auth/me`, { credentials: "include" });
    if (resp.ok) return await resp.json();
  } catch {
    // Server unreachable — not authenticated
  }
  return null;
}

async function updateNav() {
  const el = document.getElementById("nav-auth");
  if (!el) return;

  const user = await checkAuth();
  if (user) {
    el.innerHTML = `
      <a href="/dashboard" class="nav-user">
        ${user.avatar_url ? `<img src="${user.avatar_url}" alt="" width="24" height="24" style="border-radius:50%;vertical-align:middle;margin-right:6px">` : ""}
        ${escapeHtml(user.github_username)}
      </a>`;
  } else {
    el.innerHTML = `<a href="/login" class="btn ghost sm">Sign in</a>`;
  }
}

function escapeHtml(s) {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

document.addEventListener("DOMContentLoaded", updateNav);
