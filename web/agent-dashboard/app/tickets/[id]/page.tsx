import Link from "next/link";
import { notFound } from "next/navigation";
import type { ReactNode } from "react";
import { StageBadge } from "@/components/badge";
import { Markdown } from "@/components/markdown";
import { Refresh } from "@/components/refresh";
import { When } from "@/components/time";
import { readTicket, validTicketId } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { duration, stamp, usd } from "@/lib/format";
import type { AssignmentView, TicketSnapshot, Turn } from "@/lib/types";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";

type Props = { params: Promise<{ id: string }> };

export async function generateMetadata({ params }: Props) {
  const { id } = await params;
  return { title: validTicketId(id) ? `Ticket ${id}` : "Ticket" };
}

const KIND_LABEL: Record<Turn["kind"], string> = {
  request: "asked",
  message: "wrote",
  acknowledgement: "acknowledged",
  result: "replied",
  attention: "asked you",
  note: "note",
  approval: "approval",
  secret: "vault",
};

function who(turn: Turn): string {
  if (turn.from === "agent") return "Agent";
  if (turn.from === "router") return "Router";
  return turn.who ?? turn.from;
}

function TurnView({ turn, now }: { turn: Turn; now: number }) {
  return (
    <article className={`turn ${turn.from}`}>
      <div className="turn-head">
        <strong>{who(turn)}</strong>
        <span>{KIND_LABEL[turn.kind]}</span>
        {turn.channel ? <span>on {turn.channel}</span> : null}
        <When ms={turn.at_ms} now={now} />
        {turn.delivery && !turn.delivery.delivered ? (
          <span className="tag warn">
            not delivered yet
            {turn.delivery.attempts > 0 ? ` (${turn.delivery.attempts} failed)` : ""}
            {turn.delivery.error ? `: ${turn.delivery.error}` : ""}
          </span>
        ) : null}
      </div>
      <div className="prose">
        <Markdown>{turn.text}</Markdown>
      </div>
    </article>
  );
}

function Assignment({ assignment, now, open }: { assignment: AssignmentView; now: number; open: boolean }) {
  const start = assignment.started_ms ?? assignment.created_ms;
  const end = assignment.ended_ms;
  const last = assignment.reports.at(-1);
  return (
    <details className="reports" open={open}>
      <summary>
        <strong>{assignment.state}</strong>
        {last ? ` · last report ${last.status}` : ""}
        {" · "}
        started <When ms={start} now={now} />
        {end ? ` · ran ${duration(end - start)}` : ""}
        {assignment.ended_reason ? ` · ${assignment.ended_reason}` : ""}
      </summary>
      <div>
        {assignment.report_count > assignment.reports.length ? (
          <p className="muted">
            Showing the latest {assignment.reports.length} of {assignment.report_count} reports.
          </p>
        ) : null}
        {assignment.reports.length === 0 ? <p className="muted">No report yet.</p> : null}
        {assignment.reports.map((report) => (
          <div className="report" key={report.seq}>
            <div className="turn-head">
              <strong>{report.status}</strong>
              <When ms={report.at_ms} now={now} />
              {report.spent_microusd > 0 ? <span>{usd(report.spent_microusd)} so far</span> : null}
            </div>
            <div className="prose">
              <Markdown>{report.text}</Markdown>
            </div>
            {report.question ? (
              <p className="prose">
                <strong>Question:</strong> {report.question}
              </p>
            ) : null}
          </div>
        ))}
      </div>
    </details>
  );
}

