/**
 * Dashboard page — uses authFetch() from auth.js for all API calls.
 *
 * Wiring note: we cannot use inline `onclick=` attributes under the
 * strict CSP (`script-src 'self'` with no unsafe-inline). Every
 * interactive element is bound once at DOMContentLoaded via
 * `addEventListener`, with the dynamic revoke buttons handled by a
 * single event-delegation listener on the #keys container.
 */

// Set by loadDashboard; read by the delete-account confirm-modal logic
// (needs the user's github_username to validate the typed confirmation).
var currentUser = null;

async function loadDashboard() {
  var user = await checkAuth();
  if (!user) {
    window.location.href = "/login";
    return;
  }
  // Promote to module-scope so openDeleteAccountModal / the confirm-input
  // validator can read the user's github_username.
  currentUser = user;

  renderProfile(user);

  var keysResp = await authFetch("/v1/dashboard/keys");
  var keys = [];
  if (keysResp.ok) {
    var data = await keysResp.json();
    keys = data.keys || [];
    renderKeys(keys);
    renderQuickstart(keys);
    toggleKeyButtons(keys);
  }

  var billingResp = await authFetch("/v1/billing/portal");
  if (billingResp.ok) {
    var billing = await billingResp.json();
    renderBilling(billing);
  }

  await loadRepos();

  // Razorpay redirects here with `?just_paid=1` after a successful
  // subscription auth. The webhook usually lands within a couple of
  // seconds but there's a window where the dashboard would still show
  // "Free". Poll /v1/billing/portal until the tier flips, then refresh
  // the page so the badge + sidebar reflect the upgrade.
  if (window.location.search.indexOf("just_paid=1") !== -1) {
    pollForTierUpgrade(user.tier);
  }
}

/**
 * After a successful Razorpay subscription auth, poll /v1/billing/portal
 * until the user's tier moves off `oldTier`. The webhook usually fires
 * within ~2s; we poll for up to 30s before giving up. Stops at the first
 * tier change OR when the user navigates away. Drops the `just_paid`
 * query param via history.replaceState so a refresh doesn't re-trigger.
 */
async function pollForTierUpgrade(oldTier) {
  var attempts = 0;
  var MAX_ATTEMPTS = 15; // 15 × 2s = 30s
  showUpgradePendingBanner();

  // Strip the just_paid flag so a manual refresh during/after the poll
  // doesn't re-arm this loop.
  try {
    var url = new URL(window.location.href);
    url.searchParams.delete("just_paid");
    window.history.replaceState({}, "", url.toString());
  } catch (_) {}

  var iv = setInterval(async function () {
    attempts++;
    var resp = await authFetch("/v1/billing/portal");
    if (resp.ok) {
      var billing = await resp.json();
      if (billing.tier !== oldTier) {
        clearInterval(iv);
        hideUpgradePendingBanner();
        // Full reload so the user sees the new tier badge + limits
        // without us having to surgically re-render every panel.
        window.location.reload();
        return;
      }
    }
    if (attempts >= MAX_ATTEMPTS) {
      clearInterval(iv);
      hideUpgradePendingBanner(
        "Payment received — tier upgrade is taking longer than usual. Refresh in a minute, or contact support if it doesn't appear.",
      );
    }
  }, 2000);
}

function showUpgradePendingBanner() {
  var existing = document.getElementById("upgrade-pending-banner");
  if (existing) return;
  var b = document.createElement("div");
  b.id = "upgrade-pending-banner";
  b.style.cssText =
    "position:fixed;top:0;left:0;right:0;background:var(--clay);color:var(--paper);padding:10px 16px;text-align:center;font-size:13px;z-index:1000;font-family:var(--mono)";
  b.textContent =
    "Payment received — confirming your upgrade…";
  document.body.appendChild(b);
}

function hideUpgradePendingBanner(failureMessage) {
  var b = document.getElementById("upgrade-pending-banner");
  if (!b) return;
  if (failureMessage) {
    b.textContent = failureMessage;
    b.style.background = "var(--ink)";
    setTimeout(function () {
      if (b.parentNode) b.parentNode.removeChild(b);
    }, 8000);
  } else {
    b.parentNode.removeChild(b);
  }
}

/**
 * Fetch the user's registered-repo list (server-side max_repos
 * enforcement) and render it into the Repos tab. Pulled out as its own
 * function so the Remove handler can re-fetch after a delete.
 */
async function loadRepos() {
  var reposResp = await authFetch("/v1/dashboard/repos");
  if (!reposResp.ok) return;
  var data = await reposResp.json();
  renderRepos(data);
}

/**
 * Fetch token-savings rollups (Pro/Team feature) and render the Savings
 * tab. Lazy — called when the user clicks the Savings tab, not on
 * initial dashboard load, so Free users never pay the round-trip.
 *
 * Server contract: GET /v1/dashboard/savings returns
 *   { tier, range_days, daily: [{day, calls, …, tokens_saved}], totals, upsell? }
 * Free tier yields range_days=0 + an upsell payload; we render an
 * upgrade card without ever drawing the chart.
 */
async function loadSavings() {
  var box = document.getElementById("savings");
  if (!box) return;
  box.innerHTML = '<p class="empty">loading...</p>';
  try {
    var resp = await authFetch("/v1/dashboard/savings");
    if (!resp.ok) {
      box.innerHTML =
        '<p class="empty">Could not load savings (' +
        escapeHtml(String(resp.status)) +
        "). Try again later.</p>";
      return;
    }
    var data = await resp.json();
    renderSavings(data);
  } catch (e) {
    box.innerHTML =
      '<p class="empty">Network error loading savings.</p>';
  }
}

/**
 * Format an integer with thousands separators. Used for the headline
 * "tokens saved" number — a comma-separated 3,200,000 reads a lot
 * better than 3200000 at this size on the page.
 */
function fmtInt(n) {
  if (typeof n !== "number" || !isFinite(n)) return "0";
  return n.toLocaleString("en-US");
}

/**
 * Compress a long token count into "3.2M" / "850K" / "120" so the
 * headline tile reads at a glance. Three significant figures with a
 * single-letter suffix; never lossy in a way that changes the user's
 * perception of the order of magnitude.
 */
function fmtCompact(n) {
  if (typeof n !== "number" || !isFinite(n) || n < 0) return "0";
  if (n >= 1_000_000_000) return (n / 1_000_000_000).toFixed(2) + "B";
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return String(Math.round(n));
}

