"use client";

import { useRouter } from "next/navigation";
import { useEffect } from "react";

/** Reads the page again every `seconds` while it is on screen, so an open ticket moves without a reload. */
export function Refresh({ seconds }: { seconds: number }) {
  const router = useRouter();
  useEffect(() => {
    const timer = setInterval(() => {
      if (!document.hidden) router.refresh();
    }, seconds * 1000);
    return () => clearInterval(timer);
  }, [router, seconds]);
  return null;
}
