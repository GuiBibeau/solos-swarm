import { createHash } from "node:crypto";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { finishLogin, safeNext, signOut, startLogin } from "@/lib/auth-flow";
import { OAUTH_COOKIE, SESSION_COOKIE, openSession } from "@/lib/session";
import {
  CLIENT_ID,
  CLIENT_SECRET,
  ORIGIN,
  OWNER,
  config,
  cookieValue,
  deps,
  idToken,
  setCookies,
  tokenEndpoint,
  tokens,
  type Claims,
} from "./helpers";

beforeEach(() => {
  vi.spyOn(console, "error").mockImplementation(() => {});
});
afterEach(() => {
  vi.restoreAllMocks();
});

/** Begin a sign-in and return what Vercel would be sent and what the browser keeps. */
async function begin(next = "/tickets/tk_abc") {
  const response = await startLogin(new Request(`${ORIGIN}/auth/login?next=${encodeURIComponent(next)}`), deps());
  const authorize = new URL(response.headers.get("location") ?? "");
  const cookie = `${OAUTH_COOKIE}=${cookieValue(response, OAUTH_COOKIE)}`;
  return { response, authorize, cookie, params: authorize.searchParams };
}

function callback(cookie: string | null, query: Record<string, string>) {
  const url = new URL(`${ORIGIN}/auth/callback`);
  for (const [name, value] of Object.entries(query)) url.searchParams.set(name, value);
  return new Request(url, { headers: cookie ? { cookie } : {} });
}

/** A sign-in through the callback with `claims` on the ID token; returns the response and the token requests. */
async function signIn(claims: Claims = {}, over: { state?: string; cookie?: string | null; code?: string | null } = {}) {
  const { authorize, cookie, params } = await begin();
  const id = await idToken(params.get("nonce") ?? "", claims);
  const endpoint = tokenEndpoint(() => tokens(id));
  const query: Record<string, string> = { state: over.state ?? params.get("state") ?? "" };
  if (over.code !== null) query.code = over.code ?? "the-code";
  const response = await finishLogin(
    callback(over.cookie === undefined ? cookie : over.cookie, query),
    deps({ fetch: endpoint.fetch }),
  );
  return { response, endpoint, authorize };
}

const location = (response: Response) => response.headers.get("location") ?? "";

describe("starting a sign-in", () => {
  it("sends the person to Vercel with PKCE S256, a state, a nonce and this app's callback", async () => {
    const { response, authorize, params, cookie } = await begin();
    expect(response.status).toBe(303);
    expect(`${authorize.origin}${authorize.pathname}`).toBe("https://vercel.com/oauth/authorize");
    expect(params.get("client_id")).toBe(CLIENT_ID);
    expect(params.get("redirect_uri")).toBe(`${ORIGIN}/auth/callback`);
    expect(params.get("response_type")).toBe("code");
    expect(params.get("scope")).toBe("openid email profile");
    expect(params.get("code_challenge_method")).toBe("S256");
    expect(params.get("code_challenge")).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(params.get("state")).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(params.get("nonce")).toMatch(/^[A-Za-z0-9_-]{43}$/);
    expect(params.get("state")).not.toBe(params.get("nonce"));
    expect(cookie.length).toBeGreaterThan(60);
  });

  it("keeps what the callback needs in one encrypted, HttpOnly, Secure, SameSite=Lax cookie that lasts ten minutes", async () => {
    const { response } = await begin();
    const jar = setCookies(response);
    const attributes = jar[OAUTH_COOKIE] ?? "";
    expect(attributes).toContain("HttpOnly");
    expect(attributes).toContain("Secure");
    expect(attributes).toContain("SameSite=Lax");
    expect(attributes).toContain("Max-Age=600");
    expect(attributes).toContain("Path=/");
    // Ciphertext: neither the state nor the verifier is in it.
    const { params } = await begin();
    expect(attributes).not.toContain(params.get("state") ?? "x");
    expect(response.headers.get("cache-control")).toBe("no-store");
  });

  it("makes a new state, nonce and verifier every time", async () => {
    const first = await begin();
    const second = await begin();
    expect(first.params.get("state")).not.toBe(second.params.get("state"));
    expect(first.params.get("nonce")).not.toBe(second.params.get("nonce"));
    expect(first.params.get("code_challenge")).not.toBe(second.params.get("code_challenge"));
  });
});

