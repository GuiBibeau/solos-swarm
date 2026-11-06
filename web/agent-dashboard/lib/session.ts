// The session and the sign-in's temporary state are encrypted cookies: a JWE (A256GCM, direct key) whose key is
// derived from SESSION_SECRET, one key per purpose, so a cookie of one kind opens as no other. The browser holds
// ciphertext it cannot read or change, and nothing is kept on the server. A session carries who signed in and when it
// ends, and nothing of Vercel's: no access token is ever stored (the sign-in only needs to know who you are).

import { EncryptJWT, jwtDecrypt } from "jose";

export const SESSION_COOKIE = "agent_session";
export const OAUTH_COOKIE = "agent_oauth";
/** A session lasts a week, then Vercel is asked again. */
export const SESSION_SECONDS = 7 * 24 * 60 * 60;
/** The time between leaving for Vercel and coming back. */
export const OAUTH_SECONDS = 10 * 60;

const ISSUER = "agent-dashboard";

export type Session = {
  sub: string;
  email: string;
  name?: string;
};

/** What the callback must find again when Vercel sends the person back. */
export type OAuthState = {
  state: string;
  nonce: string;
  verifier: string;
  next: string;
};

type Purpose = "session" | "oauth";

const keys = new Map<string, Promise<Uint8Array>>();

function keyFor(secret: string, purpose: Purpose): Promise<Uint8Array> {
  const id = `${purpose}:${secret}`;
  let key = keys.get(id);
  if (!key) {
    key = derive(secret, purpose);
    keys.set(id, key);
  }
  return key;
}

async function derive(secret: string, purpose: Purpose): Promise<Uint8Array> {
  const encoder = new TextEncoder();
  const material = await crypto.subtle.importKey("raw", encoder.encode(secret), "HKDF", false, ["deriveBits"]);
  const bits = await crypto.subtle.deriveBits(
    {
      name: "HKDF",
      hash: "SHA-256",
      salt: encoder.encode("agent-dashboard/v1"),
      info: encoder.encode(purpose),
    },
    material,
    256,
  );
  return new Uint8Array(bits);
}

async function seal(
  claims: Record<string, unknown>,
  purpose: Purpose,
  secret: string,
  seconds: number,
  now: Date,
): Promise<string> {
  return new EncryptJWT(claims)
    .setProtectedHeader({ alg: "dir", enc: "A256GCM" })
    .setIssuer(ISSUER)
    .setAudience(purpose)
    .setIssuedAt(Math.floor(now.getTime() / 1000))
    .setExpirationTime(Math.floor(now.getTime() / 1000) + seconds)
    .encrypt(await keyFor(secret, purpose));
}

async function open(
  token: string | undefined,
  purpose: Purpose,
  secret: string,
  now: Date,
): Promise<Record<string, unknown> | null> {
  if (!token) return null;
  try {
    const { payload } = await jwtDecrypt(token, await keyFor(secret, purpose), {
      issuer: ISSUER,
      audience: purpose,
      currentDate: now,
      keyManagementAlgorithms: ["dir"],
      contentEncryptionAlgorithms: ["A256GCM"],
    });
    return payload;
  } catch {
    return null;
  }
}

export function sealSession(session: Session, secret: string, now = new Date()): Promise<string> {
  const claims: Record<string, unknown> = { sub: session.sub, email: session.email };
  if (session.name) claims.name = session.name;
  return seal(claims, "session", secret, SESSION_SECONDS, now);
}

export async function openSession(
  token: string | undefined,
  secret: string,
  now = new Date(),
): Promise<Session | null> {
  const claims = await open(token, "session", secret, now);
  if (!claims || typeof claims.sub !== "string" || typeof claims.email !== "string") return null;
  return {
    sub: claims.sub,
    email: claims.email,
    ...(typeof claims.name === "string" ? { name: claims.name } : {}),
  };
}

export function sealOAuth(state: OAuthState, secret: string, now = new Date()): Promise<string> {
  return seal({ ...state }, "oauth", secret, OAUTH_SECONDS, now);
}

export async function openOAuth(
  token: string | undefined,
  secret: string,
  now = new Date(),
): Promise<OAuthState | null> {
  const claims = await open(token, "oauth", secret, now);
  if (!claims) return null;
  const { state, nonce, verifier, next } = claims;
  if (
    typeof state !== "string" ||
    typeof nonce !== "string" ||
    typeof verifier !== "string" ||
    typeof next !== "string"
  ) {
    return null;
  }
  return { state, nonce, verifier, next };
}

// -- cookies ---------------------------------------------------------------------------------------------------------

export function readCookie(header: string | null | undefined, name: string): string | undefined {
  if (!header) return undefined;
  for (const part of header.split(";")) {
    const at = part.indexOf("=");
    if (at < 0) continue;
    if (part.slice(0, at).trim() === name) return part.slice(at + 1).trim();
  }
  return undefined;
}

/** A cookie no script can read, sent only to this site and only over https (http on localhost). */
export function setCookie(name: string, value: string, maxAge: number, secure: boolean): string {
  return [
    `${name}=${value}`,
    "Path=/",
    `Max-Age=${maxAge}`,
    "HttpOnly",
    "SameSite=Lax",
    ...(secure ? ["Secure"] : []),
  ].join("; ");
}

export function clearCookie(name: string, secure: boolean): string {
  return setCookie(name, "", 0, secure);
}
