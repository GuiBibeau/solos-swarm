import { describe, expect, it } from "vitest";
import { authConfig, ConfigError, isAllowed, parseAllowedEmails } from "@/lib/config";
import {
  openOAuth,
  openSession,
  readCookie,
  sealOAuth,
  sealSession,
  SESSION_SECONDS,
  setCookie,
} from "@/lib/session";
import { viewerFromCookieHeader } from "@/lib/viewer-core";
import { config, OWNER } from "./helpers";

const secret = config.sessionSecret;
const session = { sub: "user_1", email: OWNER, name: "Gui" };

describe("the session cookie", () => {
  it("opens to what was sealed, and shows nothing of it", async () => {
    const token = await sealSession(session, secret);
    expect(token).not.toContain(OWNER);
    expect(token.split(".")).toHaveLength(5);
    expect(await openSession(token, secret)).toEqual(session);
  });

  it("does not open with another secret, altered, or past its end", async () => {
    const token = await sealSession(session, secret, new Date("2026-01-01T00:00:00Z"));
    expect(await openSession(token, "another-secret-that-is-long-enough-xx", new Date("2026-01-01T00:01:00Z"))).toBeNull();
    const altered = `${token.slice(0, -2)}${token.endsWith("AA") ? "BB" : "AA"}`;
    expect(await openSession(altered, secret, new Date("2026-01-01T00:01:00Z"))).toBeNull();
    expect(await openSession(token, secret, new Date("2026-01-01T00:01:00Z"))).not.toBeNull();
    const after = new Date(Date.parse("2026-01-01T00:00:00Z") + (SESSION_SECONDS + 5) * 1000);
    expect(await openSession(token, secret, after)).toBeNull();
    for (const junk of [undefined, "", "a.b.c", "not a token"]) {
      expect(await openSession(junk, secret)).toBeNull();
    }
  });

  it("is not accepted where the sign-in's temporary cookie is, and the other way round", async () => {
    const sealed = await sealSession(session, secret);
    expect(await openOAuth(sealed, secret)).toBeNull();
    const oauth = await sealOAuth({ state: "s", nonce: "n", verifier: "v", next: "/" }, secret);
    expect(await openSession(oauth, secret)).toBeNull();
    expect(await openOAuth(oauth, secret)).toEqual({ state: "s", nonce: "n", verifier: "v", next: "/" });
  });

  it("is set only for this site, out of reach of scripts, and over https", () => {
    expect(setCookie("c", "v", 60, true)).toBe("c=v; Path=/; Max-Age=60; HttpOnly; SameSite=Lax; Secure");
    expect(setCookie("c", "v", 60, false)).not.toContain("Secure");
    expect(readCookie("a=1; agent_session=xyz; b=2", "agent_session")).toBe("xyz");
    expect(readCookie("a=1", "agent_session")).toBeUndefined();
    expect(readCookie(null, "agent_session")).toBeUndefined();
  });
});

describe("who is let in", () => {
  it("reads the allowlist trimmed, in lower case, from commas or spaces", () => {
    expect(parseAllowedEmails(" A@x.com, b@Y.com  c@z.com ,, ")).toEqual(["a@x.com", "b@y.com", "c@z.com"]);
    expect(parseAllowedEmails(undefined)).toEqual([]);
    expect(parseAllowedEmails("")).toEqual([]);
  });

  it("admits an address on the list in any case and nobody when the list is empty", () => {
    expect(isAllowed("YOU@EXAMPLE.COM", [OWNER])).toBe(true);
    expect(isAllowed(" you@example.com ", [OWNER])).toBe(true);
    expect(isAllowed("other@gmail.com", [OWNER])).toBe(false);
    expect(isAllowed(OWNER, [])).toBe(false);
  });

  it("makes a viewer of a good session only while its address is still allowed", async () => {
    const cookie = `agent_session=${await sealSession(session, secret)}`;
    expect(await viewerFromCookieHeader(cookie, config)).toMatchObject({ email: OWNER, name: "Gui" });
    const removed = { ...config, allowedEmails: ["someone-else@example.com"] };
    expect(await viewerFromCookieHeader(cookie, removed)).toBeNull();
    expect(await viewerFromCookieHeader(null, config)).toBeNull();
    expect(await viewerFromCookieHeader("agent_session=junk", config)).toBeNull();
  });
});

describe("the settings", () => {
  const env = {
    VERCEL_APP_CLIENT_ID: "cl_1",
    VERCEL_APP_CLIENT_SECRET: "secret",
    SESSION_SECRET: "x".repeat(32),
    ALLOWED_EMAILS: OWNER,
    PUBLIC_URL: "https://agent.example.com",
  };

  it("read the documented variables", () => {
    expect(authConfig(env)).toEqual({
      clientId: "cl_1",
      clientSecret: "secret",
      sessionSecret: "x".repeat(32),
      allowedEmails: [OWNER],
      publicUrl: "https://agent.example.com",
    });
  });

  it("name what is missing, and refuse a weak secret, an empty allowlist and a PUBLIC_URL that is not an origin", () => {
    const without = (name: string) => ({ ...env, [name]: "" });
    for (const name of Object.keys(env)) {
      expect(() => authConfig(without(name)), name).toThrow(ConfigError);
      expect(() => authConfig(without(name)), name).toThrow(name);
    }
    expect(() => authConfig({ ...env, SESSION_SECRET: "short" })).toThrow(/32 characters/);
    for (const bad of ["http://agent.example.com", "https://agent.example.com/x", "agent.example.com", "https://a@b.com"]) {
      expect(() => authConfig({ ...env, PUBLIC_URL: bad }), bad).toThrow(ConfigError);
    }
    expect(authConfig({ ...env, PUBLIC_URL: "http://localhost:3000" }).publicUrl).toBe("http://localhost:3000");
    expect(authConfig({ ...env, PUBLIC_URL: "https://agent.example.com/" }).publicUrl).toBe("https://agent.example.com");
  });
});
