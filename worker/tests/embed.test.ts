/**
 * /v1/embed contract tests.
 *
 * Covers the auth, tier gate, body validation, cache hit/miss
 * behavior, and Modal-failure branches that `crates/recon-embed-client`
 * relies on. The Modal fetch itself is mocked at `globalThis.fetch`
 * per-test so we exercise the real route logic against a stub that
 * we control the responses + errors of.
 */
import {
  beforeEach,
  afterEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { env, getJson, resetDb } from "./setup";

// Helpers ─────────────────────────────────────────────────────────────

async function sha256Hex(s: string): Promise<string> {
  const data = new TextEncoder().encode(s);
  const buf = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(buf))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

async function seedUserKey(opts: {
  userId: string;
  key: string;
  tier: string;
}): Promise<void> {
  const db = (env as { RECON_DB: D1Database }).RECON_DB;
  await db
    .prepare(
      `INSERT INTO users (id, github_id, github_username, email, tier)
         VALUES (?, ?, ?, ?, ?)`,
    )
    .bind(
      opts.userId,
      Math.floor(Math.random() * 1_000_000),
      "u-" + opts.userId,
      `${opts.userId}@example.com`,
      opts.tier,
    )
    .run();
  const keyHash = await sha256Hex(opts.key);
  await db
    .prepare(
      `INSERT INTO api_keys
         (id, user_id, key_hash, key_prefix, name, tier, limits_json,
          expires_at, created_at, revoked_at)
       VALUES (?, ?, ?, ?, 'test', ?,
               '{"max_repos":10,"max_files":5000,"max_loc":200000}',
               null, datetime('now'), null)`,
    )
    .bind(
      "ak_" + opts.userId,
      opts.userId,
      keyHash,
      opts.key.slice(0, 14),
      opts.tier,
    )
    .run();
}

function postEmbed(
  key: string,
  body: unknown,
): Promise<{ status: number; body: unknown }> {
  return getJson("/v1/embed", {
    method: "POST",
    headers: {
      Authorization: "Bearer " + key,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
}

/** Build a 768-dim sentinel vector — value at index 0 carries the tag
 *  so tests can tell which caller produced it (cache vs Modal). */
function vec768(tag: number): number[] {
  const v = new Array(768).fill(0);
  v[0] = tag;
  return v;
}

// Modal stub — replaces `globalThis.fetch` for the duration of one
// test. Each test that needs Modal calls `mockModalOK([…vectors])` or
// `mockModalFailure(...)` then issues its request.

interface ModalCall {
  url: string;
  authHeader: string | null;
  body: { texts: string[] };
}

function mockModal(
  handler: (call: ModalCall) => Promise<Response> | Response,
): { calls: ModalCall[] } {
  const calls: ModalCall[] = [];
  vi.stubGlobal(
    "fetch",
    async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string"
        ? input
        : input instanceof URL
          ? input.toString()
          : input.url;
      // Only intercept calls to our Modal URL; let other fetches
      // (none expected, but defensive) pass through to the real one.
      if (!url.startsWith("https://modal.test/")) {
        return (globalThis as { __origFetch?: typeof fetch }).__origFetch!(
          input,
          init,
        );
      }
      const headers = new Headers(init?.headers ?? {});
      const bodyText =
        typeof init?.body === "string" ? init.body : "";
      const call: ModalCall = {
        url,
        authHeader: headers.get("authorization"),
        body: bodyText ? JSON.parse(bodyText) : { texts: [] },
      };
      calls.push(call);
      return handler(call);
    },
  );
  return { calls };
}

beforeEach(async () => {
  await resetDb();
  // Wipe the KV namespace so tests can't see each other's cached
  // vectors. Miniflare's KV is in-memory, lives across tests in the
  // same worker isolate. List + delete is fine for a small test
  // namespace.
  const kv = (env as { EMBED_CACHE?: KVNamespace }).EMBED_CACHE;
  if (kv) {
    const list = await kv.list();
    for (const k of list.keys) {
      await kv.delete(k.name);
    }
  }
});

afterEach(() => {
  vi.unstubAllGlobals();
});

// Tests ───────────────────────────────────────────────────────────────

describe("POST /v1/embed — auth + tier", () => {
  it("401 without an Authorization header", async () => {
    const r = await getJson("/v1/embed", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ texts: ["fn x() {}"] }),
    });
    expect(r.status).toBe(401);
  });

  it("Free tier: 402 with upsell payload", async () => {
    await seedUserKey({ userId: "u_free", key: "sk-recon-free", tier: "Free" });
    const r = await postEmbed("sk-recon-free", { texts: ["fn x() {}"] });
    expect(r.status).toBe(402);
    expect(r.body).toMatchObject({
      error: "embed_requires_pro",
      tier: "Free",
    });
  });
});