/**
 * Render an inline-SVG line chart of the daily tokens-saved series.
 *
 * Aesthetic borrowed from `WebstormProjects/token`'s SpendChart (Chart.js):
 * thin clay line, soft downward gradient, three-tier y-axis with mono
 * tick labels, auto-skipped x-axis dates, and a hover guide + tooltip
 * pinned to the nearest data point. We stay on inline SVG (no chart
 * library) to keep the dashboard a single static HTML page.
 *
 * Empty series renders an inert "push to populate" placeholder so the
 * layout never collapses on a first-time-paid user.
 */
function renderSparkline(daily) {
  var W = 720, H = 240;
  var PAD_LEFT = 48, PAD_RIGHT = 12, PAD_TOP = 16, PAD_BOTTOM = 28;
  var plotW = W - PAD_LEFT - PAD_RIGHT;
  var plotH = H - PAD_TOP - PAD_BOTTOM;

  if (!daily || daily.length === 0) {
    return (
      '<div style="height:240px;display:flex;align-items:center;justify-content:center;' +
        'border-top:1px solid var(--rule-soft);border-bottom:1px solid var(--rule-soft)">' +
        '<span style="font-family:var(--mono);font-size:11px;color:var(--ink-3)">' +
          'push from <code style="background:transparent;padding:0">recon savings push</code> to populate' +
        '</span>' +
      '</div>'
    );
  }

  var values = daily.map(function (d) { return d.tokens_saved; });
  var rawMax = Math.max.apply(null, values);
  var max = rawMax === 0 ? 1 : rawMax;
  var stepX = plotW / Math.max(1, values.length - 1);
  var baselineY = PAD_TOP + plotH;

  var pts = values.map(function (v, i) {
    return {
      x: PAD_LEFT + (values.length === 1 ? plotW / 2 : i * stepX),
      y: PAD_TOP + plotH - (v / max) * plotH,
    };
  });

  function smoothPath(pts) {
    if (pts.length < 2) return "";
    if (pts.length === 2) {
      return "M" + pts[0].x.toFixed(1) + "," + pts[0].y.toFixed(1) +
             " L" + pts[1].x.toFixed(1) + "," + pts[1].y.toFixed(1);
    }
    var d = "M" + pts[0].x.toFixed(1) + "," + pts[0].y.toFixed(1);
    for (var i = 0; i < pts.length - 1; i++) {
      var p0 = pts[i - 1] || pts[i];
      var p1 = pts[i];
      var p2 = pts[i + 1];
      var p3 = pts[i + 2] || p2;
      var c1x = p1.x + (p2.x - p0.x) / 6;
      var c1y = p1.y + (p2.y - p0.y) / 6;
      var c2x = p2.x - (p3.x - p1.x) / 6;
      var c2y = p2.y - (p3.y - p1.y) / 6;
      d += " C" + c1x.toFixed(1) + "," + c1y.toFixed(1) +
           " " + c2x.toFixed(1) + "," + c2y.toFixed(1) +
           " " + p2.x.toFixed(1) + "," + p2.y.toFixed(1);
    }
    return d;
  }

  var linePath = smoothPath(pts);
  var areaPath = linePath +
    " L" + pts[pts.length - 1].x.toFixed(1) + "," + baselineY.toFixed(1) +
    " L" + pts[0].x.toFixed(1) + "," + baselineY.toFixed(1) + " Z";

  // ── Y-axis: 3 ticks (0, max/2, max). Horizontal gridlines + mono
  // tick labels in the left gutter. Skip the bottom rule (the area
  // path's flat bottom + the x-axis labels already define the floor).
  var yTicks = [0, max / 2, max];
  var yAxis = yTicks.map(function (val) {
    var y = PAD_TOP + plotH - (val / max) * plotH;
    var grid = val === 0
      ? ''
      : '<line x1="' + PAD_LEFT + '" y1="' + y.toFixed(1) +
        '" x2="' + (W - PAD_RIGHT) + '" y2="' + y.toFixed(1) +
        '" stroke="var(--rule-soft)" stroke-width="1"/>';
    var label =
      '<text x="' + (PAD_LEFT - 8) + '" y="' + (y + 3).toFixed(1) +
      '" text-anchor="end" font-family="var(--mono)" font-size="10" ' +
      'fill="var(--ink-3)" letter-spacing="0.04em">' +
      escapeHtml(fmtCompact(val)) + '</text>';
    return grid + label;
  }).join("");

  // ── X-axis: aim for ~5 ticks. Pick evenly spaced indices into the
  // series so dates spread out instead of crowding at the edges.
  var targetTicks = Math.min(5, daily.length);
  var xTickIdx = [];
  if (targetTicks <= 1) {
    xTickIdx = [0];
  } else {
    for (var t = 0; t < targetTicks; t++) {
      xTickIdx.push(Math.round(t * (daily.length - 1) / (targetTicks - 1)));
    }
  }
  // Format "YYYY-MM-DD" → relative time: "today", "1d", "3d", "14d".
  // Data is daily UTC, so we compute distance in whole UTC days from
  // the current UTC midnight. The hover tooltip still shows the
  // absolute ISO date (precise on hover, at-a-glance on the axis).
  function relativeDay(iso) {
    var m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(String(iso || ""));
    if (!m) return String(iso);
    var rowUtcMs = Date.UTC(+m[1], +m[2] - 1, +m[3]);
    var now = new Date();
    var todayUtcMs = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate());
    var diffDays = Math.round((todayUtcMs - rowUtcMs) / 86400000);
    if (diffDays <= 0) return "today";
    if (diffDays === 1) return "1d";
    return diffDays + "d";
  }
  var xAxis = xTickIdx.map(function (idx, j) {
    var x = pts[idx].x;
    var anchor = j === 0 ? "start"
      : j === xTickIdx.length - 1 ? "end"
      : "middle";
    return (
      '<text x="' + x.toFixed(1) + '" y="' + (H - 8).toFixed(1) +
      '" text-anchor="' + anchor +
      '" font-family="var(--mono)" font-size="10" ' +
      'fill="var(--ink-3)" letter-spacing="0.04em">' +
      escapeHtml(relativeDay(daily[idx].day)) + '</text>'
    );
  }).join("");

  // ── Hover layer: one invisible rect per data point covering its
  // vertical band. Mouseover/out shows/moves a vertical guide,
  // a circle marker, and a tooltip group above the point. Wired up
  // post-render in `wireSparklineHover` (called by renderSavings).
  var bands = pts.map(function (p, i) {
    var bandX = i === 0 ? PAD_LEFT : (pts[i - 1].x + p.x) / 2;
    var bandRight = i === pts.length - 1
      ? (W - PAD_RIGHT)
      : (p.x + pts[i + 1].x) / 2;
    return (
      '<rect class="spark-band" data-idx="' + i + '" ' +
      'data-x="' + p.x.toFixed(1) + '" data-y="' + p.y.toFixed(1) + '" ' +
      'data-day="' + escapeHtml(daily[i].day) + '" ' +
      'data-saved="' + escapeHtml(fmtInt(values[i])) + '" ' +
      'x="' + bandX.toFixed(1) + '" y="' + PAD_TOP + '" ' +
      'width="' + Math.max(1, bandRight - bandX).toFixed(1) + '" ' +
      'height="' + plotH + '" ' +
      'fill="transparent" pointer-events="all"/>'
    );
  }).join("");

  var gradId = "savings-grad-" + Math.random().toString(36).slice(2, 9);

  return (
    '<svg class="spark-svg" data-pad-top="' + PAD_TOP + '" data-baseline="' + baselineY +
    '" viewBox="0 0 ' + W + ' ' + H +
    '" preserveAspectRatio="none" ' +
    'style="width:100%;height:240px;display:block;overflow:visible">' +
      '<defs>' +
        '<linearGradient id="' + gradId + '" x1="0" y1="0" x2="0" y2="1">' +
          '<stop offset="0%" stop-color="var(--clay)" stop-opacity="0.18"/>' +
          '<stop offset="100%" stop-color="var(--clay)" stop-opacity="0"/>' +
        '</linearGradient>' +
      '</defs>' +
      yAxis +
      '<path d="' + areaPath + '" fill="url(#' + gradId + ')" stroke="none"/>' +
      '<path d="' + linePath + '" fill="none" stroke="var(--clay)" ' +
        'stroke-width="1.25" stroke-linejoin="round" stroke-linecap="round"/>' +
      xAxis +
      // Hover overlay — initially hidden, populated on mouse move.
      '<g class="spark-hover" style="visibility:hidden;pointer-events:none">' +
        '<line class="spark-guide" x1="0" x2="0" y1="' + PAD_TOP + '" y2="' + baselineY +
          '" stroke="var(--ink-3)" stroke-width="1" stroke-dasharray="2 3" opacity="0.6"/>' +
        '<circle class="spark-dot" r="3.5" fill="var(--clay)" stroke="var(--paper)" stroke-width="1.5"/>' +
        '<g class="spark-tip">' +
          '<rect class="spark-tip-bg" rx="3" ry="3" fill="var(--paper)" stroke="var(--rule)" stroke-width="1"/>' +
          '<text class="spark-tip-day" font-family="var(--mono)" font-size="10" fill="var(--ink-3)" letter-spacing="0.04em"></text>' +
          '<text class="spark-tip-val" font-family="var(--mono)" font-size="11" fill="var(--ink)" letter-spacing="0.02em"></text>' +
        '</g>' +
      '</g>' +
      bands +
    "</svg>"
  );
}

