// The first check, before anything renders: no valid session, no page. This is the optimistic one (it only opens the
// session cookie); the pages and the data reads check again on the server (lib/viewer.ts), so a mistake in the matcher
// below cannot open a page. It also gives every response its Content-Security-Policy, with a nonce per request, as
// https://nextjs.org/docs/app/guides/content-security-policy describes.

import { NextResponse, type NextRequest } from "next/server";
import { authConfig, ConfigError } from "@/lib/config";
import { viewerFromCookieHeader } from "@/lib/viewer-core";

const PUBLIC = (pathname: string) => pathname === "/login" || pathname.startsWith("/auth/");

export function contentSecurityPolicy(nonce: string, dev: boolean): string {
  return [
    "default-src 'self'",
    `script-src 'self' 'nonce-${nonce}' 'strict-dynamic'${dev ? " 'unsafe-eval'" : ""}`,
    // React's inline style attributes cannot carry a nonce; a style cannot run code.
    "style-src 'self' 'unsafe-inline'",
    // A model's markdown can name any image; none is fetched (the renderer links it instead), and none is allowed.
    "img-src 'self' data:",
    "font-src 'self'",
    "connect-src 'self'",
    "object-src 'none'",
    "base-uri 'none'",
    "form-action 'self'",
    "frame-ancestors 'none'",
  ].join("; ");
}

export async function proxy(request: NextRequest): Promise<NextResponse> {
  const { pathname, search } = request.nextUrl;
  let settings;
  try {
    settings = authConfig();
  } catch (error) {
    if (!(error instanceof ConfigError)) throw error;
    console.error(`dashboard: ${error.message}`);
    // Not configured: nobody is let in, and nothing says why beyond this.
    return new NextResponse("The dashboard is not configured.", {
      status: 503,
      headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" },
    });
  }

  if (!PUBLIC(pathname)) {
    const viewer = await viewerFromCookieHeader(request.headers.get("cookie"), settings);
    if (!viewer) {
      const login = new URL("/login", settings.publicUrl);
      if (request.method === "GET" && pathname !== "/") {
        login.searchParams.set("next", `${pathname}${search}`);
      }
      return NextResponse.redirect(login, 303);
    }
  }

  const nonce = Buffer.from(crypto.randomUUID()).toString("base64");
  const policy = contentSecurityPolicy(nonce, process.env.NODE_ENV === "development");
  const headers = new Headers(request.headers);
  headers.set("x-nonce", nonce);
  headers.set("content-security-policy", policy);
  const response = NextResponse.next({ request: { headers } });
  response.headers.set("content-security-policy", policy);
  response.headers.set("cache-control", "private, no-store");
  return response;
}

export const config = {
  // Everything but the build's own static files and the icon.
  matcher: ["/((?!_next/static|_next/image|favicon.ico|icon.svg).*)"],
};