describe("the callback", () => {
  it("signs in the allowed owner: a session cookie, the temporary one cleared, and back to where they were going", async () => {
    const { response, endpoint, authorize } = await signIn();
    expect(response.status).toBe(303);
    expect(location(response)).toBe(`${ORIGIN}/tickets/tk_abc`);

    const jar = setCookies(response);
    const session = jar[SESSION_COOKIE] ?? "";
    expect(session).toContain("HttpOnly");
    expect(session).toContain("Secure");
    expect(session).toContain("SameSite=Lax");
    expect(session).toContain("Path=/");
    expect(session).toContain(`Max-Age=${7 * 24 * 60 * 60}`);
    expect(jar[OAUTH_COOKIE]).toContain("Max-Age=0");
    const opened = await openSession(cookieValue(response, SESSION_COOKIE), config.sessionSecret);
    expect(opened).toMatchObject({ email: OWNER, sub: "user_123" });
    expect(session).not.toContain(OWNER);

    // One token request: the form Vercel's docs describe, with the verifier whose hash was the challenge.
    const [asked] = endpoint.asked;
    expect(endpoint.asked).toHaveLength(1);
    expect(asked?.url).toBe("https://api.vercel.com/login/oauth/token");
    expect(asked?.method).toBe("POST");
    expect(asked?.body.get("grant_type")).toBe("authorization_code");
    expect(asked?.body.get("client_id")).toBe(CLIENT_ID);
    expect(asked?.body.get("client_secret")).toBe(CLIENT_SECRET);
    expect(asked?.body.get("code")).toBe("the-code");
    expect(asked?.body.get("redirect_uri")).toBe(`${ORIGIN}/auth/callback`);
    const verifier = asked?.body.get("code_verifier") ?? "";
    expect(createHash("sha256").update(verifier).digest("base64url")).toBe(authorize.searchParams.get("code_challenge"));
  });

  it("stores no Vercel token in the session", async () => {
    const { response } = await signIn();
    const everything = [...response.headers.entries()].map(([, value]) => value).join(" ");
    expect(everything).not.toContain("vca_never_stored");
  });

  it("refuses a state that is not the one it sent, before it asks Vercel for anything", async () => {
    const { response, endpoint } = await signIn({}, { state: "somebody-elses-state" });
    expect(location(response)).toBe(`${ORIGIN}/login?error=state`);
    expect(endpoint.asked).toHaveLength(0);
    expect(setCookies(response)[SESSION_COOKIE]).toBeUndefined();
  });

  it("refuses a callback with no state, no code, or no cookie from this browser", async () => {
    for (const over of [{ state: "" }, { code: null }, { cookie: null }, { cookie: `${OAUTH_COOKIE}=garbage` }]) {
      const { response, endpoint } = await signIn({}, over);
      expect(location(response)).toBe(`${ORIGIN}/login?error=state`);
      expect(endpoint.asked).toHaveLength(0);
      expect(setCookies(response)[SESSION_COOKIE]).toBeUndefined();
    }
  });

  it("refuses a sign-in cookie that has expired", async () => {
    const { cookie, params } = await begin();
    const endpoint = tokenEndpoint(() => tokens("unused"));
    const later = () => new Date(Date.now() + 11 * 60 * 1000);
    const response = await finishLogin(
      callback(cookie, { code: "c", state: params.get("state") ?? "" }),
      deps({ fetch: endpoint.fetch, now: later }),
    );
    expect(location(response)).toBe(`${ORIGIN}/login?error=state`);
    expect(endpoint.asked).toHaveLength(0);
  });

  it("refuses an ID token whose nonce is not this sign-in's", async () => {
    for (const claims of [{ nonce: "a-replayed-nonce" }, { nonce: null }]) {
      const { response } = await signIn(claims);
      expect(location(response)).toBe(`${ORIGIN}/login?error=identity`);
      expect(setCookies(response)[SESSION_COOKIE]).toBeUndefined();
    }
  });

  it("refuses an ID token for another app, another issuer, that has expired, or that Vercel did not sign", async () => {
    const past = Math.floor(Date.now() / 1000) - 3600;
    for (const claims of [
      { aud: "cl_somebody_else" },
      { iss: "https://evil.example" },
      { exp: past },
      { signWith: "stranger" as const },
    ]) {
      const { response } = await signIn(claims);
      expect(location(response), JSON.stringify(claims)).toBe(`${ORIGIN}/login?error=identity`);
      expect(setCookies(response)[SESSION_COOKIE]).toBeUndefined();
    }
  });

  it("refuses an ID token with no email, or one marked unverified", async () => {
    for (const claims of [{ email: null }, { email: "not-an-address" }, { email_verified: false }]) {
      const { response } = await signIn(claims);
      expect(location(response), JSON.stringify(claims)).toBe(`${ORIGIN}/login?error=identity`);
    }
  });

  it("lets nobody in who is not on the allowlist, whoever Vercel says they are", async () => {
    for (const email of ["mallory@example.com", `x${OWNER}`, `${OWNER}.evil.com`, "you@example.org"]) {
      const { response } = await signIn({ email });
      expect(location(response), email).toBe(`${ORIGIN}/login?error=forbidden`);
      expect(setCookies(response)[SESSION_COOKIE]).toBeUndefined();
      expect(response.headers.get("location")).not.toContain(email);
    }
  });

  it("compares the address without regard to case, and keeps it in lower case", async () => {
    const { response } = await signIn({ email: OWNER.toUpperCase() });
    expect(location(response)).toBe(`${ORIGIN}/tickets/tk_abc`);
    const opened = await openSession(cookieValue(response, SESSION_COOKIE), config.sessionSecret);
    expect(opened?.email).toBe(OWNER);
  });

  it("answers a refused token exchange with an error and no session", async () => {
    const { authorize, cookie } = await begin();
    const failing = tokenEndpoint(() => new Response("nope", { status: 400 }));
    const response = await finishLogin(
      callback(cookie, { code: "c", state: authorize.searchParams.get("state") ?? "" }),
      deps({ fetch: failing.fetch }),
    );
    expect(location(response)).toBe(`${ORIGIN}/login?error=token`);
    expect(setCookies(response)[SESSION_COOKIE]).toBeUndefined();

    const empty = tokenEndpoint(() => Response.json({ access_token: "x" }));
    const missing = await finishLogin(
      callback(cookie, { code: "c", state: authorize.searchParams.get("state") ?? "" }),
      deps({ fetch: empty.fetch }),
    );
    expect(location(missing)).toBe(`${ORIGIN}/login?error=token`);
  });

  it("answers Vercel's own error, such as the person pressing Cancel, without asking for anything", async () => {
    const { cookie } = await begin();
    const endpoint = tokenEndpoint(() => tokens("unused"));
    const response = await finishLogin(callback(cookie, { error: "access_denied" }), deps({ fetch: endpoint.fetch }));
    expect(location(response)).toBe(`${ORIGIN}/login?error=provider`);
    expect(endpoint.asked).toHaveLength(0);
  });

  it("cannot be used twice: the temporary cookie is cleared, so the same callback finds none", async () => {
    const { response } = await signIn();
    expect(setCookies(response)[OAUTH_COOKIE]).toContain("Max-Age=0");
  });

  it("does not follow a redirect the token endpoint answers with", async () => {
    const { authorize, cookie } = await begin();
    let init: RequestInit | undefined;
    const response = await finishLogin(
      callback(cookie, { code: "c", state: authorize.searchParams.get("state") ?? "" }),
      deps({
        fetch: (async (_url: string | URL | Request, options?: RequestInit) => {
          init = options;
          return new Response(null, { status: 500 });
        }) as typeof fetch,
      }),
    );
    expect(init?.redirect).toBe("error");
    expect(location(response)).toBe(`${ORIGIN}/login?error=token`);
  });
});

