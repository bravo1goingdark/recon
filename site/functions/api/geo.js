/**
 * Pages Function — GET /api/geo
 *
 * Returns the caller's country (per Cloudflare's IP-geolocation) plus our
 * suggested default currency for the subscribe flow. The pricing page
 * uses this to render ₹-pricing to Indian users automatically, so UPI
 * AutoPay + Net Banking eNACH are reachable. Users can still override
 * with the currency toggle.
 *
 * No PII, no tracking — country is the only field and it's what
 * Cloudflare's edge already computed for routing. Cached by the browser
 * for 10 minutes to avoid hammering the edge on every pricing-page load.
 *
 * Absent cf.country (local dev, non-Cloudflare proxies) → defaults to USD
 * so the page still renders something sensible.
 */
export async function onRequest(ctx) {
  const country = ctx.request.cf?.country ?? null;
  const suggested_currency = country === "IN" ? "INR" : "USD";

  return new Response(
    JSON.stringify({ country, suggested_currency }),
    {
      headers: {
        "content-type": "application/json",
        // 10-minute browser cache. Geo rarely changes mid-session and
        // the endpoint is cheap, but caching saves a round-trip.
        "cache-control": "public, max-age=600",
      },
    },
  );
}
