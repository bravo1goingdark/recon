/**
 * Serves release tarballs from R2.
 * Route: /releases/<version>/<filename>
 * Key in bucket: releases/<version>/<filename>
 */
export async function onRequest(ctx) {
  const path = ctx.params.path;
  const key = "releases/" + (Array.isArray(path) ? path.join("/") : path);

  const object = await ctx.env.RELEASE_BUCKET.get(key);
  if (!object) {
    return new Response("Not found", { status: 404 });
  }

  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set("etag", object.httpEtag);
  headers.set("cache-control", "public, max-age=31536000, immutable");

  return new Response(object.body, { headers });
}
