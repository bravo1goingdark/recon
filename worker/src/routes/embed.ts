/**
 * `POST /v1/embed` — hosted embedding endpoint.
 *
 * The CLI's `recon-embed-client` crate (see crates/recon-embed-client)
 * sends batches of source-code chunks here; this route caches by
 * SHA-256 in KV (so the same chunk across users hits Modal once),
 * forwards uncached entries to the Modal-deployed Jina v2-base-code
 * service, and returns 768-dim L2-normalised vectors.
 *
 * Auth model
 * ----------
 *   - Bearer API key (recon login): `requireApiKey` middleware.
 *   - Tier gate: Pro/Team/Enterprise can call; Free gets 402 with
 *     an upsell payload. (Mirrors `accountRoutes.post("/savings")`.)
 *   - CF rate-limit: 1000/min per api-key prefix via RL_EMBED.
 *     Daily quotas are NOT enforced here in v0.4 — see wrangler.toml
 *     comment block for rationale.
 *
 * Cache semantics
 * ---------------
 *   - Key   = "v1:" + sha256(text) (lowercase hex). Bumping the
 *             prefix invalidates the cache cleanly without touching
 *             individual keys; do that if the embedding model
 *             changes (model dim swap forces a full rebuild on
 *             clients anyway).
 *   - Value = JSON-stringified Vec<f32> of length 768.
 *   - TTL   = 30 days. Source chunks rotate as users edit code, so
 *             stale entries age out without manual cleanup.
 *
 * Failure modes
 * -------------
 *   - Modal 5xx / network error / >10 s timeout → 503 fail-closed
 *     with `{"error":"embed_service_unavailable","retry_after":30}`.
 *     Client crate falls back to lexical-only search.
 *   - Body validation errors → 400.
 *   - Free-tier hit → 402 with upsell.
 *   - Auth missing/invalid → 401 (from `requireApiKey`).
 *   - Burst limit → 429 (from `rateLimit`).
 */

import { Hono } from "hono";
import { sha256Hex } from "../lib/crypto";
import { apiKeyRateKey, requireApiKey, type ApiKeyVars } from "../middleware/apikey";
import { rateLimit } from "../middleware/ratelimit";
import type { Env } from "../types";

type EmbedEnv = { Bindings: Env; Variables: ApiKeyVars };

export const embedRoutes = new Hono<EmbedEnv>();

// Burst guard. Daily quotas live elsewhere (TBD) — this stops a
// runaway loop from billing a Modal cold-start storm before the
// human can react.
embedRoutes.use("*", rateLimit<EmbedEnv>("RL_EMBED", apiKeyRateKey, 60));
embedRoutes.use("*", requireApiKey);

/** Tiers that can call /v1/embed. Mirrors the savings push gate. */
function canEmbed(tier: string): boolean {
  return tier === "Pro" || tier === "Team" || tier === "Enterprise";
}

/** Cache key shape — bumping the prefix retires the whole cache. */
function cacheKeyFor(textHashHex: string): string {
  return `v1:${textHashHex}`;
}

/** Modal POST timeout. 10 s covers warm-path inference + headroom for
 *  cold-start; longer means we make the user wait through pathological
 *  latency rather than failing fast and falling back to lexical. */
const MODAL_TIMEOUT_MS = 10_000;

/** Cache TTL. 30 days balances "model + chunk text both stable" against
 *  KV storage cost. Source chunks rotate; stale entries expire on their
 *  own. */
const CACHE_TTL_SECONDS = 60 * 60 * 24 * 30;

/** Max strings per request. Matches the Modal endpoint's batch ceiling. */
const MAX_BATCH = 64;
/** Max characters per text. Jina v2-base-code's 8192-token context window
 *  is comfortably under this in characters; rejecting longer here keeps
 *  Modal from silently truncating. */
const MAX_CHARS = 8192;

interface ModalEmbedResponse {
  vectors?: number[][];
  error?: string;
}

