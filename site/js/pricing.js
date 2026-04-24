/**
 * Pricing page — dual-currency subscribe flow.
 *
 * On load:
 *   1. Fetch /api/geo → { country, suggested_currency }.
 *   2. If user has a previous choice in localStorage, use that; otherwise
 *      use the geo-suggested default. Indian users → INR (so UPI /
 *      Net Banking eNACH work); everyone else → USD.
 *   3. Render the active currency's prices into each .price block and
 *      highlight the matching toggle button.
 *   4. When the user clicks Subscribe, POST /v1/billing/subscribe with
 *      { tier, currency } and full-page redirect to Razorpay's short_url.
 *
 * The HTML ships with USD baked in as a fallback so visitors with JS
 * disabled still see a readable page; this script is a progressive
 * enhancement.
 */

var currentUser = null;
var activeCurrency = "USD"; // overwritten after detect+load

async function initPricing() {
  currentUser = await checkAuth();

  // Mark "current plan" buttons. A user who's already on Pro sees "Current
  // plan" on the Pro card and can only interact with Team.
  document.querySelectorAll("[data-upgrade-tier]").forEach(function (btn) {
    var tier = btn.getAttribute("data-upgrade-tier");
    if (currentUser && currentUser.tier === tier) {
      btn.textContent = "Current plan";
      btn.disabled = true;
      btn.classList.add("disabled");
    }
  });

  // Currency resolution: user's explicit choice wins over geo default.
  var saved = null;
  try {
    saved = localStorage.getItem("recon.currency");
  } catch (_) {
    // localStorage blocked (private mode, strict settings) — fall through.
  }
  if (saved === "INR" || saved === "USD") {
    applyCurrency(saved);
  } else {
    // Fetch geo hint from our Pages function. If it fails (offline, local
    // dev), the page is already showing USD fallbacks, so no action needed.
    try {
      var resp = await fetch("/api/geo", { method: "GET" });
      if (resp.ok) {
        var data = await resp.json();
        if (data && data.suggested_currency) {
          applyCurrency(data.suggested_currency);
        }
      }
    } catch (_) {
      applyCurrency("USD");
    }
  }

  // Reveal the toggle now that we know the active currency.
  var toggle = document.getElementById("currency-toggle");
  if (toggle) toggle.style.display = "";
}

function applyCurrency(currency) {
  activeCurrency = currency;
  var attr = "data-price-" + currency.toLowerCase();
  document.querySelectorAll(".price[" + attr + "]").forEach(function (el) {
    var value = el.getAttribute(attr);
    if (!value) return;
    // Keep the " /month" suffix visible; swap the amount before it.
    el.innerHTML = value + ' <span>/month</span>';
  });
  // "Starter" card is hardcoded $0 — no attributes, no swap needed.

  // Highlight the active toggle button.
  document.querySelectorAll(".currency-btn").forEach(function (btn) {
    var isActive = btn.getAttribute("data-currency") === currency;
    btn.style.background = isActive ? "var(--ink)" : "none";
    btn.style.color = isActive ? "var(--paper)" : "inherit";
    btn.style.borderColor = isActive ? "var(--ink)" : "var(--rule)";
  });
}

function wireCurrencyToggle() {
  document.querySelectorAll(".currency-btn").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var currency = btn.getAttribute("data-currency");
      if (!currency) return;
      applyCurrency(currency);
      try {
        localStorage.setItem("recon.currency", currency);
      } catch (_) {
        // Not fatal — just means the choice won't persist across loads.
      }
    });
  });
}

async function subscribeToTier(tierName) {
  if (!currentUser) {
    window.location.href = "/login";
    return;
  }

  var btn = document.querySelector('[data-upgrade-tier="' + tierName + '"]');
  if (btn) {
    btn.textContent = "Opening checkout…";
    btn.disabled = true;
  }

  try {
    var resp = await authFetch("/v1/billing/subscribe", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ tier: tierName, currency: activeCurrency }),
    });

    if (!resp.ok) {
      var err = await resp.json().catch(function () {
        return { error: "Subscribe failed (" + resp.status + ")" };
      });
      alert(err.error || "Failed to start subscription");
      if (btn) {
        btn.textContent = "Subscribe to " + tierName + " →";
        btn.disabled = false;
      }
      return;
    }

    var data = await resp.json();

    // Full-page redirect to Razorpay's hosted subscription page. The user
    // authorises the mandate there; on success Razorpay redirects back to
    // our callback URL. The webhook is what actually grants the tier;
    // this redirect is UX.
    if (data.short_url) {
      window.location.href = data.short_url;
      return;
    }

    alert("Unexpected response — no checkout URL returned.");
  } catch (e) {
    alert("Payment error: " + e.message);
    if (btn) {
      btn.textContent = "Subscribe to " + tierName + " →";
      btn.disabled = false;
    }
  }
}

/**
 * Bind subscribe buttons. Inline `onclick=` would trip CSP under the
 * site's strict script-src — one delegated listener per button dispatches
 * to subscribeToTier() based on the button's data attribute.
 */
function wireUpgradeButtons() {
  document.querySelectorAll("[data-upgrade-tier]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var tier = btn.getAttribute("data-upgrade-tier");
      if (tier) subscribeToTier(tier);
    });
  });
}

document.addEventListener("DOMContentLoaded", function () {
  wireUpgradeButtons();
  wireCurrencyToggle();
  initPricing();
});