describe("POST /v1/embed — body validation", () => {
  beforeEach(async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
  });

  it("Empty texts list short-circuits to 200 + empty vectors", async () => {
    const r = await postEmbed("sk-recon-pro", { texts: [] });
    expect(r.status).toBe(200);
    expect(r.body).toEqual({ vectors: [] });
  });

  it("Batch over 64 → 400", async () => {
    const r = await postEmbed("sk-recon-pro", {
      texts: new Array(65).fill("x"),
    });
    expect(r.status).toBe(400);
    expect((r.body as { error: string }).error).toMatch(/batch size/);
  });

  it("Text over 8192 chars → 400", async () => {
    const r = await postEmbed("sk-recon-pro", {
      texts: ["a".repeat(8193)],
    });
    expect(r.status).toBe(400);
    expect((r.body as { error: string }).error).toMatch(/8192/);
  });

  it("texts not a list → 400", async () => {
    const r = await postEmbed("sk-recon-pro", { texts: "not a list" });
    expect(r.status).toBe(400);
  });

  it("Invalid JSON body → 400", async () => {
    const r = await getJson("/v1/embed", {
      method: "POST",
      headers: {
        Authorization: "Bearer sk-recon-pro",
        "Content-Type": "application/json",
      },
      body: "not json",
    });
    expect(r.status).toBe(400);
  });
});

