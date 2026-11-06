import type { Metadata, Viewport } from "next";
import Link from "next/link";
import type { ReactNode } from "react";
import { currentViewer } from "@/lib/viewer";
import "./globals.css";

export const metadata: Metadata = {
  title: { default: "Agent", template: "%s · Agent" },
  description: "What the agent is doing, and what it said.",
  robots: { index: false, follow: false },
  referrer: "no-referrer",
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  colorScheme: "light dark",
  themeColor: [
    { media: "(prefers-color-scheme: light)", color: "#fafaf8" },
    { media: "(prefers-color-scheme: dark)", color: "#121311" },
  ],
};

export default async function RootLayout({ children }: { children: ReactNode }) {
  const viewer = await currentViewer();
  return (
    <html lang="en">
      <body>
        <header className="top">
          <Link href="/" className="brand">
            <span className="mark" aria-hidden="true">
              &gt;_
            </span>
            agent
          </Link>
          {viewer ? (
            <form action="/auth/logout" method="post" className="who">
              <span className="muted" title={viewer.email}>
                {viewer.name ?? viewer.email}
              </span>
              <button type="submit" className="link">
                Sign out
              </button>
            </form>
          ) : null}
        </header>
        <main>{children}</main>
      </body>
    </html>
  );
}
