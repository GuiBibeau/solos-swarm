import Link from "next/link";
import { StageBadge } from "@/components/badge";
import { Markdown } from "@/components/markdown";
import { Refresh } from "@/components/refresh";
import { When } from "@/components/time";
import { readIndex, readTicket } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { usd } from "@/lib/format";
import type { TicketSnapshot, Turn } from "@/lib/types";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Chat" };

/** How many recent conversations the list carries. Each one is one Blob read on the server. */
const THREADS = 8;

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

type ChatItem =
  | { kind: "turn"; at_ms: number; turn: Turn }
  | { kind: "report"; at_ms: number; status: string; text: string; question?: string };

function items(ticket: TicketSnapshot): ChatItem[] {
  const turns: ChatItem[] = [ticket.request, ...ticket.conversation]
    .filter((turn): turn is Turn => Boolean(turn))
    .map((turn) => ({ kind: "turn" as const, at_ms: turn.at_ms, turn }));
  const reports: ChatItem[] = ticket.assignments.flatMap((assignment) =>
    assignment.reports.map((report) => ({
      kind: "report" as const,
      at_ms: report.at_ms,
      status: report.status,
      text: report.text,
      question: report.question,
    })),
  );
  return [...turns, ...reports].sort((a, b) => a.at_ms - b.at_ms);
}

type Props = { searchParams: Promise<{ t?: string }> };

export default async function ChatPage({ searchParams }: Props) {
  const viewer = await requireViewer();
  const now = nowMs();
  const { t } = await searchParams;
  let index = null;
  let error = false;
  try {
    index = await readIndex(viewer);
  } catch (issue) {
    console.error(issue);
    error = true;
  }
  const recent = [...(index?.tickets ?? [])].sort((a, b) => b.updated_ms - a.updated_ms).slice(0, THREADS);
  const loaded = await Promise.all(
    recent.map(async (row) => {
      try {
        return await readTicket(row.id, viewer);
      } catch (error) {
        console.error(error);
        return null;
      }
    }),
  );
  const threads = loaded.filter((ticket): ticket is TicketSnapshot => ticket !== null);
  const selected = threads.find((ticket) => ticket.id === t) ?? threads[0];

  return (
    <>
      <header className="page-head">
        <div>
          <h1>Chat</h1>
          <p className="sub">What the swarm was asked, and what it said. It answers where it was asked, so the thread lives in the source.</p>
        </div>
        <div className="stamp">
          <Refresh seconds={20} />
        </div>
      </header>

      {error ? (
        <p className="notice">The conversations could not be read from storage.</p>
      ) : threads.length === 0 ? (
        <p className="empty">No conversation has been published yet.</p>
      ) : (
        <div className="chat">
          <nav className="panel threads" aria-label="Conversations">
            {threads.map((ticket) => {
              const last = items(ticket).at(-1);
              return (
                <Link
                  key={ticket.id}
                  href={`/chat?t=${ticket.id}`}
                  className="thread-item"
                  aria-current={ticket.id === selected?.id ? "true" : undefined}
                >
                  <span className="t">{ticket.title}</span>
                  <span className="m">
                    <span>
                      {(ticket.requester.name ?? ticket.requester.id).split(" ")[0]} · {ticket.source}
                    </span>
                    <span>{last ? <When ms={last.at_ms} now={now} /> : null}</span>
                  </span>
                </Link>
              );
            })}
          </nav>

          {selected ? (
            <section className="panel" aria-labelledby="thread-title">
              <div className="thread-head">
                <h2 id="thread-title">{selected.title}</h2>
                <div className="m">
                  <StageBadge stage={selected.stage} outcome={selected.ended?.outcome} />
                  <span className="mono">{selected.id}</span>
                  <span>{selected.spent_microusd > 0 ? `${usd(selected.spent_microusd)} spent` : "no spend yet"}</span>
                  <Link href={`/tickets/${selected.id}`}>Open the ticket</Link>
                </div>
              </div>

              <div className="transcript">
                {items(selected).map((item, n) =>
                  item.kind === "turn" ? (
                    <article className={`turn ${item.turn.from}`} key={`${item.at_ms}-${n}`}>
                      <div className="turn-head">
                        <strong>{who(item.turn)}</strong>
                        <span>{KIND_LABEL[item.turn.kind]}</span>
                        {item.turn.channel ? <span>on {item.turn.channel}</span> : null}
                        <When ms={item.at_ms} now={now} />
                      </div>
                      <div className="prose">
                        <Markdown>{item.turn.text}</Markdown>
                      </div>
                    </article>
                  ) : (
                    <article className="turn bot" key={`${item.at_ms}-${n}`}>
                      <div className="turn-head">
                        <strong>Worker</strong>
                        <span>reported {item.status}</span>
                        <When ms={item.at_ms} now={now} />
                      </div>
                      <div className="prose">
                        <Markdown>{item.text}</Markdown>
                        {item.question ? (
                          <p>
                            <strong>Question:</strong> {item.question}
                          </p>
                        ) : null}
                      </div>
                    </article>
                  ),
                )}
              </div>

              <div className="composer">
                <span className="why">
                  {selected.source_url
                    ? `Reply in ${selected.source}; the agent reads that thread.`
                    : `The agent reads its configured ${selected.source} channel, not this page.`}
                </span>
                {selected.source_url ? (
                  <a className="button primary" href={selected.source_url} target="_blank" rel="noopener noreferrer">
                    Reply in {selected.source}
                  </a>
                ) : null}
              </div>
            </section>
          ) : null}
        </div>
      )}
    </>
  );
}
