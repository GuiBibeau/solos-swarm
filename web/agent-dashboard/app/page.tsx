import Link from "next/link";
import { JobBadge, StageBadge } from "@/components/badge";
import { Refresh } from "@/components/refresh";
import { When } from "@/components/time";
import { readIndex, readJobs } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { duration, usd } from "@/lib/format";
import type { Index, IndexRow, JobRow, Jobs } from "@/lib/types";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Tickets" };

/** The publisher re-uploads the index every five minutes whether or not anything changed. */
const STALE_MS = 15 * 60 * 1000;

function Row({ row, now }: { row: IndexRow; now: number }) {
  const asker = row.asker.name ?? row.asker.id;
  return (
    <li>
      <Link href={`/tickets/${row.id}`} className="row">
        <div className="row-title">
          <span>{row.title}</span>
          <StageBadge stage={row.stage} outcome={row.outcome} />
        </div>
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
            {usd(row.spent_microusd)}
            {row.budget_microusd ? ` of ${usd(row.budget_microusd)}` : ""}
          </span>
          {row.assignment ? (
            <span>
              worker {row.assignment.state}
              {row.assignment.report ? `, last report ${row.assignment.report}` : ""}
            </span>
          ) : null}
        </div>
        {row.waiting_for ? <div className="row-waiting">{row.waiting_for}</div> : null}
      </Link>
    </li>
  );
}

/** Problems first, then the order the owner wrote them in. */
function byTrouble(jobs: JobRow[]): JobRow[] {
  return [...jobs].sort((a, b) => Number(a.status === "up") - Number(b.status === "up"));
}

function Job({ job, now }: { job: JobRow; now: number }) {
  return (
    <li>
      <div className="row plain">
        <div className="row-title">
          <span>{job.name}</span>
          <JobBadge status={job.status} />
        </div>
        <div className="row-meta">
          <span>{job.check}</span>
          <span>
            since <When ms={job.since_ms} now={now} />
          </span>
          {job.status === "up" ? <span>{job.detail}</span> : null}
        </div>
        {job.status !== "up" ? <div className="row-waiting">{job.detail}</div> : null}
      </div>
    </li>
  );
}

/** The external jobs the Watchdog checks: work that runs outside agent mode and is not a ticket. */
function ExternalJobs({ jobs, now }: { jobs: Jobs; now: number }) {
  if (jobs.jobs.length === 0) return null;
  const up = jobs.jobs.filter((job) => job.status === "up").length;
  const stale = now - jobs.checked_ms > STALE_MS;
  return (
    <>
      <h2>External jobs</h2>
      <p className="sync">
        <span>
          checked <When ms={jobs.checked_ms} now={now} />
        </span>
        <span>
          {up} of {jobs.jobs.length} up
        </span>
      </p>
      {stale ? (
        <p className="warn">
          The Watchdog has not looked at these jobs for {duration(now - jobs.checked_ms)}, so they may have changed. The
          agent may be down: <code>scripts/agent-deploy status</code>.
        </p>
      ) : null}
      <ul className="rows">
        {byTrouble(jobs.jobs).map((job) => (
          <Job key={job.name} job={job} now={now} />
        ))}
      </ul>
    </>
  );
}

function Sync({ index, now }: { index: Index; now: number }) {
  const at = index.generated_at ? Date.parse(index.generated_at) : Number.NaN;
  if (Number.isNaN(at)) return null;
  const stale = now - at > STALE_MS;
  return (
    <>
      <p className="sync">
        <span>
          synced <When ms={at} now={now} />
        </span>
        <span>{index.open} open</span>
      </p>
      {stale ? (
        <p className="warn">
          Nothing has been published for {duration(now - at)}. The publisher on the box may be down:{" "}
          <code>scripts/agent-deploy status</code>.
        </p>
      ) : null}
    </>
  );
}

export default async function Tickets() {
  const viewer = await requireViewer();
  let index: Index | null = null;
  let failure: string | null = null;
  try {
    index = await readIndex(viewer);
  } catch (error) {
    console.error(error);
    failure = "The tickets could not be read from storage. Check the Blob store's credentials in the project.";
  }
  let jobs: Jobs | null = null;
  let jobsFailure: string | null = null;
  try {
    jobs = await readJobs(viewer);
  } catch (error) {
    console.error(error);
    jobsFailure = "The external jobs could not be read from storage.";
  }
  const now = nowMs();
  const open = index?.tickets.filter((row) => row.open) ?? [];
  const ended = index?.tickets.filter((row) => !row.open) ?? [];

  return (
    <>
      <Refresh seconds={20} />
      <h1>Tickets</h1>
      {index ? <Sync index={index} now={now} /> : null}
      {failure ? <p className="error">{failure}</p> : null}
      {!failure && !index ? (
        <p className="empty">
          Nothing has been published yet. Is <code>solos-swarm agent publish</code> running on the box?
        </p>
      ) : null}
      {jobs ? <ExternalJobs jobs={jobs} now={now} /> : null}
      {jobsFailure ? <p className="error">{jobsFailure}</p> : null}
      {index && open.length === 0 && ended.length === 0 ? <p className="empty">No tickets yet.</p> : null}
      {open.length > 0 ? (
        <>
          <h2>Open</h2>
          <ul className="rows">
            {open.map((row) => (
              <Row key={row.id} row={row} now={now} />
            ))}
          </ul>
        </>
      ) : null}
      {ended.length > 0 ? (
        <>
          <h2>Recently ended</h2>
          <ul className="rows">
            {ended.map((row) => (
              <Row key={row.id} row={row} now={now} />
            ))}
          </ul>
        </>
      ) : null}
    </>
  );
}