embedRoutes.post("/", async (c) => {
  const apiKey = c.get("apiKey");
  if (!canEmbed(apiKey.tier)) {
    return c.json(
      {
        error: "embed_requires_pro",
        tier: apiKey.tier,
        message:
          "Hosted embeddings are a Pro/Team feature. Upgrade your plan at https://mcprecon.pages.dev/pricing.",
      },
      402,
    );
  }

  let body: unknown;
  try {
    body = await c.req.json();
  } catch {
    return c.json({ error: "Invalid JSON body" }, 400);
  }
  if (
    !body ||
    typeof body !== "object" ||
    !Array.isArray((body as { texts?: unknown }).texts)
  ) {
    return c.json({ error: "body must be {texts: string[]}" }, 400);
  }
  const texts = (body as { texts: unknown[] }).texts;
  if (texts.length === 0) {
    return c.json({ vectors: [] });
  }
  if (texts.length > MAX_BATCH) {
    return c.json({ error: `batch size must be <= ${MAX_BATCH}` }, 400);
  }
  if (!texts.every((t): t is string => typeof t === "string")) {
    return c.json({ error: "texts must be a list of strings" }, 400);
  }
  for (let i = 0; i < texts.length; i++) {
    if ((texts[i] as string).length > MAX_CHARS) {
      return c.json(
        { error: `texts[${i}] exceeds ${MAX_CHARS}-character limit` },
        400,
      );
    }
  }

  // ── 1. cache lookup (parallel KV reads, one round-trip-ish) ─────
  const hashes = await Promise.all(
    (texts as string[]).map((t) => sha256Hex(t)),
  );
  const keys = hashes.map(cacheKeyFor);

  // KV is optional in dev/tests; treat missing binding as a permanent
  // cache miss — the rest of the route still works against Modal.
  const kv = c.env.EMBED_CACHE;
  const cached: (number[] | null)[] = await Promise.all(
    keys.map((k) => (kv ? kv.get(k, "json").then((v) => v as number[] | null) : null)),
  );

  const missingIdx: number[] = [];
  for (let i = 0; i < texts.length; i++) {
    if (cached[i] === null) missingIdx.push(i);
  }

  // ── 2. fast-path: every text was cached ────────────────────────
  if (missingIdx.length === 0) {
    return c.json({ vectors: cached as number[][] });
  }

  // ── 3. fetch the uncached subset from Modal ────────────────────
  const url = c.env.MODAL_EMBED_URL;
  const token = c.env.MODAL_AUTH_TOKEN;
  if (!url || !token) {
    return c.json(
      {
        error: "embed_service_unavailable",
        retry_after: 30,
        message:
          "MODAL_EMBED_URL or MODAL_AUTH_TOKEN not configured on the Worker.",
      },
      503,
    );
  }

  const missingTexts = missingIdx.map((i) => texts[i] as string);
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), MODAL_TIMEOUT_MS);
  let modalResp: Response;
  try {
    modalResp = await fetch(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ texts: missingTexts }),
      signal: ac.signal,
    });
  } catch (e) {
    clearTimeout(timer);
    const reason = e instanceof Error && e.name === "AbortError"
      ? "timeout"
      : "network_error";
    return c.json(
      {
        error: "embed_service_unavailable",
        retry_after: 30,
        cause: reason,
      },
      503,
    );
  }
  clearTimeout(timer);

  if (!modalResp.ok) {
    return c.json(
      {
        error: "embed_service_unavailable",
        retry_after: 30,
        upstream_status: modalResp.status,
      },
      503,
    );
  }

  let parsed: ModalEmbedResponse;
  try {
    parsed = (await modalResp.json()) as ModalEmbedResponse;
  } catch {
    return c.json(
      { error: "embed_service_unavailable", retry_after: 30, cause: "bad_json" },
      503,
    );
  }
  const fresh = parsed.vectors;
  if (!Array.isArray(fresh) || fresh.length !== missingTexts.length) {
    return c.json(
      {
        error: "embed_service_unavailable",
        retry_after: 30,
        cause: "shape_mismatch",
      },
      503,
    );
  }

  // ── 4. write-through to KV for the freshly-fetched vectors ─────
  if (kv) {
    await Promise.all(
      missingIdx.map((origIdx, j) =>
        kv.put(keys[origIdx], JSON.stringify(fresh[j]), {
          expirationTtl: CACHE_TTL_SECONDS,
        }),
      ),
    );
  }

  // ── 5. reassemble in the original order ────────────────────────
  const out: number[][] = new Array(texts.length);
  for (let i = 0; i < texts.length; i++) {
    out[i] = (cached[i] ?? null) as number[];
  }
  for (let j = 0; j < missingIdx.length; j++) {
    out[missingIdx[j]] = fresh[j];
  }
  return c.json({ vectors: out });
});
