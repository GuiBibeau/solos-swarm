import { JobRow } from "@/components/rows";
import { Refresh } from "@/components/refresh";
import { When } from "@/components/time";
import { readJobs } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { duration } from "@/lib/format";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Jobs" };

const STALE_MS = 15 * 60 * 1000;

export default async function JobsPage() {
  const viewer = await requireViewer();
  const now = nowMs();
  let jobs: Awaited<ReturnType<typeof readJobs>> = null;
  let error = false;
  try {
    jobs = await readJobs(viewer);
  } catch (issue) {
    console.error(issue);
    error = true;
  }
  const rows = [...(jobs?.jobs ?? [])].sort((a, b) => Number(a.status === "up") - Number(b.status === "up"));
  const trouble = rows.filter((job) => job.status !== "up");
  const stale = jobs !== null && now - jobs.checked_ms > STALE_MS;

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Jobs</h1>
          <p className="sub">Work outside agent mode that the Watchdog checks every minute.</p>
        </div>
        <div className="stamp">
          {jobs ? (
            <>
              <span>checked <When ms={jobs.checked_ms} now={now} /></span>
              <Refresh seconds={30} />
            </>
          ) : null}
        </div>
      </header>

      {error ? (
        <p className="notice">The external jobs could not be read from storage.</p>
      ) : stale && jobs ? (
        <p className="notice" style={{ marginBottom: 16 }}>
          The Watchdog has not looked at these jobs for {duration(now - jobs.checked_ms)}.
        </p>
      ) : null}

      {rows.length === 0 ? (
        <p className="empty">The Watchdog has no external jobs configured, or it has not published them yet.</p>
      ) : (
        <section className="panel" aria-labelledby="jobs-title">
          <div className="panel-head">
            <h2 id="jobs-title">External jobs</h2>
            <span className="legend">{trouble.length ? `${trouble.length} in trouble` : "all up"}</span>
          </div>
          <ul className="rows">
            {rows.map((job) => (
              <JobRow key={job.name} job={job} now={now} wide />
            ))}
          </ul>
        </section>
      )}
    </>
  );
}
