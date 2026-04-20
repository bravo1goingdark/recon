/**
 * Shared auth state — loaded on every page.
 * Stores session token in localStorage, sends via Authorization header.
 * On OAuth callback, the token arrives in the URL fragment (#token=xxx).
 */

const API = "https://recon-api.kumarashutosh34169.workers.dev/api";
const TOKEN_KEY = "recon_session";

/** Extract token from URL fragment on OAuth callback redirect. */
function captureTokenFromFragment() {
  var hash = window.location.hash;
  if (hash && hash.indexOf("token=") !== -1) {
    var token = hash.split("token=")[1].split("&")[0];
    if (token) {
      localStorage.setItem(TOKEN_KEY, token);
      // Clean the URL fragment so the token isn't visible/bookmarkable
      history.replaceState(null, "", window.location.pathname);
    }
  }
}

/** Get stored session token. */
function getToken() {
  return localStorage.getItem(TOKEN_KEY);
}

/** Make an authenticated fetch to the API. */
function authFetch(path, opts) {
  opts = opts || {};
  opts.headers = opts.headers || {};
  var token = getToken();
  if (token) {
    opts.headers["Authorization"] = "Bearer " + token;
  }
  return fetch(API + path, opts);
}

/** Check auth state — returns user object or null. */
async function checkAuth() {
  var token = getToken();
  if (!token) return null;
  try {
    var resp = await authFetch("/v1/auth/me");
    if (resp.ok) return await resp.json();
    // Token invalid/expired — clear it
    localStorage.removeItem(TOKEN_KEY);
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

/** Logout — clear token and redirect. */
async function logout() {
  var token = getToken();
  if (token) {
    // Tell server to destroy the session
    await authFetch("/v1/auth/logout", { method: "POST" }).catch(function () {});
  }
  localStorage.removeItem(TOKEN_KEY);
  window.location.href = "/";
}

function escapeHtml(s) {
  var d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// Run on every page load
captureTokenFromFragment();
document.addEventListener("DOMContentLoaded", updateNav);
