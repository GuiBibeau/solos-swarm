// The dashboard's settings, read from the environment when they are needed and never at import time, so `next build`
// needs none of them. Anything missing or malformed is an error, and every caller fails closed on it.

export type AuthConfig = {
  clientId: string;
  clientSecret: string;
  sessionSecret: string;
  allowedEmails: string[];
  /** The dashboard's origin, with no trailing slash. */
  publicUrl: string;
};

export class ConfigError extends Error {}

type Env = Record<string, string | undefined>;

/** `a@x.com, B@y.com` becomes `["a@x.com", "b@y.com"]`: trimmed, lower case, no blanks. */
export function parseAllowedEmails(value: string | undefined): string[] {
  return (value ?? "")
    .split(/[,\s]+/)
    .map((email) => email.trim().toLowerCase())
    .filter((email) => email.length > 0);
}

export function isAllowed(email: string, allowed: string[]): boolean {
  return allowed.includes(email.trim().toLowerCase());
}

export function authConfig(env: Env = process.env): AuthConfig {
  const missing: string[] = [];
  const need = (name: string): string => {
    const value = env[name]?.trim();
    if (!value) missing.push(name);
    return value ?? "";
  };
  const clientId = need("VERCEL_APP_CLIENT_ID");
  const clientSecret = need("VERCEL_APP_CLIENT_SECRET");
  const sessionSecret = need("SESSION_SECRET");
  const allowedEmails = parseAllowedEmails(env.ALLOWED_EMAILS);
  if (allowedEmails.length === 0) missing.push("ALLOWED_EMAILS");
  const publicUrl = need("PUBLIC_URL");
  if (missing.length > 0) {
    throw new ConfigError(`missing environment: ${missing.join(", ")}`);
  }
  if (sessionSecret.length < 32) {
    throw new ConfigError("SESSION_SECRET must be at least 32 characters (openssl rand -base64 32)");
  }
  let origin: URL;
  try {
    origin = new URL(publicUrl);
  } catch {
    throw new ConfigError("PUBLIC_URL is not a URL");
  }
  const local = origin.hostname === "localhost" || origin.hostname === "127.0.0.1";
  if (origin.protocol !== "https:" && !(local && origin.protocol === "http:")) {
    throw new ConfigError("PUBLIC_URL must be https (http only for localhost)");
  }
  if (origin.pathname !== "/" || origin.search || origin.hash || origin.username) {
    throw new ConfigError("PUBLIC_URL must be an origin: scheme, host and port only");
  }
  return {
    clientId,
    clientSecret,
    sessionSecret,
    allowedEmails,
    publicUrl: origin.origin,
  };
}
