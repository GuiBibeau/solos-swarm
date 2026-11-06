// The three routes of the sign-in as plain functions of a Request: /auth/login sends the person to Vercel,
// /auth/callback takes them back and decides whether they come in, /auth/logout ends the session. The route files are
// one line each, so everything that matters is here and is tested without a server.

import { timingSafeEqual } from "node:crypto";
import type { JWTVerifyGetKey } from "jose";
import { isAllowed, type AuthConfig } from "./config";
import {
  authorizeUrl,
  challengeOf,
  exchangeCode,
  OAuthError,
  randomToken,
  verifyIdToken,
} from "./oauth";
import {
  clearCookie,
  OAUTH_COOKIE,
  OAUTH_SECONDS,
  readCookie,
  SESSION_COOKIE,
  SESSION_SECONDS,
  sealOAuth,
  sealSession,
  setCookie,
  openOAuth,
} from "./session";

export type Deps = {
  config: AuthConfig;
  fetch?: typeof fetch;
  /** Vercel's key set, unless a test has its own. */
  keys?: JWTVerifyGetKey;
  now?: () => Date;
};

/** Where `/login?error=` can send a person back to, so a page never prints what a URL says. */
export type LoginError = "state" | "provider" | "token" | "identity" | "forbidden" | "config";

export function callbackUrl(config: AuthConfig): string {
  return `${config.publicUrl}/auth/callback`;
}

function secure(config: AuthConfig): boolean {
  return config.publicUrl.startsWith("https://");
}

/** A path on this site, or "/". Never a URL, so a sign-in cannot end somewhere else. */
export function safeNext(next: string | null | undefined): string {
  if (!next || !next.startsWith("/") || next.startsWith("//") || next.includes("\\") || /[\u0000-\u001f]/.test(next)) {
    return "/";
  }
  return next;
}

function redirect(location: string, cookies: string[] = []): Response {
  const headers = new Headers({ location, "cache-control": "no-store" });
  for (const cookie of cookies) headers.append("set-cookie", cookie);
  return new Response(null, { status: 303, headers });
}

function loginError(config: AuthConfig, error: LoginError, cookies: string[] = []): Response {
  return redirect(`${config.publicUrl}/login?error=${error}`, [...cookies, clearCookie(OAUTH_COOKIE, secure(config))]);
}

/** Equal text, in time that does not say where they differ. */
export function sameText(a: string, b: string): boolean {
  const left = Buffer.from(a);
  const right = Buffer.from(b);
  return left.length === right.length && timingSafeEqual(left, right);
}

/** GET /auth/login: remember what the callback must check, and go to Vercel. */
export async function startLogin(request: Request, deps: Deps): Promise<Response> {
  const { config } = deps;
  const now = deps.now?.() ?? new Date();
  const verifier = randomToken();
  const state = randomToken();
  const nonce = randomToken();
  const next = safeNext(new URL(request.url).searchParams.get("next"));
  const sealed = await sealOAuth({ state, nonce, verifier, next }, config.sessionSecret, now);
  const location = authorizeUrl({
    clientId: config.clientId,
    redirectUri: callbackUrl(config),
    state,
    nonce,
    challenge: await challengeOf(verifier),
  });
  return redirect(location, [setCookie(OAUTH_COOKIE, sealed, OAUTH_SECONDS, secure(config))]);
}

/** GET /auth/callback: the checks, in the order that spends nothing before it must. */
export async function finishLogin(request: Request, deps: Deps): Promise<Response> {
  const { config } = deps;
  const now = deps.now?.() ?? new Date();
  const url = new URL(request.url);
  const cookies = request.headers.get("cookie");
  const pending = await openOAuth(readCookie(cookies, OAUTH_COOKIE), config.sessionSecret, now);

  // The person said no, or Vercel did: nothing came with it to check.
  if (url.searchParams.has("error")) {
    console.error(`sign-in: Vercel answered error=${url.searchParams.get("error")}`);
    return loginError(config, "provider");
  }
  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state");
  // State first: a callback that is not the answer to a sign-in this browser began is dropped before any request.
  if (!pending || !code || !state || !sameText(state, pending.state)) {
    console.error("sign-in: the state did not match, or the sign-in's cookie is missing or expired");
    return loginError(config, "state");
  }

  let identity;
  try {
    const idToken = await exchangeCode(
      {
        clientId: config.clientId,
        clientSecret: config.clientSecret,
        redirectUri: callbackUrl(config),
        code,
        verifier: pending.verifier,
      },
      deps.fetch,
    );
    identity = await verifyIdToken(idToken, { clientId: config.clientId, nonce: pending.nonce }, deps.keys, now);
  } catch (error) {
    const reason = error instanceof OAuthError ? error : null;
    console.error(`sign-in: ${(error as Error).message}`);
    return loginError(config, reason?.code === "token" ? "token" : "identity");
  }

  if (!isAllowed(identity.email, config.allowedEmails)) {
    // Which address was refused is for the log, not for the page.
    console.error(`sign-in: ${identity.email} is not in ALLOWED_EMAILS`);
    return loginError(config, "forbidden");
  }

  const session = await sealSession(
    { sub: identity.sub, email: identity.email.toLowerCase(), ...(identity.name ? { name: identity.name } : {}) },
    config.sessionSecret,
    now,
  );
  return redirect(`${config.publicUrl}${safeNext(pending.next)}`, [
    setCookie(SESSION_COOKIE, session, SESSION_SECONDS, secure(config)),
    clearCookie(OAUTH_COOKIE, secure(config)),
  ]);
}

/** POST /auth/logout: the cookie is the whole session, so clearing it ends it. */
export function signOut(config: AuthConfig): Response {
  return redirect(`${config.publicUrl}/login`, [
    clearCookie(SESSION_COOKIE, secure(config)),
    clearCookie(OAUTH_COOKIE, secure(config)),
  ]);
}