/**
 * Wire the hover interactions for the sparkline. Called after
 * `renderSavings` injects the SVG into the DOM. Idempotent — re-running
 * after a re-render rebinds against the fresh nodes.
 *
 * Implementation note: SVG `<text>` doesn't auto-size its background,
 * so we measure the rendered text via getBBox() and resize the
 * tooltip rect to fit. Tooltip flips to the right of the cursor when
 * it would otherwise overflow the left edge.
 */
function wireSparklineHover() {
  var svgs = document.querySelectorAll("svg.spark-svg");
  svgs.forEach(function (svg) {
    var hover = svg.querySelector(".spark-hover");
    var guide = svg.querySelector(".spark-guide");
    var dot = svg.querySelector(".spark-dot");
    var tipG = svg.querySelector(".spark-tip");
    var tipBg = svg.querySelector(".spark-tip-bg");
    var tipDay = svg.querySelector(".spark-tip-day");
    var tipVal = svg.querySelector(".spark-tip-val");
    var padTop = parseFloat(svg.getAttribute("data-pad-top") || "16");

    function showAt(rect) {
      var x = parseFloat(rect.getAttribute("data-x"));
      var y = parseFloat(rect.getAttribute("data-y"));
      var day = rect.getAttribute("data-day");
      var saved = rect.getAttribute("data-saved");

      guide.setAttribute("x1", x);
      guide.setAttribute("x2", x);
      dot.setAttribute("cx", x);
      dot.setAttribute("cy", y);
      tipDay.textContent = day;
      tipVal.textContent = saved + " saved";

      // Position the tooltip group above the point, then size its bg
      // to fit the rendered text. flipRight keeps the tip inside the
      // viewBox when the cursor is on the left edge.
      var TIP_PAD_X = 8, TIP_PAD_Y = 6;
      var dayBox = tipDay.getBBox();
      var valBox = tipVal.getBBox();
      var w = Math.max(dayBox.width, valBox.width) + TIP_PAD_X * 2;
      var h = dayBox.height + valBox.height + TIP_PAD_Y * 2 + 2;

      var tipX = x - w / 2;
      var viewW = svg.viewBox.baseVal.width;
      if (tipX < 4) tipX = x + 12;
      else if (tipX + w > viewW - 4) tipX = x - w - 12;

      var tipY = Math.max(padTop + 2, y - h - 12);
      tipBg.setAttribute("x", tipX);
      tipBg.setAttribute("y", tipY);
      tipBg.setAttribute("width", w);
      tipBg.setAttribute("height", h);
      tipDay.setAttribute("x", tipX + TIP_PAD_X);
      tipDay.setAttribute("y", tipY + TIP_PAD_Y + dayBox.height - 2);
      tipVal.setAttribute("x", tipX + TIP_PAD_X);
      tipVal.setAttribute("y", tipY + TIP_PAD_Y + dayBox.height + valBox.height + 0);

      hover.style.visibility = "visible";
    }

    var bands = svg.querySelectorAll("rect.spark-band");
    bands.forEach(function (r) {
      r.addEventListener("mouseenter", function () { showAt(r); });
      r.addEventListener("mousemove", function () { showAt(r); });
    });
    svg.addEventListener("mouseleave", function () {
      hover.style.visibility = "hidden";
    });
  });
}