describe("where a sign-in ends", () => {
  it("is a path on this site and nothing else", async () => {
    for (const next of ["//evil.example", "https://evil.example", "/\\evil.example", "evil", "/a\nb", ""]) {
      expect(safeNext(next), JSON.stringify(next)).toBe("/");
    }
    expect(safeNext(null)).toBe("/");
    expect(safeNext("/tickets/tk_1?x=1")).toBe("/tickets/tk_1?x=1");

    const { response } = await (async () => {
      const begun = await begin("//evil.example");
      const id = await idToken(begun.params.get("nonce") ?? "");
      const endpoint = tokenEndpoint(() => tokens(id));
      return {
        response: await finishLogin(
          callback(begun.cookie, { code: "c", state: begun.params.get("state") ?? "" }),
          deps({ fetch: endpoint.fetch }),
        ),
      };
    })();
    expect(location(response)).toBe(`${ORIGIN}/`);
  });
});

describe("signing out", () => {
  it("clears both cookies and goes to the sign-in page", () => {
    const response = signOut(config);
    expect(location(response)).toBe(`${ORIGIN}/login`);
    const jar = setCookies(response);
    expect(jar[SESSION_COOKIE]).toContain("Max-Age=0");
    expect(jar[OAUTH_COOKIE]).toContain("Max-Age=0");
  });
});
