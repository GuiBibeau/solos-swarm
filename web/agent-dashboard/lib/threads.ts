// A web conversation's key: `wc_` and 32 hex digits. Client-safe, so the page, the API and the live chat agree.

import type { IndexRow } from "./types";

export const THREAD_RE = /^wc_[0-9a-f]{32}$/;

/** The thread id in a published conversation's source link, when it is a web one. */
export function threadOf(row: IndexRow): string | null {
  if (row.source !== "web" || !row.source_url) return null;
  try {
    return new URL(row.source_url).searchParams.get("t");
  } catch {
    return null;
  }
}
