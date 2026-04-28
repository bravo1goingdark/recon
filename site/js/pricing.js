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

  // Geo first — we need the caller's country before we decide whether to
  // expose the INR toggle at all. INR pricing is PPP for India only; a
  // non-IN visitor must not be able to toggle INR and effectively pay
  // ~75% less. Server-side /v1/billing/subscribe also rejects INR from
  // non-IN IPs (403), but gating the UI keeps the UX honest.
  var country = null;
  try {
    var resp = await fetch("/api/geo", { method: "GET" });
    if (resp.ok) {
      var data = await resp.json();
      country = data && data.country ? data.country : null;
    }
  } catch (_) {
    // Offline / local dev — leave country null, treat as non-IN.
  }
  var inrAllowed = country === "IN";

  if (!inrAllowed) {
    // Remove the INR toggle button entirely for non-IN visitors. A stale
    // `recon.currency === "INR"` in localStorage (from a past VPN trip,
    // testing, etc.) must also be wiped so we don't render INR prices to
    // someone who can't actually subscribe in INR.
    var inrBtn = document.querySelector('.currency-btn[data-currency="INR"]');
    if (inrBtn && inrBtn.parentNode) inrBtn.parentNode.removeChild(inrBtn);
    try {
      if (localStorage.getItem("recon.currency") === "INR") {
        localStorage.removeItem("recon.currency");
      }
    } catch (_) {}
  }

  // Currency resolution: user's explicit saved choice wins, BUT only if
  // it's still a choice they're allowed to make. Non-IN users fall back
  // to USD regardless.
  var saved = null;
  try { saved = localStorage.getItem("recon.currency"); } catch (_) {}

  var chosen;
  if (saved === "USD" || (saved === "INR" && inrAllowed)) {
    chosen = saved;
  } else {
    chosen = inrAllowed ? "INR" : "USD";
  }
  applyCurrency(chosen);

  // Reveal the (possibly-trimmed) toggle now that we know what to show.
  // For non-IN users only the USD button remains, so this is effectively
  // a read-only currency indicator rather than a choice.
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

  // Footer caption mirrors the active currency. The HTML ships with USD
  // baked in for JS-disabled readers; this swap keeps it honest when the
  // INR toggle is engaged.
  var footerCurrency = document.getElementById("footer-currency");
  if (footerCurrency) {
    footerCurrency.textContent = "all prices in " + currency;
  }
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

    // Open Razorpay's Checkout SDK in an in-page modal.
    //
    // Two complementary redirect paths are needed:
    //
    // 1. handler — fires for in-modal flows (UPI collect, cards without 3DS).
    //    Runs in the same JS context as this page so the __Host-session cookie
    //    (SameSite=Lax) is still present when the browser navigates to the
    //    dashboard.
    //
    // 2. callback_url — required for redirect-based mandate authorization
    //    (eNACH / Net Banking). These flows send the user's browser to an
    //    external bank portal. After authorization, the bank redirects back
    //    to Razorpay, which then does a TOP-LEVEL GET to callback_url.
    //    SameSite=Lax cookies ARE sent on top-level GET navigations, so the
    //    session survives. Without callback_url Razorpay has no return URL
    //    and shows "page not present".
    //
    //    Critically, redirect: true must NOT be set. That flag makes Razorpay
    //    use a POST form submission instead of a GET redirect — POST is
    //    cross-site and SameSite=Lax blocks the cookie entirely.
    //
    // The webhook is still the authoritative tier-grant. The dashboard
    // polls /v1/billing/portal until subscription.activated fires.
    //
    // Falls back to the hosted short_url page if the SDK isn't loaded
    // (CSP, ad-blocker, etc.) — user still completes auth, just without
    // the auto-redirect-back.
    if (typeof Razorpay === "undefined") {
      if (data.short_url) {
        window.location.href = data.short_url;
        return;
      }
      alert("Unexpected response — no checkout URL returned.");
      return;
    }

    var dashboardUrl = window.location.origin + "/dashboard?just_paid=1";
    var rzp = new Razorpay({
      key: data.key_id,
      subscription_id: data.subscription_id,
      name: "recon",
      description: tierName + " plan",
      handler: function () {
        window.location.href = dashboardUrl;
      },
      callback_url: dashboardUrl,
      prefill: currentUser
        ? {
            name: currentUser.github_username || "",
            email: currentUser.email || "",
          }
        : {},
      notes: {
        tier: tierName,
        currency: activeCurrency,
      },
      theme: { color: "#b07040" }, // matches site --clay accent
      modal: {
        ondismiss: function () {
          // User closed the modal without finishing auth — restore the
          // subscribe button so they can retry. The placeholder row in
          // D1 will be cleaned up by the webhook never firing (sub is
          // still 'created' status; the user can /subscribe again with
          // the cancel-at-period-end flow if they get stuck).
          if (btn) {
            btn.textContent = "Subscribe to " + tierName + " →";
            btn.disabled = false;
          }
        },
      },
    });
    rzp.open();
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
 *
 * The Starter card's link carries `data-upgrade-tier="Free"` so it can
 * still be marked "Current plan" for users already on Free. Clicking it
 * MUST fall through to its href ("/login" or "/dashboard") instead of
 * triggering /v1/billing/subscribe — the backend rejects Free with a 400
 * and the user got an alert. We skip Free here and let the browser
 * follow the link normally.
 */
function wireUpgradeButtons() {
  document.querySelectorAll("[data-upgrade-tier]").forEach(function (btn) {
    btn.addEventListener("click", function (e) {
      var tier = btn.getAttribute("data-upgrade-tier");
      if (!tier || tier === "Free") return;
      e.preventDefault();
      subscribeToTier(tier);
    });
  });
}

document.addEventListener("DOMContentLoaded", function () {
  wireUpgradeButtons();
  wireCurrencyToggle();
  initPricing();
});