describe("POST /v1/embed — cache + Modal forwarding", () => {
  beforeEach(async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
  });

  it("Cache hit: pre-populated KV returns vector without calling Modal", async () => {
    const text = "fn cached() {}";
    const hash = await sha256Hex(text);
    const kv = (env as { EMBED_CACHE: KVNamespace }).EMBED_CACHE;
    const cached = vec768(7);
    await kv.put(`v1:${hash}`, JSON.stringify(cached));

    const { calls } = mockModal(() =>
      Promise.resolve(
        new Response(JSON.stringify({ vectors: [vec768(99)] }), {
          status: 200,
        }),
      ),
    );
    const r = await postEmbed("sk-recon-pro", { texts: [text] });
    expect(r.status).toBe(200);
    const vecs = (r.body as { vectors: number[][] }).vectors;
    expect(vecs).toHaveLength(1);
    expect(vecs[0][0]).toBe(7); // sentinel — came from cache, not Modal
    expect(calls).toHaveLength(0);
  });

  it("Cache miss: Modal called with full batch, write-through to KV", async () => {
    const texts = ["fn a() {}", "fn b() {}"];
    const { calls } = mockModal((call) => {
      expect(call.body.texts).toEqual(texts);
      return Promise.resolve(
        new Response(
          JSON.stringify({ vectors: [vec768(1), vec768(2)] }),
          { status: 200 },
        ),
      );
    });

    const r = await postEmbed("sk-recon-pro", { texts });
    expect(r.status).toBe(200);
    expect(calls).toHaveLength(1);
    expect(calls[0].url).toBe("https://modal.test/embed");
    expect(calls[0].authHeader).toBe("Bearer test-modal-bearer");

    const kv = (env as { EMBED_CACHE: KVNamespace }).EMBED_CACHE;
    const h0 = await sha256Hex(texts[0]);
    const h1 = await sha256Hex(texts[1]);
    expect(await kv.get(`v1:${h0}`, "json")).toEqual(vec768(1));
    expect(await kv.get(`v1:${h1}`, "json")).toEqual(vec768(2));
  });

  it("Mixed batch: 1 cached + 2 fresh — Modal called only for the 2 missing, response order preserved", async () => {
    const texts = ["fn cached() {}", "fn fresh1() {}", "fn fresh2() {}"];
    const kv = (env as { EMBED_CACHE: KVNamespace }).EMBED_CACHE;
    const h0 = await sha256Hex(texts[0]);
    await kv.put(`v1:${h0}`, JSON.stringify(vec768(7)));

    const { calls } = mockModal((call) => {
      // Only the two uncached texts should travel.
      expect(call.body.texts).toEqual([texts[1], texts[2]]);
      return Promise.resolve(
        new Response(
          JSON.stringify({ vectors: [vec768(11), vec768(12)] }),
          { status: 200 },
        ),
      );
    });

    const r = await postEmbed("sk-recon-pro", { texts });
    expect(r.status).toBe(200);
    expect(calls).toHaveLength(1);

    // Result MUST be in the original input order.
    const vecs = (r.body as { vectors: number[][] }).vectors;
    expect(vecs[0][0]).toBe(7);  // cached
    expect(vecs[1][0]).toBe(11); // fresh #1
    expect(vecs[2][0]).toBe(12); // fresh #2
  });

  it("Cache key uses v1: prefix + lowercase hex sha256", async () => {
    const text = "stable-key-shape";
    mockModal(() =>
      Promise.resolve(
        new Response(JSON.stringify({ vectors: [vec768(1)] }), {
          status: 200,
        }),
      ),
    );
    await postEmbed("sk-recon-pro", { texts: [text] });

    const kv = (env as { EMBED_CACHE: KVNamespace }).EMBED_CACHE;
    const expectedHash = await sha256Hex(text);
    // Key shape regression-guard: v1: prefix, lowercase hex, exact 64
    // hex chars from sha256.
    expect(expectedHash).toMatch(/^[0-9a-f]{64}$/);
    const cached = await kv.get(`v1:${expectedHash}`, "json");
    expect(cached).toEqual(vec768(1));
  });
});

describe("POST /v1/embed — Modal failure modes", () => {
  beforeEach(async () => {
    await seedUserKey({ userId: "u_pro", key: "sk-recon-pro", tier: "Pro" });
  });

  it("Modal returns 5xx → 503 fail-closed", async () => {
    mockModal(() =>
      Promise.resolve(new Response("upstream boom", { status: 502 })),
    );
    const r = await postEmbed("sk-recon-pro", { texts: ["fn x() {}"] });
    expect(r.status).toBe(503);
    expect(r.body).toMatchObject({
      error: "embed_service_unavailable",
      retry_after: 30,
      upstream_status: 502,
    });
  });

  it("Modal network error → 503 fail-closed", async () => {
    mockModal(() => {
      throw new TypeError("fetch failed");
    });
    const r = await postEmbed("sk-recon-pro", { texts: ["fn x() {}"] });
    expect(r.status).toBe(503);
    expect(r.body).toMatchObject({
      error: "embed_service_unavailable",
      retry_after: 30,
    });
  });

  it("Modal returns wrong-shape JSON → 503 fail-closed", async () => {
    mockModal(() =>
      Promise.resolve(
        new Response(JSON.stringify({ vectors: "not a list" }), {
          status: 200,
        }),
      ),
    );
    const r = await postEmbed("sk-recon-pro", { texts: ["fn x() {}"] });
    expect(r.status).toBe(503);
  });
});
