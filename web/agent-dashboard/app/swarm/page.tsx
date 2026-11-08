import Link from "next/link";
import { StageBadge } from "@/components/badge";
import { Field } from "@/components/field";
import { Refresh } from "@/components/refresh";
import { When } from "@/components/time";
import { readIndex } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { usd } from "@/lib/format";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Swarm" };

export default async function SwarmPage() {
  const viewer = await requireViewer();
  const now = nowMs();
  let index = null;
  let error = false;
  try {
    index = await readIndex(viewer);
  } catch (issue) {
    console.error(issue);
    error = true;
  }
  const rows = index?.tickets ?? [];
  const open = rows.filter((row) => row.open);
  const live = open.filter((row) => row.assignment?.state === "running");
  const waiting = open.filter((row) => row.waiting_for);
  const workers = [...rows]
    .filter((row) => row.assignment)
    .sort((a, b) => Number(b.open) - Number(a.open) || b.updated_ms - a.updated_ms)
    .slice(0, 20);
  const spend = open.reduce((sum, row) => sum + row.spent_microusd, 0);

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Swarm</h1>
          <p className="sub">Every Ticket on one field: the host at the centre, its Tickets on the rings, and each live worker beside its Ticket.</p>
        </div>
        <div className="stamp">
          {index ? (
            <>
              <span>synced {index.generated_at ? <When ms={Date.parse(index.generated_at)} now={now} /> : "once"}</span>
              <Refresh seconds={20} />
            </>
          ) : null}
        </div>
      </header>

      {error ? (
        <p className="notice">The tickets could not be read from storage.</p>
      ) : !index ? (
        <p className="empty">The swarm has not published its index yet.</p>
      ) : (
        <div className="grid">
          <section className="panel col-12" aria-labelledby="field-title">
            <div className="panel-head">
              <h2 id="field-title">The field</h2>
              <span className="legend">
                <span><span className="dot live" />live worker</span>
                <span><span className="dot warn" />waiting</span>
                <span><span className="dot" />open</span>
                <span>{rows.length} tickets</span>
              </span>
            </div>
            <div className="panel-body">
              <Field tickets={rows} large />
            </div>
          </section>

          <div className="col-12">
            <div className="strip" role="group" aria-label="The swarm now">
              <div className="cell">
                <div className="label">Open tickets</div>
                <div className="value">{open.length}</div>
                <div className="note">of {rows.length} in the index</div>
              </div>
              <div className="cell">
                <div className="label">Live workers</div>
                <div className="value">{live.length}</div>
                <div className="note">running now</div>
              </div>
              <div className="cell">
                <div className="label">Waiting</div>
                <div className="value">{waiting.length}</div>
                <div className="note">asking for a person</div>
              </div>
              <div className="cell">
                <div className="label">Committed</div>
                <div className="value">
                  {usd(spend)}<small>on open tickets</small>
                </div>
                <div className="note">Router and workers</div>
              </div>
            </div>
          </div>

          <section className="panel col-12" aria-labelledby="workers-title">
            <div className="panel-head">
              <h2 id="workers-title">Workers</h2>
              <span className="legend">{workers.length ? `${workers.length} with a run` : "none yet"}</span>
            </div>
            {workers.length === 0 ? (
              <div className="panel-body">
                <p className="empty">No worker has run yet.</p>
              </div>
            ) : (
              <ul className="rows">
                {workers.map((row) => (
                  <li key={row.id}>
                    <Link href={`/tickets/${row.id}`} className="row">
                      <div className="row-title">
                        <span className="name">{row.title}</span>
                        <StageBadge stage={row.stage} outcome={row.outcome} />
                      </div>
                      <span className="row-right">{usd(row.spent_microusd)}</span>
                      <div className="row-meta">
                        <span>worker {row.assignment?.state}</span>
                        <span>{row.assignment?.reports ?? 0} reports</span>
                        {row.assignment?.report ? <span>last {row.assignment.report}</span> : null}
                        <span>updated <When ms={row.updated_ms} now={now} /></span>
                      </div>
                    </Link>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      )}
    </>
  );
}
