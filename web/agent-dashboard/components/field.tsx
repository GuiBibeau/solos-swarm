import type { IndexRow } from "@/lib/types";

/**
 * The field: every Ticket is a node on the host's rings, and a Ticket with a running worker carries a satellite.
 * It is drawn from the published index alone, so it is deterministic, server-rendered, and honest: a node that is
 * not there is a Ticket the publisher has not seen, not a Ticket that does not exist.
 */

const GOLDEN = Math.PI * (3 - Math.sqrt(5));

function kind(row: IndexRow): "live" | "waiting" | "bad" | "open" | "ended" {
  if (!row.open) return row.outcome === "dropped" || row.outcome === "stopped" ? "bad" : "ended";
  if (row.assignment?.state === "running") return "live";
  if (row.waiting_for) return "waiting";
  return "open";
}

function shortId(id: string): string {
  return id.length > 8 ? id.slice(-6) : id;
}

export function Field({ tickets, large = false }: { tickets: IndexRow[]; large?: boolean }) {
  const shown = tickets.slice(0, large ? 40 : 22);
  const hidden = tickets.length - shown.length;
  const w = large ? 960 : 760;
  const h = large ? 460 : 320;
  const cx = w / 2;
  const cy = h / 2;
  const ringFor = (row: IndexRow): number => {
    const k = kind(row);
    if (k === "live" || k === "waiting") return large ? 92 : 64;
    if (k === "open") return large ? 150 : 104;
    return large ? 205 : 142;
  };
  const nodes = shown.map((row, i) => {
    const r = ringFor(row);
    const angle = i * GOLDEN;
    const x = cx + r * Math.cos(angle);
    const y = cy + (r * 0.86) * Math.sin(angle);
    const k = kind(row);
    const worker = row.assignment?.state === "running" ? 1 : 0;
    const wx = x + 11 * Math.cos(angle);
    const wy = y + 11 * Math.sin(angle);
    return { row, x, y, k, worker, wx, wy };
  });
  const labelled = nodes.filter((n) => n.k === "live" || n.k === "waiting" || n.k === "open").slice(0, large ? 12 : 7);

  return (
    <svg
      className="field"
      viewBox={`0 0 ${w} ${h}`}
      role="img"
      aria-label={`The swarm: ${shown.length} Tickets on the field, ${shown.filter((r) => r.open).length} open`}
    >
      <circle className="ring" cx={cx} cy={cy} r={large ? 92 : 64} />
      <circle className="ring" cx={cx} cy={cy} r={large ? 150 : 104} />
      <circle className="ring" cx={cx} cy={cy} r={large ? 205 : 142} />
      <line className="axis" x1={cx - (large ? 240 : 170)} y1={cy} x2={cx + (large ? 240 : 170)} y2={cy} />
      <line className="axis" x1={cx} y1={cy - (large ? 210 : 150)} x2={cx} y2={cy + (large ? 210 : 150)} />

      {nodes.map(({ row, x, y, k, worker, wx, wy }) => (
        <g key={row.id}>
          <title>{`${row.title} — ${row.stage}${row.assignment ? `, worker ${row.assignment.state}` : ""}`}</title>
          {worker ? <line className={`worker-link ${k === "live" ? "live" : ""}`} x1={x} y1={y} x2={wx} y2={wy} style={{ stroke: k === "live" ? "var(--ACCENT)" : "var(--LINE-STRONG)", opacity: 0.6 }} /> : null}
          <circle className={`node ${k}`} cx={x} cy={y} r={large ? (row.open ? 5 : 3.4) : row.open ? 4.4 : 3} />
          {worker ? <circle className={`worker ${k === "live" ? "live" : ""}`} cx={wx} cy={wy} r={2.3} /> : null}
        </g>
      ))}

      <g className="host">
        <rect x={cx - 4} y={cy - 4} width={8} height={8} fill="var(--INK)" transform={`rotate(-35 ${cx} ${cy})`} />
      </g>

      {labelled.map(({ row, x, y, k }) => (
        <text key={row.id} className={`lbl ${k === "live" ? "live" : ""}`} x={x + 8} y={y - 7} textAnchor="start">
          {shortId(row.id)}
        </text>
      ))}

      {hidden > 0 ? (
        <text className="lbl" x={w - 12} y={h - 12} textAnchor="end">
          {`+${hidden} more`}
        </text>
      ) : null}
    </svg>
  );
}