/**
 * Render the Savings panel body. Three states:
 *   - upsell      → Free tier, render the upgrade card
 *   - empty paid  → Pro/Team but no rows yet → render the "push from CLI" hint
 *   - paid w/data → headline + sparkline + per-day table
 */
function renderSavings(data) {
  var box = document.getElementById("savings");
  if (!box) return;

  if (data.upsell) {
    var url = (data.upsell.upgrade_url || "/pricing").toString();
    box.innerHTML =
      '<div style="border:1px solid var(--rule);border-radius:6px;padding:24px;background:var(--paper-2)">' +
      '<div style="font-size:13px;font-family:var(--mono);text-transform:uppercase;letter-spacing:.08em;color:var(--ink-3);margin-bottom:12px">Pro / Team feature</div>' +
      '<h3 style="font-size:22px;letter-spacing:-.02em;margin-bottom:10px">Token-savings rollups</h3>' +
      '<p style="color:var(--ink-2);max-width:60ch;margin-bottom:18px">' +
      escapeHtml(data.upsell.message || "Upgrade to a paid plan to start aggregating token savings across your sessions.") +
      "</p>" +
      '<a class="btn primary sm" href="' +
      escapeHtml(url) +
      '">Upgrade your plan</a>' +
      "</div>";
    return;
  }

  var totals = data.totals || {};
  var saved = totals.tokens_saved || 0;
  var calls = totals.calls || 0;
  var range = data.range_days || 0;
  var daily = Array.isArray(data.daily) ? data.daily : [];

  // Empty paid state: zero rows yet.
  if (daily.length === 0) {
    box.innerHTML =
      '<div style="display:flex;align-items:baseline;gap:14px;flex-wrap:wrap;margin-bottom:14px">' +
      '<div style="font-size:38px;font-family:var(--serif);letter-spacing:-.02em">0 tokens</div>' +
      '<div style="color:var(--ink-3);font-size:13px">estimated saved · last ' +
      escapeHtml(String(range)) +
      " days</div></div>" +
      renderSparkline([]) +
      '<p style="margin-top:14px;color:var(--ink-2);font-size:14px">No rollups pushed yet. Run <code>recon savings push</code> after using the MCP tools, or set <code>RECON_AUTO_PUSH_SAVINGS=1</code> to push automatically when each session ends.</p>';
    return;
  }

  // Paid + data: headline + chart + table.
  var rows = daily
    .slice()
    .reverse() // newest first in the table; chart stays time-ordered
    .map(function (d) {
      return (
        "<tr>" +
        '<td style="font-family:var(--mono);font-size:12px;color:var(--ink-2)">' +
        escapeHtml(String(d.day)) +
        "</td>" +
        '<td style="text-align:right">' +
        fmtInt(d.calls) +
        "</td>" +
        '<td style="text-align:right">' +
        fmtInt(d.response_tokens) +
        "</td>" +
        '<td style="text-align:right">' +
        fmtInt(d.baseline_tokens) +
        "</td>" +
        '<td style="text-align:right;color:var(--clay);font-weight:500">' +
        fmtInt(d.tokens_saved) +
        "</td>" +
        "</tr>"
      );
    })
    .join("");

  // ── Measured / Estimated badge ───────────────────────────────────
  // The dashboard shows three different surfaces depending on how
  // many of today's calls were actually measured against a real
  // Read/grep alternative (i.e. ran with RECON_MEASURED_BASELINES=1
  // on the server). The thresholds match the migration plan:
  //   ratio >= 0.8        → "Measured"      + new disclaimer
  //   0.05 <= ratio < 0.8 → "Mostly estimated" + legacy disclaimer
  //   ratio <  0.05       → unchanged from pre-v0.4 (legacy line)
  // The split-row beneath the headline shows both numbers so users
  // can see the static estimate side-by-side with the live measurement.
  var measuredCalls = (totals.measured_calls || 0);
  var measuredSaved = (totals.measured_tokens_saved || 0);
  var measuredRatio = calls > 0 ? measuredCalls / calls : 0;
  var estimatedSaved = saved - measuredSaved;
  if (estimatedSaved < 0) estimatedSaved = 0; // defensive clamp
  var badgeHtml;
  var disclaimer;
  if (measuredRatio >= 0.8) {
    badgeHtml =
      '<span style="display:inline-block;font-family:var(--mono);font-size:10px;letter-spacing:.1em;text-transform:uppercase;' +
      'padding:2px 8px;border-radius:2px;background:var(--clay-soft);color:var(--clay);margin-left:8px">Measured</span>';
    disclaimer = "Measured against in-process Read/grep equivalent on every call.";
  } else if (measuredRatio >= 0.05) {
    badgeHtml =
      '<span style="display:inline-block;font-family:var(--mono);font-size:10px;letter-spacing:.1em;text-transform:uppercase;' +
      'padding:2px 8px;border-radius:2px;border:1px solid var(--rule);color:var(--ink-3);margin-left:8px">Mostly estimated</span>';
    disclaimer = "Estimate, not a measurement.";
  } else {
    badgeHtml = "";
    disclaimer = "Estimate, not a measurement.";
  }
  // Split row only renders when there's at least some measured data.
  // Otherwise the legacy single-headline UX is preserved exactly.
  var splitRow = "";
  if (measuredCalls > 0) {
    splitRow =
      '<div style="display:flex;flex-wrap:wrap;gap:24px;margin-top:6px;margin-bottom:14px;font-family:var(--mono);font-size:11px;color:var(--ink-3);letter-spacing:.04em">' +
        '<span>Estimated saved · <strong style="color:var(--ink-2);font-weight:500">' +
          fmtCompact(estimatedSaved) + '</strong></span>' +
        '<span>Measured saved · <strong style="color:var(--clay);font-weight:500">' +
          fmtCompact(measuredSaved) + '</strong></span>' +
      '</div>';
  }

  box.innerHTML =
    '<div style="display:flex;align-items:baseline;gap:18px;flex-wrap:wrap;margin-bottom:8px">' +
    '<div style="font-size:44px;font-family:var(--serif);letter-spacing:-.02em">' +
    fmtCompact(saved) +
    " tokens" + badgeHtml + "</div>" +
    '<div style="color:var(--ink-3);font-size:13px">estimated saved · last ' +
    escapeHtml(String(range)) +
    " days · " +
    fmtInt(calls) +
    " tool calls</div></div>" +
    splitRow +
    '<div style="font-size:12px;color:var(--ink-3);margin-bottom:14px">' +
    escapeHtml(disclaimer) +
    '</div>' +
    renderSparkline(daily) +
    '<table style="margin-top:18px;width:100%;border-collapse:collapse;font-size:13px">' +
    '<thead><tr style="border-bottom:1px solid var(--rule);text-align:left">' +
    '<th style="padding:8px 4px;font-weight:500">Day</th>' +
    '<th style="padding:8px 4px;font-weight:500;text-align:right">Calls</th>' +
    '<th style="padding:8px 4px;font-weight:500;text-align:right">Response tokens</th>' +
    '<th style="padding:8px 4px;font-weight:500;text-align:right">Baseline</th>' +
    '<th style="padding:8px 4px;font-weight:500;text-align:right">Est. saved</th>' +
    "</tr></thead><tbody>" +
    rows +
    "</tbody></table>";

  // Wire the hover layer against the freshly-injected SVG.
  wireSparklineHover();
}

