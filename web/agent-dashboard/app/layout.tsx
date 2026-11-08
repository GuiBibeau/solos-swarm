import type { Metadata, Viewport } from "next";
import { Geist, JetBrains_Mono } from "next/font/google";
import type { ReactNode } from "react";
import { Rail } from "@/components/rail";
import { currentViewer } from "@/lib/viewer";
import "./globals.css";

const sans = Geist({ subsets: ["latin"], variable: "--font-sans", display: "swap" });
const mono = JetBrains_Mono({ subsets: ["latin"], variable: "--font-mono", display: "swap" });

export const metadata: Metadata = {
  title: { default: "Command", template: "%s · Command" },
  description: "The swarm's command center: what it is working on, what it said, and what it spent.",
  robots: { index: false, follow: false },
  referrer: "no-referrer",
};

export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  colorScheme: "dark",
  themeColor: "#09090b",
};

export default async function RootLayout({ children }: { children: ReactNode }) {
  const viewer = await currentViewer();
  return (
    <html lang="en" className={`${sans.variable} ${mono.variable}`}>
      <body>
        <div className="shell">
          <Rail viewer={viewer} />
          <main className="canvas">{children}</main>
        </div>
      </body>
    </html>
  );
}
