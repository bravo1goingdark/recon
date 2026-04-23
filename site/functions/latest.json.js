/**
 * Serves latest.json from R2 — used by install.sh to auto-detect current version.
 * Always re-validated (no-cache) so installs pick up new releases immediately.
 */
export async function onRequest(ctx) {
  const object = await ctx.env.RELEASE_BUCKET.get("latest.json");
  if (!object) {
    return new Response('{"error":"no releases yet"}', {
      status: 404,
      headers: { "content-type": "application/json" },
    });
  }

  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set("content-type", "application/json");
  headers.set("cache-control", "no-cache");
  headers.set("etag", object.httpEtag);

  return new Response(object.body, { headers });
}