function renderProfile(user) {
  var el = document.getElementById("profile");
  if (el) {
    el.innerHTML =
      '<div style="display:flex;align-items:center;gap:14px">' +
      (user.avatar_url
        ? '<img src="' + escapeHtml(user.avatar_url) + '" width="40" height="40" style="border-radius:50%;border:2px solid var(--rule)">'
        : "") +
      "<div><h1 style=\"font-family:var(--serif);font-weight:400;font-size:clamp(28px,4vw,40px);letter-spacing:-.03em\">" +
      escapeHtml(user.github_username) +
      '</h1></div><span class="tier-badge" style="margin-left:auto">' +
      escapeHtml(user.tier) +
      "</span></div>";
  }
  // Mirror plan + email into the sidebar footer/header so the user sees
  // their identity without relying on the main header alone.
  var tierEl = document.getElementById("ds-tier");
  if (tierEl) tierEl.textContent = (user.tier || "recon").toString();
  var emailEl = document.getElementById("ds-email");
  if (emailEl) emailEl.textContent = user.email || user.github_username || "";
}

// localStorage key for the dismiss-quickstart flag. Once set, the
// "Get started" panel stays hidden across reloads so returning users
// aren't re-onboarded every time they open the dashboard.
var QUICKSTART_DISMISSED_KEY = "recon.quickstart.dismissed";

function isQuickstartDismissed() {
  try {
    return localStorage.getItem(QUICKSTART_DISMISSED_KEY) === "1";
  } catch (_) {
    return false;
  }
}

function dismissQuickstart() {
  try { localStorage.setItem(QUICKSTART_DISMISSED_KEY, "1"); } catch (_) {}
  var el = document.getElementById("quickstart");
  if (el) el.style.display = "none";
}

function renderQuickstart(keys) {
  var el = document.getElementById("quickstart");
  var keyEl = document.getElementById("qs-key");
  if (!el) return;
  if (isQuickstartDismissed()) {
    el.style.display = "none";
    return;
  }
  var active = keys.filter(function (k) { return !k.revoked; });
  if (active.length > 0 && keyEl) {
    // Show prefix with ellipsis — full key was shown once at generation time
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

  // NOTE: the Revoke button uses `data-key-id` + a single delegated
  // listener on #keys. Don't reintroduce `onclick=` — CSP blocks it.
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
          : '<button class="btn ghost sm danger" data-action="revoke-key" data-key-id="' + escapeHtml(k.id) + '">Revoke</button>') +
        "</div>"
      );
    })
    .join("");
}

/**
 * Render the user's registered-repo list (v0.2.0+).
 *
 * Each row shows a truncated fingerprint, first/last seen, and a
 * Remove button. Rows are session-tracked server-side via
 * /v1/dashboard/repos so the dashboard and the CLI see the same
 * authoritative state.
 */
function renderRepos(payload) {
  var el = document.getElementById("repos");
  if (!el) return;

  var repos = (payload && payload.repos) || [];
  var limit = (payload && payload.limit) || 1;
  var tier = (payload && payload.tier) || "Free";

  var header =
    '<div class="repos-header">' +
    "<span><b>" +
    repos.length +
    "</b> / " +
    limit +
    " repos used" +
    "</span>" +
    '<span class="dim">' +
    escapeHtml(tier) +
    " plan" +
    "</span>" +
    "</div>";

  if (repos.length === 0) {
    el.innerHTML =
      header +
      '<p class="empty">No repos registered yet. Run <code>recon init --mcp &lt;ide&gt;</code> in a project to register one.</p>';
    return;
  }

  // Delegated Remove buttons via [data-action='remove-repo'] +
  // data-fingerprint. CSP blocks inline onclick, so we wire one
  // listener on #repos in wireControls.
  var rows = repos
    .map(function (r) {
      return (
        '<div class="repo-row">' +
        '<code class="repo-fp" title="' +
        escapeHtml(r.fingerprint) +
        '">' +
        escapeHtml(r.fingerprint.slice(0, 16)) +
        "…</code>" +
        '<span class="dim">first ' +
        formatDate(r.first_seen_at) +
        "</span>" +
        '<span class="dim">last ' +
        formatDate(r.last_seen_at) +
        "</span>" +
        '<button class="btn ghost sm" data-action="remove-repo" data-fingerprint="' +
        escapeHtml(r.fingerprint) +
        '">Remove</button>' +
        "</div>"
      );
    })
    .join("");

  el.innerHTML = header + rows;
}

/**
 * Remove a server-side repo slot from the dashboard. Re-fetches the
 * list on success so the count + tier badge stay accurate.
 */
async function removeRepo(fingerprint) {
  if (!fingerprint) return;
  if (!confirm("Remove this repo from your account? Re-running `recon init` from that project will register it again (if you're under your tier limit).")) {
    return;
  }
  var resp = await authFetch("/v1/dashboard/repos/" + encodeURIComponent(fingerprint), {
    method: "DELETE",
  });
  if (!resp.ok && resp.status !== 204) {
    var msg = "Failed to remove repo.";
    try {
      var body = await resp.json();
      if (body && body.error) msg = body.error;
    } catch {}
    alert(msg);
    return;
  }
  await loadRepos();
}