function Facts({ ticket }: { ticket: TicketSnapshot }) {
  const asker = ticket.requester.name ?? ticket.requester.id;
  const plan =
    ticket.plan.kind === "swarm"
      ? `swarm of ${ticket.plan.agents ?? "?"}`
      : [ticket.plan.harness ?? "session", ticket.plan.environment].filter(Boolean).join(" on ");
  const facts: [string, ReactNode][] = [
    ["Asked by", `${asker} (${ticket.source})`],
    ["Created", stamp(ticket.created_ms)],
    ["Updated", stamp(ticket.updated_ms)],
    ["Spent", `${usd(ticket.spent_microusd)}${ticket.budget_microusd ? ` of ${usd(ticket.budget_microusd)}` : ""}`],
    ["Plan", plan],
    ["Owes", ticket.deliverable.replace("_", " ")],
  ];
  if (ticket.repo) {
    facts.push([
      "Repository",
      <a key="repo" href={`https://github.com/${ticket.repo}`} target="_blank" rel="noopener noreferrer">
        {ticket.repo}
      </a>,
    ]);
  }
  if (ticket.approval.state !== "not_needed") {
    facts.push([
      "Budget",
      ticket.approval.state === "waiting" ? "waiting for approval" : `approved by ${ticket.approval.by ?? "someone"}`,
    ]);
  }
  if (ticket.ended) {
    facts.push(["Ended", `${ticket.ended.outcome} by ${ticket.ended.by}, ${stamp(ticket.ended.at_ms)}`]);
  }
  return (
    <dl className="kv">
      {facts.map(([name, value]) => (
        <div key={name} style={{ display: "contents" }}>
          <dt>{name}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

export default async function TicketPage({ params }: Props) {
  const viewer = await requireViewer();
  const { id } = await params;
  if (!validTicketId(id)) notFound();
  let ticket: TicketSnapshot | null;
  try {
    ticket = await readTicket(id, viewer);
  } catch (error) {
    console.error(error);
    return (
      <>
        <nav className="crumbs">
          <Link href="/tickets">Tickets</Link>
        </nav>
        <header className="page-head">
          <div>
            <h1>Ticket {id}</h1>
          </div>
        </header>
        <p className="notice">This ticket could not be read from storage.</p>
      </>
    );
  }
  if (!ticket) {
    return (
      <>
        <Refresh seconds={10} />
        <nav className="crumbs">
          <Link href="/tickets">Tickets</Link>
        </nav>
        <header className="page-head">
          <div>
            <h1>Ticket {id}</h1>
          </div>
        </header>
        <p className="empty">
          This ticket has not reached the dashboard yet. The publisher uploads within a few seconds of a change; this
          page checks again by itself.
        </p>
      </>
    );
  }
  const now = nowMs();
  const outcome = ticket.ended?.outcome;
  return (
    <>
      {ticket.open ? <Refresh seconds={10} /> : null}
      <nav className="crumbs">
        <Link href="/tickets">Tickets</Link>
      </nav>
      <header className="page-head">
        <div>
          <h1>{ticket.title}</h1>
          <p className="sub">
            <span className="mono">{ticket.id}</span>
            {ticket.source_url ? (
              <>
                {" · "}
                <a href={ticket.source_url} target="_blank" rel="noopener noreferrer">
                  Open the conversation in {ticket.source}
                </a>
              </>
            ) : null}
          </p>
        </div>
        <div className="stamp">
          <StageBadge stage={ticket.stage} outcome={outcome} />
        </div>
      </header>

      {ticket.attention ? (
        <p className="notice" style={{ marginBottom: 16 }}>
          <strong>Waiting for a person.</strong> {ticket.attention}
        </p>
      ) : null}

      <div className="detail-grid">
        <div className="side-stack">
          <section className="panel" aria-labelledby="conversation-title">
            <h2 className="section-title" id="conversation-title">
              Conversation
            </h2>
            <div className="transcript">
              {ticket.request ? <TurnView turn={ticket.request} now={now} /> : null}
              {ticket.conversation.map((turn, n) => (
                <TurnView key={`${turn.at_ms}-${n}`} turn={turn} now={now} />
              ))}
            </div>
          </section>

          {ticket.assignments.length > 0 ? (
            <section className="panel" aria-labelledby="work-title">
              <h2 className="section-title" id="work-title">
                Work
              </h2>
              {ticket.assignments.map((assignment, n) => (
                <Assignment
                  key={assignment.id}
                  assignment={assignment}
                  now={now}
                  open={ticket.open && n === ticket.assignments.length - 1}
                />
              ))}
            </section>
          ) : null}
        </div>

        <div className="side-stack">
          <section className="panel" aria-labelledby="facts-title">
            <h2 className="section-title" id="facts-title">
              Facts
            </h2>
            <div className="panel-body">
              <Facts ticket={ticket} />
            </div>
          </section>

          {ticket.browser ? (
            <section className="panel" aria-labelledby="browser-title">
              <h2 className="section-title" id="browser-title">
                Browser
              </h2>
              <details className="reports">
                <summary>
                  {ticket.browser.steps} steps
                  {ticket.browser.refused ? `, ${ticket.browser.refused} refused` : ""}
                  {ticket.browser.lost ? `, ${ticket.browser.lost} unanswered` : ""}
                  {ticket.browser.needs_approval.length > 0
                    ? `, asked about ${ticket.browser.needs_approval.join(", ")}`
                    : ""}
                </summary>
                <ul className="prose">
                  {ticket.browser.pages_omitted > 0 ? (
                    <li className="muted">{ticket.browser.pages_omitted} earlier pages not shown</li>
                  ) : null}
                  {ticket.browser.pages.map((page) => (
                    <li key={`${page.n}-${page.at}`}>
                      <span className="muted">
                        {page.action} · {page.at.slice(11, 19)}
                        {page.status ? ` · ${page.status}` : ""}
                      </span>
                      <br />
                      {page.title ? <strong>{page.title} </strong> : null}
                      <a href={page.url} target="_blank" rel="noopener noreferrer nofollow">
                        {page.url}
                      </a>
                    </li>
                  ))}
                </ul>
                <p className="muted">Addresses are shown without their query, and what was typed is not shown at all.</p>
              </details>
            </section>
          ) : null}

          <section className="panel" aria-labelledby="scope-title">
            <h2 className="section-title" id="scope-title">
              Scope
            </h2>
            <div className="panel-body prose">
              <Markdown>{ticket.scope}</Markdown>
            </div>
          </section>

          {ticket.decisions.length > 0 ? (
            <section className="panel" aria-labelledby="decisions-title">
              <h2 className="section-title" id="decisions-title">
                Decisions
              </h2>
              <details className="reports">
                <summary>
                  {ticket.decisions.length} decision{ticket.decisions.length === 1 ? "" : "s"} about this ticket
                </summary>
                <ul className="rows">
                  {ticket.decisions.map((decision) => (
                    <li key={decision.id} className="row">
                      <div className="row-title">
                        <span className="name">{decision.kind}</span>
                        <When ms={decision.at_ms} now={now} />
                      </div>
                      <div className="row-meta">
                        <span>{decision.reason}</span>
                      </div>
                    </li>
                  ))}
                </ul>
              </details>
            </section>
          ) : null}
        </div>
      </div>
      <p className="sub" style={{ marginTop: 16 }}>
        Snapshot of {stamp(ticket.updated_ms)}.
      </p>
    </>
  );
}
