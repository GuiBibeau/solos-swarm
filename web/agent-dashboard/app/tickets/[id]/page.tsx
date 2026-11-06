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
import type { AssignmentView, Turn, TicketSnapshot } from "@/lib/types";
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

function TurnView({ turn, now }: { turn: Turn; now: number }) {
  const who = turn.from === "agent" ? "Agent" : (turn.who ?? turn.from);
  return (
    <article className={`turn ${turn.from}`}>
      <div className="turn-head">
        <strong>{who}</strong>
        <span>{KIND_LABEL[turn.kind]}</span>
        {turn.channel ? <span>on {turn.channel}</span> : null}
        <When ms={turn.at_ms} now={now} />
        {turn.delivery && !turn.delivery.delivered ? (
          <span className="undelivered">
            not delivered yet
            {turn.delivery.attempts > 0 ? ` (${turn.delivery.attempts} failed)` : ""}
            {turn.delivery.error ? `: ${turn.delivery.error}` : ""}
          </span>
        ) : null}
      </div>
      <Markdown>{turn.text}</Markdown>
    </article>
  );
}

function Assignment({ assignment, now, open }: { assignment: AssignmentView; now: number; open: boolean }) {
  const start = assignment.started_ms ?? assignment.created_ms;
  const end = assignment.ended_ms;
  const last = assignment.reports.at(-1);
  return (
    <details open={open}>
      <summary>
        <strong>{assignment.state}</strong>
        {last ? ` · last report ${last.status}` : ""}
        {" · "}
        started <When ms={start} now={now} />
        {end ? ` · ran ${duration(end - start)}` : ""}
        {assignment.ended_reason ? ` · ${assignment.ended_reason}` : ""}
      </summary>
      <div className="reports">
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
            <Markdown>{report.text}</Markdown>
            {report.question ? (
              <p>
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
    facts.push(["Repository", <a key="repo" href={`https://github.com/${ticket.repo}`} target="_blank" rel="noopener noreferrer">{ticket.repo}</a>]);
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
    <dl className="facts">
      {facts.map(([name, value]) => (
        <div className="fact" key={name}>
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
        <Link href="/" className="crumb">
          ← Tickets
        </Link>
        <h1>Ticket {id}</h1>
        <p className="error">This ticket could not be read from storage.</p>
      </>
    );
  }
  if (!ticket) {
    return (
      <>
        <Refresh seconds={10} />
        <Link href="/" className="crumb">
          ← Tickets
        </Link>
        <h1>Ticket {id}</h1>
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
      <Link href="/" className="crumb">
        ← Tickets
      </Link>
      <h1>{ticket.title}</h1>
      <p className="row-meta">
        <StageBadge stage={ticket.stage} outcome={outcome} />
        <span className="mono">{ticket.id}</span>
        {ticket.source_url ? (
          <a href={ticket.source_url} target="_blank" rel="noopener noreferrer">
            Open the conversation in {ticket.source}
          </a>
        ) : null}
      </p>

      {ticket.attention ? (
        <div className="callout">
          <strong>Waiting for a person.</strong> {ticket.attention}
        </div>
      ) : null}
      <Facts ticket={ticket} />

      <h2>Conversation</h2>
      <div className="turns">
        {ticket.request ? <TurnView turn={ticket.request} now={now} /> : null}
        {ticket.conversation.map((turn, n) => (
          <TurnView key={`${turn.at_ms}-${n}`} turn={turn} now={now} />
        ))}
      </div>

      {ticket.assignments.length > 0 ? (
        <>
          <h2>Work</h2>
          {ticket.assignments.map((assignment, n) => (
            <Assignment
              key={assignment.id}
              assignment={assignment}
              now={now}
              open={ticket.open && n === ticket.assignments.length - 1}
            />
          ))}
        </>
      ) : null}

      {ticket.browser ? (
        <>
          <h2>Browser</h2>
          <details>
            <summary>
              {ticket.browser.steps} steps
              {ticket.browser.refused ? `, ${ticket.browser.refused} refused` : ""}
              {ticket.browser.lost ? `, ${ticket.browser.lost} unanswered` : ""}
              {ticket.browser.needs_approval.length > 0
                ? `, asked about ${ticket.browser.needs_approval.join(", ")}`
                : ""}
            </summary>
            <ul className="plain">
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
        </>
      ) : null}

      {ticket.decisions.length > 0 ? (
        <>
          <h2>Decisions</h2>
          <details>
            <summary>
              {ticket.decisions.length} decision{ticket.decisions.length === 1 ? "" : "s"} about this ticket
            </summary>
            <ul className="plain">
              {ticket.decisions.map((decision) => (
                <li key={decision.id}>
                  <strong>{decision.kind}</strong> <span className="muted">{decision.at.slice(0, 19)}Z</span>
                  <br />
                  {decision.reason}
                </li>
              ))}
            </ul>
          </details>
        </>
      ) : null}

      <details>
        <summary>What the Router understood the work to be</summary>
        <Markdown>{ticket.scope}</Markdown>
      </details>

      <p className="foot">Snapshot of {stamp(ticket.updated_ms)}.</p>
    </>
  );
}
