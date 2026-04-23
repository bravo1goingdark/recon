/**
 * Pages Function — proxies `/api/*` to the recon-api Worker.
 *
 * Forwards the original browser-visible host via X-Forwarded-Host / -Proto
 * so the Worker can compute same-origin URLs (OAuth redirect_uri, absolute
 * callback links) that match what the GitHub OAuth app has registered AND
 * what the browser will actually hit on the return leg.
 *
 * Set-Cookie: the Worker sets `__Host-session` in its response; because
 * the browser sees the response as coming from `mcprecon.pages.dev`
 * (the Pages origin), the cookie is scoped to that origin — which is
 * exactly where the site will read it from on subsequent calls.
 */

const WORKER = "https://recon-api.kumarashutosh34169.workers.dev";

export async function onRequest(ctx) {
  const url = new URL(ctx.request.url);
  const target = WORKER + url.pathname + url.search;

  // Clone headers so we can append without mutating the shared instance.
  const headers = new Headers(ctx.request.headers);
  headers.set("X-Forwarded-Host", url.host);
  headers.set("X-Forwarded-Proto", url.protocol.replace(":", ""));

  const req = new Request(target, {
    method: ctx.request.method,
    headers,
    body: ["GET", "HEAD"].includes(ctx.request.method) ? undefined : ctx.request.body,
    redirect: "manual",
  });

  return fetch(req);
}
