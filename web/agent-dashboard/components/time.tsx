import { ago, stamp } from "@/lib/format";

/** A moment as "3m ago", with the date and time in UTC when pointed at. */
export function When({ ms, now }: { ms: number; now: number }) {
  return (
    <time dateTime={new Date(ms).toISOString()} title={stamp(ms)}>
      {ago(ms, now)}
    </time>
  );
}
