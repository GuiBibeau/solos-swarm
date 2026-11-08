import Link from "next/link";
import { JobBadge, StageBadge } from "@/components/badge";
import { When } from "@/components/time";
import { duration, usd } from "@/lib/format";
import type { IndexRow, JobRow } from "@/lib/types";

/**
 * One Ticket in a list. `wide` spreads the facts across the row (a full-width list); without it the facts sit
 * under the title (an overview panel). Both are the same markup; the grid does the difference.
 */
export function TicketRow({ row, now, wide = false }: { row: IndexRow; now: number; wide?: boolean }) {
  const asker = row.asker.name ?? row.asker.id;
  return (
    <li>
      <Link href={`/tickets/${row.id}`} className={`row${wide ? " row-wide" : ""}`}>
        <div className="row-title">
          <span className="name">{row.title}</span>
          <StageBadge stage={row.stage} outcome={row.outcome} />
        </div>
        <span className="row-right">{usd(row.spent_microusd)}</span>
        <div className="row-meta">
          <span>
            {asker} · {row.source}
          </span>
          <span>
            {row.open ? (
              <>
                {duration(now - row.created_ms)} old · <When ms={row.updated_ms} now={now} />
              </>
            ) : (
              <>
                took {duration(row.duration_ms ?? 0)} · ended <When ms={row.updated_ms} now={now} />
              </>
            )}
          </span>
          <span>
            {row.waiting_for ? (
              <span className="row-waiting-inline">{row.waiting_for}</span>
            ) : row.assignment ? (
              <>
                worker {row.assignment.state}
                {row.assignment.reports > 0 ? ` · ${row.assignment.reports} reports` : ""}
              </>
            ) : (
              "no worker yet"
            )}
          </span>
        </div>
      </Link>
    </li>
  );
}

/** One external job: its state, how it is checked, and since when. */
export function JobRow({ job, now, wide = false }: { job: JobRow; now: number; wide?: boolean }) {
  return (
    <li className={`row${wide ? " row-wide" : ""}`}>
      <div className="row-title">
        <span className="name">{job.name}</span>
        <JobBadge status={job.status} />
      </div>
      <span className="row-right">
        <When ms={job.since_ms} now={now} />
      </span>
      <div className="row-meta">
        <span>{job.check}</span>
        <span>{job.detail}</span>
      </div>
    </li>
  );
}
