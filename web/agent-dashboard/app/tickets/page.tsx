import { Refresh } from "@/components/refresh";
import { TicketRow } from "@/components/rows";
import { readIndex } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Tickets" };

export default async function TicketsPage() {
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
  const open = [...rows].filter((row) => row.open).sort((a, b) => b.updated_ms - a.updated_ms);
  const ended = [...rows].filter((row) => !row.open).sort((a, b) => b.updated_ms - a.updated_ms).slice(0, 30);

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Tickets</h1>
          <p className="sub">
            {rows.length} in the index · {open.length} open · {ended.length} recently ended
          </p>
        </div>
        <div className="stamp">
          <Refresh seconds={20} />
        </div>
      </header>

      {error ? (
        <p className="notice">The tickets could not be read from storage.</p>
      ) : rows.length === 0 ? (
        <p className="empty">No tickets have been published yet.</p>
      ) : (
        <div className="grid">
          <section className="panel col-12" aria-labelledby="open-title">
            <div className="panel-head">
              <h2 id="open-title">Open</h2>
              <span className="legend">{open.length ? `${open.length} in flight` : "none"}</span>
            </div>
            {open.length === 0 ? (
              <div className="panel-body">
                <p className="empty">Nothing is open.</p>
              </div>
            ) : (
              <ul className="rows">
                {open.map((row) => (
                  <TicketRow key={row.id} row={row} now={now} wide />
                ))}
              </ul>
            )}
          </section>

          <section className="panel col-12" aria-labelledby="ended-title">
            <div className="panel-head">
              <h2 id="ended-title">Recently ended</h2>
              <span className="legend">{ended.length ? `the latest ${ended.length}` : "none"}</span>
            </div>
            {ended.length === 0 ? (
              <div className="panel-body">
                <p className="empty">Nothing has ended yet.</p>
              </div>
            ) : (
              <ul className="rows">
                {ended.map((row) => (
                  <TicketRow key={row.id} row={row} now={now} wide />
                ))}
              </ul>
            )}
          </section>
        </div>
      )}
    </>
  );
}
