import type { Context, Next } from "hono";
import type { Env } from "../types";

/** CORS middleware — explicit origin allowlist, credentials: include support. */
export async function cors(
  c: Context<{ Bindings: Env }>,
  next: Next,
): Promise<Response | void> {
  const origin = c.req.header("Origin");
  const allowed = c.env.ALLOWED_ORIGINS.split(",").map((s) => s.trim());

  if (origin && allowed.includes(origin)) {
    c.header("Access-Control-Allow-Origin", origin);
    c.header("Access-Control-Allow-Credentials", "true");
    c.header(
      "Access-Control-Allow-Methods",
      "GET, POST, DELETE, PUT, OPTIONS",
    );
    c.header(
      "Access-Control-Allow-Headers",
      "Content-Type, Authorization",
    );
    c.header("Access-Control-Max-Age", "86400");
  }

  if (c.req.method === "OPTIONS") {
    return new Response(null, { status: 204, headers: c.res.headers });
  }

  await next();
}