function renderBilling(billing) {
  var el = document.getElementById("billing");
  if (!el) return;

  var tc = billing.tier_config;
  var limitsHtml =
    tc.limits.max_repos + " repos" +
    " · " + tc.limits.max_files.toLocaleString() + " files" +
    " · " + (tc.limits.max_loc / 1000).toLocaleString() + "K LOC";

  // Pick the status line + right-hand CTA based on subscription state.
  // The states the dashboard has to reflect:
  //   1. Free + no sub                              → Subscribe CTA
  //   2. Subscribed + cancel_at_period_end OR
  //      status='cancelled'                          → "Access until DATE" + Resubscribe
  //   3. Subscribed + status='halted'                 → "Payment failed" + Resubscribe
  //   4. Subscribed + status in (active, authenticated, pending, created)
  //      AND cancel_at_period_end=0                   → status copy + Cancel button
  //
  // The Cancel button has to render for the in-between states too —
  // 'created' / 'authenticated' / 'pending' is the window between the user
  // clicking Subscribe and Razorpay confirming the first charge. The
  // backend's POST /v1/billing/cancel accepts all of these, so locking the
  // UI out of them stranded users with no self-serve cancel path.
  var statusLine = "";
  var actionHtml = "";
  var sub = billing.subscription;

  // States we treat as "subscription is live or about to become live" and
  // therefore eligible for a Cancel button. Mirrors the WHERE-status list
  // in /v1/billing/cancel and /v1/billing/subscribe's stack-prevention
  // guard so the three never disagree about what "active-ish" means.
  var ACTIVEISH = ["created", "authenticated", "active", "pending"];

  if (sub && sub.tier !== "Free") {
    var endDate = sub.current_period_end
      ? formatDate(sub.current_period_end)
      : "unknown";

    if (
      ACTIVEISH.indexOf(sub.status) !== -1 &&
      !sub.cancel_at_period_end
    ) {
      // status copy varies a bit so the user can tell whether their first
      // charge has landed yet. The Cancel CTA is the same in all cases.
      var statusText;
      if (sub.status === "active") {
        statusText = "Next billing: " + endDate;
      } else if (sub.status === "pending") {
        statusText = "Awaiting payment confirmation";
      } else {
        statusText = "Awaiting first charge from Razorpay";
      }
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px">' +
        escapeHtml(statusText) +
        "</div>";
      actionHtml =
        '<button id="cancel-sub-btn" class="btn ghost sm" style="margin-left:auto">Cancel subscription</button>';
    } else if (sub.cancel_at_period_end || sub.status === "cancelled") {
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px;color:var(--clay)">Cancelled — access until ' +
        escapeHtml(endDate) +
        "</div>";
      actionHtml =
        '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Resubscribe →</a>';
    } else if (sub.status === "halted") {
      statusLine =
        '<div class="dim" style="font-family:var(--mono);font-size:11px;margin-top:4px;color:var(--clay)">Payment failed — access until ' +
        escapeHtml(endDate) +
        ". Update card at Razorpay and resubscribe.</div>";
      actionHtml =
        '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Resubscribe →</a>';
    }
  } else if (billing.tier === "Free") {
    actionHtml =
      '<a href="/pricing" class="btn primary sm" style="margin-left:auto">Subscribe →</a>';
  }

  el.innerHTML =
    '<div class="billing-row">' +
    '<div><strong>' + escapeHtml(billing.tier) + "</strong>" +
    statusLine +
    "</div>" +
    '<div class="billing-limits">' + limitsHtml + "</div>" +
    actionHtml +
    "</div>";

  // Wire the cancel button if it was rendered this pass.
  var cancelBtn = document.getElementById("cancel-sub-btn");
  if (cancelBtn) {
    // Opens the modal; the modal's confirm button actually runs the
    // cancel call (wired in wireControls).
    cancelBtn.addEventListener("click", openCancelSubModal);
  }
}

/**
 * Format a UTC ISO date or unix-second-ish value into a local YYYY-MM-DD
 * string. Worker returns ISO-8601 from D1; fall back gracefully for
 * anything else.
 */
function formatDate(value) {
  try {
    var d = new Date(value);
    if (isNaN(d.getTime())) return String(value);
    return d.toISOString().slice(0, 10);
  } catch (_) {
    return String(value);
  }
}

// ─── Cancel subscription ─────────────────────────────────────────────────
// openCancelSubModal pops the in-page modal; the confirm button (wired in
// wireControls) runs the actual cancel call. Browser confirm() was the
// previous UX — jarring, not themable, and shows the URL bar in some
// browsers which looks unprofessional.

function openCancelSubModal() {
  var modal = document.getElementById("cancel-sub-modal");
  if (modal) modal.style.display = "flex";
}

function closeCancelSubModal() {
  var modal = document.getElementById("cancel-sub-modal");
  if (modal) modal.style.display = "none";
}

async function cancelSubscription() {
  closeCancelSubModal();

  var btn = document.getElementById("cancel-sub-btn");
  if (btn) {
    btn.textContent = "Cancelling…";
    btn.disabled = true;
  }

  var resp = await authFetch("/v1/billing/cancel", { method: "POST" });
  if (!resp.ok) {
    var err = await resp.json().catch(function () {
      return { error: "Cancel failed (" + resp.status + ")" };
    });
    alert(err.error || "Cancel failed");
    if (btn) {
      btn.textContent = "Cancel subscription";
      btn.disabled = false;
    }
    return;
  }

  loadDashboard();
}

// ─── API key lifecycle ──────────────────────────────────────────────────
// One active key per account. The worker rejects a second POST /keys with
// 409 (see dashboardRoutes.post("/keys", ...) in worker/src/routes/dashboard.ts).
// Here we keep the UI's Generate / Rotate buttons in sync with key state
// so the user never clicks Generate to hit a 409.

// Holds the id of the key targeted by the revoke-key modal. Set when the
// user clicks a row's Revoke button; read by revokeKeyConfirmed.
// Null → modal wasn't opened via a row, so it's the "Rotate" path instead.
var pendingRevokeKeyId = null;
// True when the Rotate button was clicked: after revoke succeeds, we
// automatically call generate() so the user gets a replacement key.
var pendingRotate = false;

