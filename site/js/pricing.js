/**
 * Pricing page — Razorpay checkout integration.
 */

const API = "https://api.recon.dev";

let currentUser = null;

async function initPricing() {
  try {
    const resp = await fetch(`${API}/v1/auth/me`, { credentials: "include" });
    if (resp.ok) currentUser = await resp.json();
  } catch {
    // Not authenticated — buttons will redirect to login
  }

  // Update button states
  document.querySelectorAll("[data-upgrade-tier]").forEach((btn) => {
    const tier = btn.getAttribute("data-upgrade-tier");
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

  const btn = document.querySelector(`[data-upgrade-tier="${tierName}"]`);
  if (btn) {
    btn.textContent = "Processing...";
    btn.disabled = true;
  }

  try {
    const resp = await fetch(`${API}/v1/billing/checkout`, {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ tier: tierName }),
    });

    if (!resp.ok) {
      const err = await resp.json();
      alert(err.error || "Failed to create checkout");
      return;
    }

    const data = await resp.json();

    // Open Razorpay checkout modal
    const rzp = new Razorpay({
      key: data.key_id,
      amount: data.amount,
      currency: data.currency,
      order_id: data.order_id,
      name: "recon",
      description: `${data.tier} plan - monthly`,
      theme: { color: "#18181b" },
      handler: function (_response) {
        // Payment success on client — server confirms via webhook
        window.location.href = "/dashboard?upgraded=true";
      },
      modal: {
        ondismiss: function () {
          if (btn) {
            btn.textContent = `Upgrade to ${tierName}`;
            btn.disabled = false;
          }
        },
      },
    });
    rzp.open();
  } catch (e) {
    alert("Payment error: " + e.message);
    if (btn) {
      btn.textContent = `Upgrade to ${tierName}`;
      btn.disabled = false;
    }
  }
}

document.addEventListener("DOMContentLoaded", initPricing);
