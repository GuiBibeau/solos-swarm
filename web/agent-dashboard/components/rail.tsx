"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";

/** The five destinations, in reading order. The rail is unlabelled; every item carries a tooltip and an accessible name. */
const ITEMS = [
  {
    href: "/",
    label: "Chat",
    icon: (
      <svg viewBox="0 0 18 18" fill="none" aria-hidden="true">
        <path d="M2.5 3.5h13v8.6h-7.2L4.6 15v-2.9H2.5z" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
        <path d="M5.5 7h7M5.5 9.6h4.6" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
      </svg>
    ),
  },
  {
    href: "/command",
    label: "Command",
    icon: (
      <svg viewBox="0 0 18 18" fill="none" aria-hidden="true">
        <circle cx="9" cy="9" r="6.5" stroke="currentColor" strokeWidth="1.2" />
        <circle cx="9" cy="9" r="2" fill="currentColor" />
        <path d="M9 1v2.4M9 14.6V17M1 9h2.4M14.6 9H17" stroke="currentColor" strokeWidth="1.2" />
      </svg>
    ),
  },
  {
    href: "/tickets",
    label: "Tickets",
    icon: (
      <svg viewBox="0 0 18 18" fill="none" aria-hidden="true">
        <path d="M3 4.2h12M3 9h12M3 13.8h12" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
        <circle cx="2" cy="4.2" r="0.9" fill="currentColor" />
        <circle cx="2" cy="9" r="0.9" fill="currentColor" />
        <circle cx="2" cy="13.8" r="0.9" fill="currentColor" />
      </svg>
    ),
  },
  {
    href: "/swarm",
    label: "Swarm",
    icon: (
      <svg viewBox="0 0 18 18" fill="none" aria-hidden="true">
        <circle cx="9" cy="9" r="2" fill="currentColor" />
        <circle cx="3" cy="4" r="1.4" stroke="currentColor" strokeWidth="1.1" />
        <circle cx="15" cy="4" r="1.4" stroke="currentColor" strokeWidth="1.1" />
        <circle cx="3" cy="14" r="1.4" stroke="currentColor" strokeWidth="1.1" />
        <circle cx="15" cy="14" r="1.4" stroke="currentColor" strokeWidth="1.1" />
        <path d="M4.2 5.1 7.4 7.6M13.8 5.1 10.6 7.6M4.2 12.9l3.2-2.5M13.8 12.9l-3.2-2.5" stroke="currentColor" strokeWidth="1.1" />
      </svg>
    ),
  },
  {
    href: "/jobs",
    label: "Jobs",
    icon: (
      <svg viewBox="0 0 18 18" fill="none" aria-hidden="true">
        <path d="M1.5 9h3l2-4.5 3 9 2-4.5h5" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" strokeLinecap="round" />
      </svg>
    ),
  },
];

/** The mark: the swarm's beacon — a tilted square, a ring, a live centre. */
export function Mark() {
  return (
    <svg viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <rect x="4.2" y="4.2" width="15.6" height="15.6" stroke="currentColor" strokeWidth="1.1" transform="rotate(-35 12 12)" />
      <circle cx="12" cy="12" r="5.6" stroke="currentColor" strokeWidth="1.1" />
      <circle cx="12" cy="12" r="1.8" fill="currentColor" />
    </svg>
  );
}

export function Rail({ viewer }: { viewer: { name?: string; email: string } | null }) {
  const pathname = usePathname() ?? "/";
  const current = (href: string) =>
    href === "/" ? pathname === "/" : pathname === href || pathname.startsWith(`${href}/`);
  return (
    <nav className="rail" aria-label="Command center">
      <Link href="/" className="rail-mark" aria-label="solos agent, command center">
        <Mark />
      </Link>
      <div className="rail-sep" aria-hidden="true" />
      {ITEMS.map((item) => (
        <Link
          key={item.href}
          href={item.href}
          className="rail-item"
          aria-label={item.label}
          aria-current={current(item.href) ? "page" : undefined}
        >
          {item.icon}
          <span className="tip" aria-hidden="true">
            {item.label}
          </span>
        </Link>
      ))}
      <span className="rail-grow" />
      {viewer ? (
        <form action="/auth/logout" method="post">
          <button type="submit" className="rail-item" aria-label={`Sign out ${viewer.email}`}>
            <svg viewBox="0 0 18 18" fill="none" aria-hidden="true">
              <path d="M7 3H3.8v12H7M7.5 9h8M12.6 5.8 15.8 9l-3.2 3.2" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" />
            </svg>
            <span className="tip" aria-hidden="true">
              Sign out
            </span>
          </button>
        </form>
      ) : null}
    </nav>
  );
}
