/**
 * Pricing page — starts a Razorpay subscription for the clicked tier,
 * then full-page redirects to Razorpay's hosted checkout (short_url).
 *
 * We use Razorpay Subscriptions (not Orders), so the user authorises a
 * recurring mandate and the first charge runs immediately. `subscription.
 * activated` fires back to our webhook and flips the tier; we then
 * redirect the browser to /dashboard.
 */

var currentUser = null;

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
      body: JSON.stringify({ tier: tierName }),
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
    // our callback URL (configured in Razorpay dashboard → Subscriptions).
    // The webhook is what actually grants the tier; this redirect is UX.
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
  initPricing();
});
