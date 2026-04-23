const WORKER = "https://recon-api.kumarashutosh34169.workers.dev";

export async function onRequest(ctx) {
  const url = new URL(ctx.request.url);
  const target = WORKER + url.pathname + url.search;

  const req = new Request(target, {
    method: ctx.request.method,
    headers: ctx.request.headers,
    body: ["GET", "HEAD"].includes(ctx.request.method) ? undefined : ctx.request.body,
    redirect: "manual",
  });

  return fetch(req);
}