function toggleKeyButtons(keys) {
  var active = (keys || []).filter(function (k) { return !k.revoked; });
  var gen = document.getElementById("generate-key-btn");
  var rot = document.getElementById("rotate-key-btn");
  var hint = document.getElementById("key-hint");
  if (active.length === 0) {
    if (gen) gen.style.display = "";
    if (rot) rot.style.display = "none";
    if (hint) hint.style.display = "none";
  } else {
    if (gen) gen.style.display = "none";
    if (rot) rot.style.display = "";
    if (hint) hint.style.display = "";
  }
}

async function doGenerateKey() {
  var resp = await authFetch("/v1/dashboard/keys", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name: "Default" }),
  });

  if (!resp.ok) {
    var err = await resp.json().catch(function () {
      return { error: "Failed to generate key" };
    });
    alert(err.error || "Failed to generate key");
    return;
  }

  var data = await resp.json();
  showKeyModal(data.key);
  loadDashboard();
}

function openRevokeKeyModal(keyId, options) {
  pendingRevokeKeyId = keyId;
  pendingRotate = !!(options && options.rotate);
  var titleEl = document.getElementById("revoke-key-title");
  var bodyEl = document.getElementById("revoke-key-body");
  var confirmBtn = document.getElementById("revoke-key-confirm-btn");
  if (pendingRotate) {
    if (titleEl) titleEl.textContent = "Rotate API key?";
    if (bodyEl) bodyEl.textContent =
      "We'll revoke your current key and immediately issue a replacement. " +
      "Any running recon serve using the old key loses access within 15 minutes.";
    if (confirmBtn) confirmBtn.textContent = "Rotate now";
  } else {
    if (titleEl) titleEl.textContent = "Revoke API key?";
    if (bodyEl) bodyEl.textContent =
      "Any running recon serve using this key will lose access within 15 minutes, " +
      "and the key can't be used for new logins. This cannot be undone.";
    if (confirmBtn) confirmBtn.textContent = "Revoke";
  }
  var modal = document.getElementById("revoke-key-modal");
  if (modal) modal.style.display = "flex";
}

function closeRevokeKeyModal() {
  pendingRevokeKeyId = null;
  pendingRotate = false;
  var modal = document.getElementById("revoke-key-modal");
  if (modal) modal.style.display = "none";
}

async function revokeKeyConfirmed() {
  var id = pendingRevokeKeyId;
  var rotate = pendingRotate;
  closeRevokeKeyModal();
  if (!id) return;

  var resp = await authFetch("/v1/dashboard/keys/" + encodeURIComponent(id), {
    method: "DELETE",
  });
  if (!resp.ok && resp.status !== 404) {
    // 404 → row already gone; treat as success.
    alert("Failed to revoke key");
    return;
  }

  if (rotate) {
    // Generate the replacement immediately. If this fails (worker
    // transient), loadDashboard will render the zero-keys state and
    // the user can click Generate manually.
    await doGenerateKey();
    return;
  }
  loadDashboard();
}

// Called by the row-level Revoke button (event delegation in wireControls).
function openRevokeForRowKey(id) {
  openRevokeKeyModal(id, { rotate: false });
}

// Called by the "Rotate key" button below the keys list.
async function rotateKey() {
  // Need to find the (one) active key's id to pass into the modal.
  var resp = await authFetch("/v1/dashboard/keys");
  if (!resp.ok) {
    alert("Failed to load key state");
    return;
  }
  var data = await resp.json();
  var active = (data.keys || []).filter(function (k) { return !k.revoked; });
  if (active.length === 0) {
    // Edge case: somebody deleted the row via another tab before the
    // click. Just fall through to generating a fresh one.
    doGenerateKey();
    return;
  }
  openRevokeKeyModal(active[0].id, { rotate: true });
}

function showKeyModal(key) {
  var modal = document.getElementById("key-modal");
  var keyEl = document.getElementById("new-key-value");
  var loginCmdEl = document.getElementById("login-cmd");
  if (modal && keyEl) {
    keyEl.textContent = key;
    if (loginCmdEl) loginCmdEl.textContent = "recon login " + key;
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
      setTimeout(function () { btn.textContent = "Copy key"; }, 2000);
    }
  }
}

function copyLoginCmd() {
  var el = document.getElementById("login-cmd");
  if (el) {
    navigator.clipboard.writeText(el.textContent || "");
    var btn = document.getElementById("copy-login-btn");
    if (btn) {
      btn.textContent = "Copied!";
      setTimeout(function () { btn.textContent = "Copy command"; }, 2000);
    }
  }
}

/**
 * Bind all interactive elements once on page load.
 * Static buttons get direct listeners; the dynamic revoke buttons
 * (generated by renderKeys into #keys innerHTML) use a single
 * delegated listener on the container.
 */
function openDeleteAccountModal() {
  if (!currentUser) return;
  var expectedEl = document.getElementById("delete-expected-username");
  if (expectedEl) expectedEl.textContent = currentUser.github_username;
  var input = document.getElementById("delete-confirm-input");
  var confirmBtn = document.getElementById("delete-confirm-btn");
  if (input) {
    input.value = "";
    input.focus();
  }
  if (confirmBtn) confirmBtn.disabled = true;
  var modal = document.getElementById("delete-account-modal");
  if (modal) modal.style.display = "flex";
}

function closeDeleteAccountModal() {
  var modal = document.getElementById("delete-account-modal");
  if (modal) modal.style.display = "none";
}

async function deleteAccount() {
  var btn = document.getElementById("delete-confirm-btn");
  if (btn) {
    btn.textContent = "Deleting…";
    btn.disabled = true;
  }

  var resp = await authFetch("/v1/dashboard/account", { method: "DELETE" });
  if (!resp.ok) {
    var err = await resp.json().catch(function () {
      return { error: "Delete failed (" + resp.status + ")" };
    });
    alert(err.error || "Delete failed");
    if (btn) {
      btn.textContent = "Delete permanently";
      btn.disabled = false;
    }
    return;
  }

  // Session is gone server-side — clear any client state and redirect
  // to home. Logout helper from auth.js does the local cleanup.
  if (typeof logout === "function") {
    logout();
  } else {
    window.location.href = "/";
  }
}

// ─── Tabs ────────────────────────────────────────────────────────────────
// Sidebar nav items select the visible panel; one panel renders at a time
// (others get the native `hidden` attribute). Active button gets the
// clay-dot indicator via `.active`.

