import { Hono } from "hono";
import { describe, expect, it } from "vitest";
import { rateLimit } from "../src/middleware/ratelimit";
import type { Env } from "../src/types";

function makeApp() {
  const app = new Hono<{ Bindings: Env }>();
  app.use("*", rateLimit("RL_LICENSE", () => "test-key", 60));
  app.get("/", (c) => c.json({ ok: true }));
  return app;
}

describe("rateLimit missing binding behavior", () => {
  it("fails open for local/test requests", async () => {
    const resp = await makeApp().request(
      "http://localhost/",
      {},
      {} as Partial<Env> as Env,
    );
    expect(resp.status).toBe(200);
    await expect(resp.json()).resolves.toEqual({ ok: true });
  });

  it("fails closed for production requests", async () => {
    const resp = await makeApp().request(
      "https://api.example.com/",
      {},
      {} as Partial<Env> as Env,
    );
    expect(resp.status).toBe(503);
    await expect(resp.json()).resolves.toMatchObject({
      error: "rate limit unavailable",
    });
  });
});
