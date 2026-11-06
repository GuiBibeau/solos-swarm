// Who is looking. A Viewer exists only as the result of a session that opened and whose email is allowed *now*, so
// taking an address out of ALLOWED_EMAILS ends that person's access on their next request, session or not. The data
// functions in blob.ts take a Viewer, which is how "every read is behind the check" is a type and not a habit.
// (This half has no Next.js in it, so the proxy and the tests can use it; viewer.ts is the request-scoped half.)

import { isAllowed, type AuthConfig } from "./config";
import { openSession, readCookie, SESSION_COOKIE } from "./session";

declare const viewerBrand: unique symbol;

export type Viewer = {
  readonly email: string;
  readonly name?: string;
  readonly [viewerBrand]: true;
};

/** The session in a Cookie header, if it opens and its email is allowed. */
export async function viewerFromCookieHeader(
  header: string | null | undefined,
  config: AuthConfig,
  now = new Date(),
): Promise<Viewer | null> {
  const session = await openSession(readCookie(header, SESSION_COOKIE), config.sessionSecret, now);
  if (!session || !isAllowed(session.email, config.allowedEmails)) return null;
  return { email: session.email, ...(session.name ? { name: session.name } : {}) } as Viewer;
}
