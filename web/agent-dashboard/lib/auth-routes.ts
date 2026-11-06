// What the three /auth route files share: the settings, or a plain 503 when there are none. A dashboard that is not
// configured lets nobody in; it never falls back to open.

import { authConfig, ConfigError, type AuthConfig } from "./config";

export function configOrNull(): AuthConfig | null {
  try {
    return authConfig();
  } catch (error) {
    if (error instanceof ConfigError) {
      console.error(`dashboard: ${error.message}`);
      return null;
    }
    throw error;
  }
}

export function notConfigured(): Response {
  return new Response("The dashboard is not configured.", {
    status: 503,
    headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" },
  });
}
