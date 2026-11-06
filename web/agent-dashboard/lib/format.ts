// Small formatters for the pages. Times are shown in UTC with a relative form beside them, so what a person reads
// does not depend on the server's time zone.

export function ago(ms: number, now: number): string {
  const seconds = Math.max(0, Math.round((now - ms) / 1000));
  if (seconds < 5) return "just now";
  return `${duration(seconds * 1000)} ago`;
}

/** `45s`, `12m`, `2h 05m`, `3d 04h`. */
export function duration(ms: number): string {
  const seconds = Math.floor(ms / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(seconds / 3600);
  const days = Math.floor(seconds / 86400);
  if (days > 0) return `${days}d ${String(hours % 24).padStart(2, "0")}h`;
  if (hours > 0) return `${hours}h ${String(minutes % 60).padStart(2, "0")}m`;
  if (minutes > 0) return `${minutes}m`;
  return `${seconds}s`;
}

/** `2026-11-06 14:03 UTC`. */
export function stamp(ms: number): string {
  const text = new Date(ms).toISOString();
  return `${text.slice(0, 10)} ${text.slice(11, 16)} UTC`;
}

/** Millionths of a dollar, as the agent writes them: `$0.35`, `$12.40`, `$0.0042`. */
export function usd(microusd: number): string {
  const whole = Math.floor(microusd / 1_000_000);
  let fraction = String(microusd % 1_000_000).padStart(6, "0");
  while (fraction.length > 2 && fraction.endsWith("0")) fraction = fraction.slice(0, -1);
  return `$${whole}.${fraction}`;
}

export const STAGE_LABEL: Record<string, string> = {
  approval: "needs approval",
  answer: "needs an answer",
  blocked: "blocked",
  failed: "failed",
  queued: "queued",
  idle: "idle",
  working: "working",
  ended: "ended",
};

/** What a stage asks of the person reading, as a tone for the badge. */
export function stageTone(stage: string, outcome?: string): "attention" | "bad" | "live" | "quiet" | "good" {
  if (stage === "approval" || stage === "answer") return "attention";
  if (stage === "blocked" || stage === "failed") return "bad";
  if (stage === "working") return "live";
  if (stage === "ended") return outcome === "completed" ? "good" : "quiet";
  return "quiet";
}
