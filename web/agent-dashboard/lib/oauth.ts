// Sign in with Vercel: OAuth 2.0 authorization code with PKCE (S256), `state` and `nonce`, then OpenID Connect's ID
// token for who signed in. Followed from https://vercel.com/docs/sign-in-with-vercel/getting-started and
// .../authorization-server-api and .../tokens, which say:
//
//   authorize:  https://vercel.com/oauth/authorize  with client_id, redirect_uri, response_type=code, scope,
//               state, nonce, code_challenge and code_challenge_method=S256 (all required but state and nonce,
//               which this sends anyway)
//   token:      POST https://api.vercel.com/login/oauth/token, a form with grant_type=authorization_code,
//               client_id, client_secret, code, code_verifier and the same redirect_uri
//   id token:   a JWT signed by the key set at https://vercel.com/.well-known/jwks (RS256), iss https://vercel.com,
//               aud the app's client id, with `email` when the scope has it and `nonce` as sent
//
// The scopes are `openid email`, and `profile` for a name to greet by. No `offline_access`: no refresh token is
// wanted. The access token the exchange also returns is never read, stored or sent anywhere.

import { createRemoteJWKSet, jwtVerify, type JWTVerifyGetKey } from "jose";

export const AUTHORIZE_URL = "https://vercel.com/oauth/authorize";
export const TOKEN_URL = "https://api.vercel.com/login/oauth/token";
export const JWKS_URL = "https://vercel.com/.well-known/jwks";
export const ISSUER = "https://vercel.com";
export const SCOPE = "openid email profile";

export class OAuthError extends Error {
  constructor(
    readonly code: "token" | "identity" | "nonce",
    message: string,
  ) {
    super(message);
  }
}

function base64url(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("base64url");
}

/** 256 random bits, as URL-safe text: a `state`, a `nonce` or a PKCE verifier (43 characters, RFC 7636). */
export function randomToken(): string {
  return base64url(crypto.getRandomValues(new Uint8Array(32)));
}

/** The S256 challenge of a verifier: base64url of its SHA-256. */
export async function challengeOf(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return base64url(new Uint8Array(digest));
}

export function authorizeUrl(params: {
  clientId: string;
  redirectUri: string;
  state: string;
  nonce: string;
  challenge: string;
}): string {
  const query = new URLSearchParams({
    client_id: params.clientId,
    redirect_uri: params.redirectUri,
    response_type: "code",
    scope: SCOPE,
    state: params.state,
    nonce: params.nonce,
    code_challenge: params.challenge,
    code_challenge_method: "S256",
  });
  return `${AUTHORIZE_URL}?${query.toString()}`;
}

/** Trade the code for tokens. Only the ID token is returned: the rest is not wanted. */
export async function exchangeCode(
  params: {
    clientId: string;
    clientSecret: string;
    redirectUri: string;
    code: string;
    verifier: string;
  },
  fetchImpl: typeof fetch = fetch,
): Promise<string> {
  const response = await fetchImpl(TOKEN_URL, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: params.clientId,
      client_secret: params.clientSecret,
      code: params.code,
      code_verifier: params.verifier,
      redirect_uri: params.redirectUri,
    }),
    redirect: "error",
    cache: "no-store",
  });
  if (!response.ok) {
    // RFC 6749 §5.2 error fields name the cause and carry no secret; anything else in the body is dropped.
    const failure: unknown = await response.json().catch(() => null);
    const field = (name: string): string | undefined => {
      const value = typeof failure === "object" && failure !== null ? (failure as Record<string, unknown>)[name] : undefined;
      return typeof value === "string" ? value.slice(0, 200) : undefined;
    };
    const why = [field("error"), field("error_description")].filter(Boolean).join(": ");
    throw new OAuthError("token", `the token endpoint answered ${response.status}${why ? ` (${why})` : ""}`);
  }
  const body: unknown = await response.json().catch(() => null);
  const idToken = typeof body === "object" && body !== null ? (body as { id_token?: unknown }).id_token : undefined;
  if (typeof idToken !== "string" || idToken.length === 0) {
    throw new OAuthError("token", "the token endpoint sent no id_token");
  }
  return idToken;
}

export type Identity = { sub: string; email: string; name?: string };

const vercelKeys = createRemoteJWKSet(new URL(JWKS_URL));

/**
 * Check an ID token's signature against Vercel's keys, its issuer, audience and expiry, and that it answers
 * this sign-in's nonce, then read who it names. `keys` is Vercel's key set unless a test gives its own.
 */
export async function verifyIdToken(
  idToken: string,
  expected: { clientId: string; nonce: string },
  keys: JWTVerifyGetKey = vercelKeys,
  now = new Date(),
): Promise<Identity> {
  let payload;
  try {
    ({ payload } = await jwtVerify(idToken, keys, {
      issuer: ISSUER,
      audience: expected.clientId,
      algorithms: ["RS256"],
      currentDate: now,
      requiredClaims: ["sub", "exp", "iat"],
    }));
  } catch (error) {
    throw new OAuthError("identity", `the id token did not verify: ${(error as Error).message}`);
  }
  if (typeof payload.nonce !== "string" || payload.nonce !== expected.nonce) {
    throw new OAuthError("nonce", "the id token's nonce is not this sign-in's");
  }
  const email = payload.email;
  if (typeof email !== "string" || !email.includes("@")) {
    throw new OAuthError("identity", "the id token has no email");
  }
  // Not among the claims Vercel lists, but if it ever says the address is unverified, it is not trusted.
  if (payload.email_verified === false) {
    throw new OAuthError("identity", "the id token's email is not verified");
  }
  const sub = payload.sub;
  if (typeof sub !== "string" || sub.length === 0) {
    throw new OAuthError("identity", "the id token has no subject");
  }
  return {
    sub,
    email,
    ...(typeof payload.name === "string" && payload.name ? { name: payload.name } : {}),
  };
}