function activateTab(name) {
  document.querySelectorAll(".ds-nav-item").forEach(function (btn) {
    var isActive = btn.getAttribute("data-tab") === name;
    btn.classList.toggle("active", isActive);
    btn.setAttribute("aria-selected", isActive ? "true" : "false");
  });
  document.querySelectorAll(".tab-panel").forEach(function (panel) {
    panel.hidden = panel.id !== "panel-" + name;
  });
}

function wireTabs() {
  document.querySelectorAll(".ds-nav-item").forEach(function (btn) {
    btn.addEventListener("click", function () {
      var name = btn.getAttribute("data-tab");
      if (!name) return;
      activateTab(name);
      // Lazy-load the Savings tab the first time it's opened so Free
      // users never trigger a round-trip and paid users only pay it when
      // they actually look at the panel. After a successful render
      // we leave the DOM populated; subsequent re-clicks re-fetch so
      // the sparkline reflects fresh pushes within the session.
      if (name === "savings") {
        loadSavings();
      }
    });
  });
}

// ─── Sidebar collapse ───────────────────────────────────────────────────
// Persist preference in localStorage so a returning user keeps their
// chosen layout. The expand button is a floating affordance shown only
// when the sidebar is closed.
var SIDEBAR_KEY = "recon.dashboard.sidebar.collapsed";
function setSidebarCollapsed(collapsed) {
  var shell = document.getElementById("dashShell");
  var expand = document.getElementById("dsExpand");
  if (!shell) return;
  shell.classList.toggle("collapsed", !!collapsed);
  if (expand) expand.hidden = !collapsed;
  try { localStorage.setItem(SIDEBAR_KEY, collapsed ? "1" : "0"); } catch (_) {}
}
function wireSidebar() {
  var initial = false;
  try { initial = localStorage.getItem(SIDEBAR_KEY) === "1"; } catch (_) {}
  setSidebarCollapsed(initial);
  var c = document.getElementById("dsCollapse");
  if (c) c.addEventListener("click", function () { setSidebarCollapsed(true); });
  var e = document.getElementById("dsExpand");
  if (e) e.addEventListener("click", function () { setSidebarCollapsed(false); });
}

function wireControls() {
  wireTabs();
  wireSidebar();

  // API key buttons. Generate is visible when the user has zero active
  // keys; Rotate replaces it when a key exists. toggleKeyButtons (called
  // from loadDashboard) manages the display: toggle.
  var gen = document.getElementById("generate-key-btn");
  if (gen) gen.addEventListener("click", doGenerateKey);

  var rot = document.getElementById("rotate-key-btn");
  if (rot) rot.addEventListener("click", rotateKey);

  // Revoke-key modal
  var revConfirm = document.getElementById("revoke-key-confirm-btn");
  if (revConfirm) revConfirm.addEventListener("click", revokeKeyConfirmed);
  var revCancel = document.getElementById("revoke-key-cancel-btn");
  if (revCancel) revCancel.addEventListener("click", closeRevokeKeyModal);

  // Cancel-subscription modal
  var cancelConfirm = document.getElementById("cancel-sub-confirm-btn");
  if (cancelConfirm) cancelConfirm.addEventListener("click", cancelSubscription);
  var cancelCancel = document.getElementById("cancel-sub-cancel-btn");
  if (cancelCancel) cancelCancel.addEventListener("click", closeCancelSubModal);

  // Delete-account modal
  var delBtn = document.getElementById("delete-account-btn");
  if (delBtn) delBtn.addEventListener("click", openDeleteAccountModal);

  var delCancel = document.getElementById("delete-cancel-btn");
  if (delCancel) delCancel.addEventListener("click", closeDeleteAccountModal);

  var delConfirm = document.getElementById("delete-confirm-btn");
  if (delConfirm) delConfirm.addEventListener("click", deleteAccount);

  // Only enable the confirm button once the user has typed their exact
  // username. Prevents accidental enter-to-submit nukes.
  var delInput = document.getElementById("delete-confirm-input");
  if (delInput) {
    delInput.addEventListener("input", function () {
      if (!delConfirm) return;
      delConfirm.disabled =
        !currentUser ||
        delInput.value.trim() !== currentUser.github_username;
    });
  }

  // Both the desktop nav link and the mobile-sheet entry use the same
  // logout handler. `logout` is defined in auth.js — shared across pages.
  ["logout-link", "logout-link-sheet"].forEach(function (id) {
    var el = document.getElementById(id);
    if (!el) return;
    el.addEventListener("click", function (e) {
      e.preventDefault();
      if (typeof logout === "function") logout();
    });
  });

  var closeBtn = document.getElementById("close-key-modal-btn");
  if (closeBtn) closeBtn.addEventListener("click", closeKeyModal);

  var copyBtn = document.getElementById("copy-btn");
  if (copyBtn) copyBtn.addEventListener("click", copyKey);

  var copyLoginBtn = document.getElementById("copy-login-btn");
  if (copyLoginBtn) copyLoginBtn.addEventListener("click", copyLoginCmd);

  // Dismiss "Get started" panel. Persists via localStorage so the panel
  // doesn't reappear on every reload after the user has onboarded.
  var qsClose = document.getElementById("quickstart-close");
  if (qsClose) qsClose.addEventListener("click", dismissQuickstart);

  // Event delegation: one listener for every Revoke button rendered
  // into #keys by renderKeys(). Opens the revoke-key modal instead of
  // the old browser confirm() dialog.
  var keysContainer = document.getElementById("keys");
  if (keysContainer) {
    keysContainer.addEventListener("click", function (e) {
      var btn = e.target.closest("[data-action='revoke-key']");
      if (!btn) return;
      var id = btn.getAttribute("data-key-id");
      if (id) openRevokeForRowKey(id);
    });
  }

  // Same pattern for the Repos tab: one listener for every Remove
  // button rendered by renderRepos().
  var reposContainer = document.getElementById("repos");
  if (reposContainer) {
    reposContainer.addEventListener("click", function (e) {
      var btn = e.target.closest("[data-action='remove-repo']");
      if (!btn) return;
      var fp = btn.getAttribute("data-fingerprint");
      if (fp) removeRepo(fp);
    });
  }
}

document.addEventListener("DOMContentLoaded", function () {
  wireControls();
  loadDashboard();
});
