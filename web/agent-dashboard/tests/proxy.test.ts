import { NextRequest } from "next/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { contentSecurityPolicy, proxy } from "@/proxy";
import { sealSession } from "@/lib/session";
import { config, ORIGIN, OWNER } from "./helpers";

beforeEach(() => {
  vi.stubEnv("VERCEL_APP_CLIENT_ID", config.clientId);
  vi.stubEnv("VERCEL_APP_CLIENT_SECRET", config.clientSecret);
  vi.stubEnv("SESSION_SECRET", config.sessionSecret);
  vi.stubEnv("ALLOWED_EMAILS", OWNER);
  vi.stubEnv("PUBLIC_URL", ORIGIN);
  vi.spyOn(console, "error").mockImplementation(() => {});
});
afterEach(() => {
  vi.unstubAllEnvs();
  vi.restoreAllMocks();
});

function request(path: string, cookie?: string, method = "GET") {
  return new NextRequest(`${ORIGIN}${path}`, { method, headers: cookie ? { cookie } : {} });
}

describe("the proxy", () => {
  it("sends a visitor with no session to the sign-in page, remembering where they were going", async () => {
    const response = await proxy(request("/tickets/tk_abc?x=1"));
    expect(response.status).toBe(303);
    expect(response.headers.get("location")).toBe(`${ORIGIN}/login?next=%2Ftickets%2Ftk_abc%3Fx%3D1`);
    expect((await proxy(request("/"))).headers.get("location")).toBe(`${ORIGIN}/login`);
  });

  it("sends a visitor whose session is junk, or belongs to an address no longer allowed, the same way", async () => {
    const junk = await proxy(request("/", "agent_session=junk"));
    expect(junk.status).toBe(303);
    const cookie = `agent_session=${await sealSession({ sub: "u", email: "mallory@example.com" }, config.sessionSecret)}`;
    expect((await proxy(request("/", cookie))).status).toBe(303);
  });

  it("lets the owner's session through, with a nonce and a policy on the response", async () => {
    const cookie = `agent_session=${await sealSession({ sub: "u", email: OWNER }, config.sessionSecret)}`;
    const response = await proxy(request("/tickets/tk_abc", cookie));
    expect(response.status).toBe(200);
    expect(response.headers.get("x-middleware-next")).toBe("1");
    const policy = response.headers.get("content-security-policy") ?? "";
    expect(policy).toMatch(/script-src 'self' 'nonce-[A-Za-z0-9+/=]+' 'strict-dynamic'/);
    expect(policy).toContain("frame-ancestors 'none'");
    expect(policy).toContain("img-src 'self' data:");
    expect(policy).not.toContain("unsafe-eval");
    expect(response.headers.get("cache-control")).toBe("private, no-store");
    const again = await proxy(request("/", cookie));
    expect(again.headers.get("content-security-policy")).not.toBe(policy);
  });

  it("leaves the sign-in pages open and still gives them a policy", async () => {
    for (const path of ["/login", "/login?error=state", "/auth/login", "/auth/callback?code=c&state=s", "/auth/logout"]) {
      const response = await proxy(request(path, undefined, path === "/auth/logout" ? "POST" : "GET"));
      expect(response.status, path).toBe(200);
      expect(response.headers.get("content-security-policy"), path).toBeTruthy();
    }
  });

  it("lets nobody in when the dashboard is not configured", async () => {
    vi.stubEnv("SESSION_SECRET", "");
    for (const path of ["/", "/login", "/tickets/tk_abc"]) {
      const response = await proxy(request(path));
      expect(response.status, path).toBe(503);
    }
  });

  it("allows the dev server's eval only in development", () => {
    expect(contentSecurityPolicy("n", true)).toContain("'unsafe-eval'");
    expect(contentSecurityPolicy("n", false)).not.toContain("'unsafe-eval'");
  });
});
