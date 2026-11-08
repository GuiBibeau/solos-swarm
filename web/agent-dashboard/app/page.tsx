import Link from "next/link";
import { StageBadge } from "@/components/badge";
import { Field } from "@/components/field";
import { Refresh } from "@/components/refresh";
import { JobRow as JobRowView, TicketRow } from "@/components/rows";
import { When } from "@/components/time";
import { readIndex, readJobs } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { duration, usd } from "@/lib/format";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Command" };

/** The publisher re-uploads the index every five minutes whether or not anything changed. */
const STALE_MS = 15 * 60 * 1000;

export default async function CommandPage() {
  const viewer = await requireViewer();
  const now = nowMs();
  let index = null;
  let indexError = false;
  try {
    index = await readIndex(viewer);
  } catch (error) {
    console.error(error);
    indexError = true;
  }
  let jobs: Awaited<ReturnType<typeof readJobs>> = null;
  let jobsError = false;
  try {
    jobs = await readJobs(viewer);
  } catch (error) {
    console.error(error);
    jobsError = true;
  }
  const rows = index?.tickets ?? [];
  const open = rows.filter((row) => row.open);
  const waiting = open.filter((row) => row.waiting_for);
  const working = open.filter((row) => row.assignment?.state === "running");
  const committed = open.reduce((sum, row) => sum + row.spent_microusd, 0);
  const jobRows = [...(jobs?.jobs ?? [])].sort((a, b) => Number(a.status === "up") - Number(b.status === "up"));
  const trouble = jobRows.filter((job) => job.status !== "up");
  const jobsStale = jobs !== null && now - jobs.checked_ms > STALE_MS;
  const recent = [...rows].sort((a, b) => b.updated_ms - a.updated_ms).slice(0, 3);
  const generated = index?.generated_at ? Date.parse(index.generated_at) : null;
  const stale = generated !== null && now - generated > STALE_MS;

  const needs = [...waiting, ...open.filter((row) => row.stage === "approval" || row.stage === "answer")].filter(
    (row, n, all) => all.findIndex((other) => other.id === row.id) === n,
  );

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Command</h1>
          <p className="sub">
            {index
              ? `${open.length} open · ${waiting.length} waiting on you · ${working.length} working`
              : "The swarm has not published its index yet."}
          </p>
        </div>
        <div className="stamp">
          {generated ? (
            <>
              <span>synced <When ms={generated} now={now} /></span>
              <Refresh seconds={20} />
            </>
          ) : null}
        </div>
      </header>

      {stale ? (
        <p className="notice" style={{ marginBottom: 16 }}>
          The publisher has not written for fifteen minutes; these figures may be old.
        </p>
      ) : null}
      {jobsStale && !jobsError && jobs ? (
        <p className="notice" style={{ marginBottom: 16 }}>
          The Watchdog has not looked at these jobs for {duration(now - jobs.checked_ms)}.
        </p>
      ) : null}

      {indexError ? (
        <p className="notice">The tickets could not be read from storage.</p>
      ) : !index ? (
        <p className="empty">Nothing has been published yet. The publisher uploads the first index within a few seconds of a change.</p>
      ) : (
        <div className="grid">
          <section className="panel col-12" aria-labelledby="field-title">
            <div className="panel-head">
              <h2 id="field-title">The field</h2>
              <span className="legend">
                <span><span className="dot live" />live worker</span>
                <span><span className="dot warn" />waiting</span>
                <span><span className="dot" />open</span>
              </span>
            </div>
            <div className="field-split">
              <div className="field-body">
                <Field tickets={rows} />
              </div>
              <div className="needs">
                <p className="needs-head" id="needs-title">
                  <span>Needs you</span>
                  <span>{needs.length ? `${needs.length} waiting` : "clear"}</span>
                </p>
                {needs.length === 0 ? (
                  <p className="empty">Nothing is waiting on you. The swarm is working.</p>
                ) : (
                  <ul className="rows">
                    {needs.slice(0, 3).map((row) => (
                      <li key={row.id}>
                        <Link href={`/tickets/${row.id}`} className="row">
                          <div className="row-title">
                            <span className="name">{row.title}</span>
                            <StageBadge stage={row.stage} outcome={row.outcome} />
                          </div>
                          <div className="row-meta">
                            <span>{row.waiting_for ?? row.stage.replace("_", " ")}</span>
                          </div>
                        </Link>
                      </li>
                    ))}
                  </ul>
                )}
                <div className="side-strip" role="group" aria-label="Today">
                  <div className="cell">
                    <div className="label">Open tickets</div>
                    <div className="value">{open.length}</div>
                  </div>
                  <div className="cell">
                    <div className="label">Working now</div>
                    <div className="value">{working.length}</div>
                  </div>
                  <div className="cell">
                    <div className="label">Waiting on you</div>
                    <div className="value">{waiting.length}</div>
                  </div>
                  <div className="cell">
                    <div className="label">Committed</div>
                    <div className="value">
                      {usd(committed)}<small>on open tickets</small>
                    </div>
                  </div>
                </div>
              </div>
            </div>
          </section>

          <section className="panel col-12" aria-labelledby="recent-title">
            <div className="dual">
              <div className="dual-main">
                <div className="panel-head">
                  <h2 id="recent-title">Recent tickets</h2>
                  <span className="legend"><Link href="/tickets">View all</Link></span>
                </div>
                {recent.length === 0 ? (
                  <div className="panel-body">
                    <p className="empty">No tickets yet.</p>
                  </div>
                ) : (
                  <ul className="rows">
                    {recent.map((row) => (
                      <TicketRow key={row.id} row={row} now={now} />
                    ))}
                  </ul>
                )}
              </div>
              <div className="dual-side" aria-labelledby="jobs-title">
                <div className="panel-head">
                  <h2 id="jobs-title">External jobs</h2>
                  <span className="legend">
                    {trouble.length ? `${trouble.length} in trouble` : jobRows.length ? "all up" : "none"}
                    {jobRows.length > 3 ? (
                      <>
                        {" · "}
                        <Link href="/jobs">View all</Link>
                      </>
                    ) : null}
                  </span>
                </div>
                {jobsError ? (
                  <div className="panel-body">
                    <p className="notice">The external jobs could not be read from storage.</p>
                  </div>
                ) : jobRows.length === 0 ? (
                  <div className="panel-body">
                    <p className="empty">The Watchdog has no external jobs configured.</p>
                  </div>
                ) : (
                  <ul className="rows">
                    {jobRows.slice(0, 3).map((job) => (
                      <JobRowView key={job.name} job={job} now={now} />
                    ))}
                  </ul>
                )}
              </div>
            </div>
          </section>
        </div>
      )}
    </>
  );
}
