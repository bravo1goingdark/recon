/**
 * Shared auth state — loaded on every page.
 *
 * The session token is an HttpOnly + Secure + SameSite=Lax cookie
 * (__Host-session) set by the Worker on OAuth callback. JS cannot read
 * it and cannot write it — which is exactly the point: XSS that steals
 * a DOM variable cannot steal the cookie. Every fetch uses
 * `credentials: "include"` so the cookie rides along automatically.
 */

const API = "/api";

/** Make a same-origin authenticated fetch to the API. */
function authFetch(path, opts) {
  opts = opts || {};
  opts.headers = opts.headers || {};
  // Browser sends __Host-session automatically; we just need to ensure
  // credentials travel cross-subpath (same-origin Pages Function calls).
  opts.credentials = "include";
  return fetch(API + path, opts);
}

/** Check auth state — returns user object or null. */
async function checkAuth() {
  try {
    var resp = await authFetch("/v1/auth/me");
    if (resp.ok) return await resp.json();
  } catch {
    // Network error
  }
  return null;
}

/** Update nav with auth state. */
async function updateNav() {
  var el = document.getElementById("nav-auth");
  if (!el) return;

  var user = await checkAuth();
  if (user) {
    el.innerHTML =
      '<a href="/dashboard" style="font-family:var(--mono);font-size:12px;display:flex;align-items:center;gap:6px">' +
      (user.avatar_url
        ? '<img src="' + escapeHtml(user.avatar_url) + '" width="22" height="22" style="border-radius:50%">'
        : "") +
      escapeHtml(user.github_username) +
      "</a>";
  } else {
    el.innerHTML = '<a href="/login" class="btn sm ghost">Sign in</a>';
  }
}

/** Logout — tell the server to destroy the session; it clears the cookie. */
async function logout() {
  await authFetch("/v1/auth/logout", { method: "POST" }).catch(function () {});
  window.location.href = "/";
}

function escapeHtml(s) {
  var d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Clean up any legacy localStorage token left behind by older builds.
// Safe to run unconditionally; value was never load-bearing for the auth
// decision after this migration (the Worker ignores Bearer if the cookie
// is present).
try { localStorage.removeItem("recon_session"); } catch (e) { /* ignore */ }

// Run on every page load.
document.addEventListener("DOMContentLoaded", updateNav);
