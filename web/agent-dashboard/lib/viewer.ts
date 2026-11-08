// The viewer of the request being served: read from the session cookie, or none. Every page that shows data begins
// with requireViewer(), whatever the proxy did before it.

import { cookies } from "next/headers";
import { redirect } from "next/navigation";
import { authConfig, ConfigError, type AuthConfig } from "./config";
import { FIXTURES } from "./fixtures";
import { SESSION_COOKIE } from "./session";
import { viewerFromCookieHeader, type Viewer } from "./viewer-core";

export type { Viewer } from "./viewer-core";

const PREVIEW = { email: "preview@localhost" } as Viewer;

/** The viewer of this request, or null: signed out, expired, not allowed, or the dashboard is not configured. */
export async function currentViewer(): Promise<Viewer | null> {
  if (FIXTURES) return PREVIEW;
  let config: AuthConfig;
  try {
    config = authConfig();
  } catch (error) {
    if (error instanceof ConfigError) return null;
    throw error;
  }
  const jar = await cookies();
  const value = jar.get(SESSION_COOKIE)?.value;
  return viewerFromCookieHeader(value ? `${SESSION_COOKIE}=${value}` : null, config);
}

/** The viewer, or the sign-in page. */
export async function requireViewer(): Promise<Viewer> {
  const viewer = await currentViewer();
  if (!viewer) redirect("/login");
  return viewer;
}
