/**
 * Pricing page — uses authFetch() from auth.js for Razorpay checkout.
 */

var currentUser = null;

async function initPricing() {
  currentUser = await checkAuth();

  // Update button states
  document.querySelectorAll("[data-upgrade-tier]").forEach(function (btn) {
    var tier = btn.getAttribute("data-upgrade-tier");
    if (currentUser && currentUser.tier === tier) {
      btn.textContent = "Current plan";
      btn.disabled = true;
      btn.classList.add("disabled");
    }
  });
}

async function upgradeTier(tierName) {
  if (!currentUser) {
    window.location.href = "/login";
    return;
  }

  var btn = document.querySelector('[data-upgrade-tier="' + tierName + '"]');
  if (btn) {
    btn.textContent = "Processing...";
    btn.disabled = true;
  }

  try {
    var resp = await authFetch("/v1/billing/checkout", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ tier: tierName }),
    });

    if (!resp.ok) {
      var err = await resp.json();
      alert(err.error || "Failed to create checkout");
      return;
    }

    var data = await resp.json();

    var rzp = new Razorpay({
      key: data.key_id,
      amount: data.amount,
      currency: data.currency,
      order_id: data.order_id,
      name: "recon",
      description: data.tier + " plan — monthly",
      theme: { color: "#18181b" },
      handler: function () {
        window.location.href = "/dashboard?upgraded=true";
      },
      modal: {
        ondismiss: function () {
          if (btn) {
            btn.textContent = "Upgrade to " + tierName + " \u2192";
            btn.disabled = false;
          }
        },
      },
    });
    rzp.open();
  } catch (e) {
    alert("Payment error: " + e.message);
    if (btn) {
      btn.textContent = "Upgrade to " + tierName + " \u2192";
      btn.disabled = false;
    }
  }
}

/**
 * Bind upgrade buttons. Inline `onclick=` would trip CSP under the
 * site's strict script-src — one delegated listener on the document
 * dispatches to upgradeTier() based on the button's data attribute.
 */
function wireUpgradeButtons() {
  document.querySelectorAll("[data-upgrade-tier]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var tier = btn.getAttribute("data-upgrade-tier");
      if (tier) upgradeTier(tier);
    });
  });
}

document.addEventListener("DOMContentLoaded", function () {
  wireUpgradeButtons();
  initPricing();
});
