// A pretend Vercel for the sign-in tests: a key pair whose public half is the "JWKS", an ID token signer, a token
// endpoint that records what it was asked, and a cookie jar.

import { createLocalJWKSet, exportJWK, generateKeyPair, SignJWT, type JWTVerifyGetKey } from "jose";
import type { AuthConfig } from "@/lib/config";
import type { Deps } from "@/lib/auth-flow";

export const CLIENT_ID = "cl_test_client";
export const CLIENT_SECRET = "shh-client-secret";
export const ORIGIN = "https://agent.example";
export const OWNER = "you@example.com";

export const config: AuthConfig = {
  clientId: CLIENT_ID,
  clientSecret: CLIENT_SECRET,
  sessionSecret: "0123456789abcdef0123456789abcdef-secret",
  allowedEmails: [OWNER],
  publicUrl: ORIGIN,
};

const pair = await generateKeyPair("RS256", { extractable: true });
const other = await generateKeyPair("RS256", { extractable: true });
const jwk = { ...(await exportJWK(pair.publicKey)), kid: "k1", alg: "RS256", use: "sig" };

/** Vercel's key set, with only the test key in it. */
export const keys: JWTVerifyGetKey = createLocalJWKSet({ keys: [jwk] });

export type Claims = {
  iss?: string;
  aud?: string;
  sub?: string;
  email?: string | null;
  nonce?: string | null;
  exp?: number;
  name?: string;
  email_verified?: boolean;
  signWith?: "vercel" | "stranger";
};

/** An ID token as Vercel would sign it, unless a claim is changed. */
export async function idToken(nonce: string, claims: Claims = {}, now = Date.now()): Promise<string> {
  const body: Record<string, unknown> = {};
  const email = claims.email === undefined ? OWNER : claims.email;
  if (email !== null) body.email = email;
  const sent = claims.nonce === undefined ? nonce : claims.nonce;
  if (sent !== null) body.nonce = sent;
  if (claims.name) body.name = claims.name;
  if (claims.email_verified !== undefined) body.email_verified = claims.email_verified;
  const signer = new SignJWT(body)
    .setProtectedHeader({ alg: "RS256", kid: "k1" })
    .setIssuer(claims.iss ?? "https://vercel.com")
    .setAudience(claims.aud ?? CLIENT_ID)
    .setSubject(claims.sub ?? "user_123")
    .setIssuedAt(Math.floor(now / 1000))
    .setExpirationTime(claims.exp ?? Math.floor(now / 1000) + 600);
  return signer.sign((claims.signWith === "stranger" ? other : pair).privateKey);
}

export type Asked = { url: string; method?: string; body: URLSearchParams };

/** A token endpoint that answers every request with `answer` and keeps what it was asked. */
export function tokenEndpoint(answer: () => Response | Promise<Response>) {
  const asked: Asked[] = [];
  const fake = (async (url: string | URL | Request, init?: RequestInit) => {
    asked.push({
      url: String(url),
      method: init?.method,
      body: new URLSearchParams(String(init?.body ?? "")),
    });
    return answer();
  }) as typeof fetch;
  return { fetch: fake, asked };
}

export function tokens(idTokenValue: string): Response {
  return Response.json({
    access_token: "vca_never_stored",
    token_type: "Bearer",
    id_token: idTokenValue,
    expires_in: 3600,
    scope: "openid email profile",
  });
}

export function deps(over: Partial<Deps> = {}): Deps {
  return { config, keys, ...over };
}

/** The cookies a response sets, by name, with their attribute text. */
export function setCookies(response: Response): Record<string, string> {
  const jar: Record<string, string> = {};
  for (const line of response.headers.getSetCookie()) {
    const [pair = "", ...attributes] = line.split(";");
    const at = pair.indexOf("=");
    jar[pair.slice(0, at)] = [pair.slice(at + 1), ...attributes.map((attribute) => attribute.trim())].join("; ");
  }
  return jar;
}

export function cookieValue(response: Response, name: string): string | undefined {
  const text = setCookies(response)[name];
  return text?.split(";")[0] || undefined;
}
