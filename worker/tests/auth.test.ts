import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { SELF, resetDb } from "./setup";

function cookieValue(setCookie: string, name: string): string {
  const part = setCookie
    .split(",")
    .map((s) => s.trim())
    .find((s) => s.startsWith(`${name}=`));
  if (!part) throw new Error(`missing cookie ${name}: ${setCookie}`);
  return part.slice(name.length + 1).split(";")[0];
}

function redirectParam(location: string, name: string): string | null {
  return new URL(location).searchParams.get(name);
}

function mockGitHub(): void {
  const realFetch = globalThis.fetch;
  vi.stubGlobal(
    "fetch",
    async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const url =
        typeof input === "string"
          ? input
          : input instanceof URL
            ? input.toString()
            : input.url;
      if (url === "https://github.com/login/oauth/access_token") {
        const body = JSON.parse(String(init?.body ?? "{}")) as {
          code?: string;
        };
        if (body.code !== "good-code") {
          return new Response(JSON.stringify({ error: "bad_verification_code" }), {
            status: 400,
            headers: { "Content-Type": "application/json" },
          });
        }
        return new Response(JSON.stringify({ access_token: "gh-token" }), {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      }
      if (url === "https://api.github.com/user") {
        return new Response(
          JSON.stringify({
            id: 123456,
            login: "octo",
            email: "octo@example.test",
            avatar_url: "https://avatars.test/octo.png",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        );
      }
      return realFetch(input, init);
    },
  );
}

describe("GitHub OAuth auth", () => {
  beforeEach(async () => {
    await resetDb();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("sets and validates OAuth state, then clears it on callback", async () => {
    mockGitHub();
    const start = await SELF.fetch("http://localhost/v1/auth/github", {
      redirect: "manual",
    });
    expect(start.status).toBe(302);
    const setCookie = start.headers.get("set-cookie") ?? "";
    const state = cookieValue(setCookie, "__Host-oauth-state");
    const location = start.headers.get("location") ?? "";
    expect(redirectParam(location, "state")).toBe(state);

    const callback = await SELF.fetch(
      `http://localhost/v1/auth/github/callback?code=good-code&state=${state}`,
      {
        headers: { Cookie: `__Host-oauth-state=${state}` },
        redirect: "manual",
      },
    );
    expect(callback.status).toBe(302);
    const callbackCookies = callback.headers.get("set-cookie") ?? "";
    expect(callbackCookies).toContain("__Host-session=");
    expect(callbackCookies).toContain("__Host-oauth-state=;");
  });

  it("rejects callback when OAuth state is missing or mismatched", async () => {
    const missing = await SELF.fetch(
      "http://localhost/v1/auth/github/callback?code=good-code",
      { headers: { Cookie: "__Host-oauth-state=abc" } },
    );
    expect(missing.status).toBe(400);
    expect(await missing.json()).toMatchObject({ error: "Invalid OAuth state" });

    const mismatched = await SELF.fetch(
      "http://localhost/v1/auth/github/callback?code=good-code&state=abc",
      { headers: { Cookie: "__Host-oauth-state=def" } },
    );
    expect(mismatched.status).toBe(400);
    expect(mismatched.headers.get("set-cookie")).toContain("__Host-oauth-state=;");
  });

  it("ignores spoofed forwarded hosts but accepts configured proxy hosts", async () => {
    const spoofed = await SELF.fetch("http://localhost/v1/auth/github", {
      headers: {
        "X-Forwarded-Host": "evil.example",
        "X-Forwarded-Proto": "https",
      },
      redirect: "manual",
    });
    const spoofedRedirectUri = redirectParam(
      spoofed.headers.get("location") ?? "",
      "redirect_uri",
    );
    expect(spoofedRedirectUri).toBe(
      "http://localhost/v1/auth/github/callback",
    );

    const allowed = await SELF.fetch("http://localhost/v1/auth/github", {
      headers: {
        "X-Forwarded-Host": "localhost:8788",
        "X-Forwarded-Proto": "http",
      },
      redirect: "manual",
    });
    const allowedRedirectUri = redirectParam(
      allowed.headers.get("location") ?? "",
      "redirect_uri",
    );
    expect(allowedRedirectUri).toBe(
      "http://localhost:8788/v1/auth/github/callback",
    );
  });
});
