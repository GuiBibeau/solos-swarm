"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { JobBadge, StageBadge } from "@/components/badge";
import { Markdown } from "@/components/markdown";
import { When } from "@/components/time";
import { duration, usd } from "@/lib/format";
import { THREAD_RE, threadOf } from "@/lib/threads";
import type { ChatMessage, Index, IndexRow, Jobs, Note, TicketSnapshot, Turn } from "@/lib/types";

const THREAD_KEY = "solos-web-thread";
const POLL_MS = 2000;

const STARTERS = [
  "What are you working on?",
  "What needs me right now?",
  "Summarise the open tickets",
  "What did you spend today?",
];

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

type Item =
  | { kind: "turn"; at_ms: number; turn: Turn }
  | { kind: "report"; at_ms: number; status: string; text: string; question?: string }
  | { kind: "pending"; at_ms: number; message: ChatMessage }
  | { kind: "note"; at_ms: number; note: Note };

function who(turn: Turn): string {
  if (turn.from === "agent") return "Agent";
  if (turn.from === "router") return "Router";
  return turn.who ?? turn.from;
}

function normalize(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

/**
 * The thread as one timeline. A worker's `ready` report and the agent's delivered result usually carry the
 * same text; the conversation already has the reply, so the report is only added when it says something the
 * conversation does not. The same turn delivered twice is kept once.
 */
function items(ticket: TicketSnapshot | null): Item[] {
  if (!ticket) return [];
  const seenTurns = new Set<string>();
  const said = new Set<string>();
  const turns: Item[] = [];
  for (const turn of [ticket.request, ...ticket.conversation]) {
    if (!turn) continue;
    const text = normalize(turn.text);
    const key = `${turn.at_ms}|${turn.from}|${text}`;
    if (seenTurns.has(key)) continue;
    seenTurns.add(key);
    said.add(text);
    turns.push({ kind: "turn", at_ms: turn.at_ms, turn });
  }
  const spoken = [...said];
  const seenReports = new Set<string>();
  const reports: Item[] = [];
  for (const assignment of ticket.assignments) {
    for (const report of assignment.reports) {
      const text = normalize(report.text);
      const key = `${report.at_ms}|${text}`;
      if (text.length === 0 || seenReports.has(key)) continue;
      seenReports.add(key);
      // A report the conversation already delivered, or that the agent's reply contains (the reply may add a
      // sentence around it), is the same answer; showing it again reads as two answers.
      if (spoken.some((turn) => turn === text || (text.length >= 80 && (turn.includes(text) || text.includes(turn))))) {
        continue;
      }
      reports.push({
        kind: "report",
        at_ms: report.at_ms,
        status: report.status,
        text: report.text,
        question: report.question,
      });
    }
  }
  return [...turns, ...reports].sort((a, b) => a.at_ms - b.at_ms);
}

function makeThread(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return `wc_${Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")}`;
}

/** The state mark beside a conversation, as a bot's avatar shows idle, working or needs-you. */
function threadTone(row: IndexRow): string {
  if (!row.open) return row.outcome === "completed" ? "good" : "";
  if (row.assignment?.state === "running") return "live";
  if (row.waiting_for) return "warn";
  return "";
}

function working(ticket: TicketSnapshot | null, pending: ChatMessage[]): boolean {
  if (pending.length > 0) return true;
  return ticket?.open === true && ["queued", "working", "delivering"].includes(ticket.stage);
}

export function ChatLive({
  initialThread,
  initialTicket,
  initialIndex,
  initialJobs,
  initialNotes,
  initialNow,
}: {
  initialThread: string | null;
  initialTicket: TicketSnapshot | null;
  initialIndex: Index | null;
  initialJobs: Jobs | null;
  initialNotes: Note[];
  initialNow: number;
}) {
  const [thread, setThread] = useState<string | null>(initialThread);
  const [ticket, setTicket] = useState<TicketSnapshot | null>(initialTicket);
  const [index, setIndex] = useState<Index | null>(initialIndex);
  const [jobs, setJobs] = useState<Jobs | null>(initialJobs);
  const [notes, setNotes] = useState<Note[]>(initialNotes);
  const [pending, setPending] = useState<ChatMessage[]>([]);
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(initialNow);
  const transcriptRef = useRef<HTMLDivElement | null>(null);
  const inputRef = useRef<HTMLTextAreaElement | null>(null);
  const nearBottom = useRef(true);

  // A thread lives in the address and in storage, so a reload returns to the same conversation.
  useEffect(() => {
    if (thread) {
      try {
        localStorage.setItem(THREAD_KEY, thread);
      } catch {
        // storage can be unavailable; the address still holds it
      }
      return;
    }
    let next: string | null = null;
    try {
      const stored = localStorage.getItem(THREAD_KEY);
      if (stored && THREAD_RE.test(stored)) next = stored;
    } catch {
      next = null;
    }
    const chosen = next ?? makeThread();
    window.history.replaceState(null, "", `?t=${chosen}`);
    // Deferred, so the effect does not cascade a render synchronously; the thread id is an external system's answer.
    const timer = window.setTimeout(() => setThread(chosen), 0);
    return () => window.clearTimeout(timer);
  }, [thread]);

  // Poll the thread's conversation and the command state while the page is on screen.
  useEffect(() => {
    if (!thread) return;
    let stopped = false;
    const tick = async () => {
      try {
        const response = await fetch(`/api/chat?t=${encodeURIComponent(thread)}`, { cache: "no-store" });
        if (!response.ok) return;
        const data = (await response.json()) as {
          ticket: TicketSnapshot | null;
          index: Index | null;
          jobs: Jobs | null;
          notes?: Note[];
        };
        if (stopped) return;
        setTicket(data.ticket);
        setIndex(data.index);
        setJobs(data.jobs);
        setNotes(data.notes ?? []);
        setNow(Date.now());
      } catch {
        // a poll that fails leaves the last state on screen
      }
    };
    void tick();
    const timer = window.setInterval(() => {
      if (!document.hidden) void tick();
    }, POLL_MS);
    return () => {
      stopped = true;
      window.clearInterval(timer);
    };
  }, [thread]);

  const messages = useMemo(() => {
    const fromTicket = items(ticket);
    // A sent message stops being pending once the conversation carries it.
    const said = new Set(
      [ticket?.request, ...(ticket?.conversation ?? [])]
        .filter((turn): turn is Turn => Boolean(turn))
        .map((turn) => turn.text.trim()),
    );
    const inFlight = pending
      .filter((message) => !said.has(message.text.trim()))
      .map((message): Item => ({ kind: "pending", at_ms: message.at_ms, message }));
    const agentNotes = notes
      .filter((note) => note.kind === "ignore" || note.kind === "unavailable" || note.kind === "answered")
      .map((note): Item => ({ kind: "note", at_ms: note.at_ms, note }));
    return [...fromTicket, ...inFlight, ...agentNotes].sort((a, b) => a.at_ms - b.at_ms);
  }, [ticket, pending, notes]);

  const scroll = useCallback(() => {
    const el = transcriptRef.current;
    if (el && nearBottom.current) el.scrollTop = el.scrollHeight;
  }, []);

  useEffect(() => {
    scroll();
  }, [messages.length, scroll]);

  const conversations = useMemo(() => {
    const seen = new Set<string>();
    const list = (index?.tickets ?? [])
      .map((row) => ({ row, thread: threadOf(row) }))
      .filter((entry): entry is { row: IndexRow; thread: string } => Boolean(entry.thread))
      .sort((a, b) => b.row.updated_ms - a.row.updated_ms)
      .filter((entry) => (seen.has(entry.thread) ? false : (seen.add(entry.thread), true)));
    return list;
  }, [index]);

  const needs = (index?.tickets ?? []).filter((row) => row.open && row.waiting_for).slice(0, 3);
  const busy = working(ticket, pending);
  const lastReport = ticket?.assignments.at(-1)?.reports.at(-1) ?? null;

  const send = async (value: string) => {
    const trimmed = value.trim();
    if (!trimmed || !thread || sending) return;
    setSending(true);
    setError(null);
    try {
      const response = await fetch("/api/chat", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ thread, text: trimmed }),
      });
      const data = (await response.json()) as { error?: string; message?: ChatMessage };
      if (!response.ok || !data.message) throw new Error(data.error ?? "The message did not send.");
      nearBottom.current = true;
      setPending((current) => [...current, data.message as ChatMessage]);
      setText("");
      inputRef.current?.focus();
    } catch (issue) {
      setError((issue as Error).message);
    } finally {
      setSending(false);
    }
  };

  const onSubmit = (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    void send(text);
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      void send(text);
    }
  };

  const startNew = () => {
    const next = makeThread();
    setThread(next);
    setTicket(null);
    setPending([]);
    setText("");
    setError(null);
    window.history.replaceState(null, "", `?t=${next}`);
    inputRef.current?.focus();
  };

  const choose = (next: string) => {
    setThread(next);
    setTicket(null);
    setPending([]);
    setError(null);
    window.history.replaceState(null, "", `?t=${next}`);
  };

  return (
    <>
      <header className="page-head chat-head">
        <div>
          <h1>Chat</h1>
          <p className="sub">Talk to the swarm here; it also answers on Discord and GitHub. What it is doing stays on the right.</p>
        </div>
        <div className="stamp">
          <span className="dot live" aria-hidden="true" />
          <span>synced <When ms={now} now={now} /></span>
        </div>
      </header>

      <div className="chat-app">
        <nav className="panel conversations" aria-label="Conversations">
          <div className="panel-head">
            <h2>Conversations</h2>
            <button type="button" className="text-action" onClick={startNew}>
              New chat
            </button>
          </div>
          {conversations.length === 0 ? (
            <div className="panel-body">
              <p className="empty">No web conversation yet. This one is the first.</p>
            </div>
          ) : (
            <ul className="thread-list">
              {conversations.map(({ row, thread: id }) => (
                <li key={id}>
                  <button type="button" className="thread-item" aria-current={id === thread} onClick={() => choose(id)}>
                    <span className="t">{row.title}</span>
                    <span className="m">
                      <span className="thread-state">
                        <span className={`dot ${threadTone(row)}`} aria-hidden="true" />
                        {row.assignment?.state === "running" ? "working" : row.stage.replace("_", " ")}
                      </span>
                      <span><When ms={row.updated_ms} now={now} /></span>
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </nav>

        <section className="panel conversation" aria-labelledby="conversation-title">
          <div className="thread-head">
            <div>
              <h2 id="conversation-title">{ticket?.title ?? "New conversation"}</h2>
              <div className="m">
                {ticket ? <StageBadge stage={ticket.stage} outcome={ticket.ended?.outcome} /> : <span className="tag">web</span>}
                {ticket ? <span className="mono">{ticket.id}</span> : null}
                {ticket && ticket.spent_microusd > 0 ? <span>{usd(ticket.spent_microusd)} spent</span> : null}
                {busy ? (
                  <span className="working-line">
                    <span className="dot live" aria-hidden="true" />
                    {pending.length > 0 && !ticket ? "waiting for the agent to pick this up" : "the agent is working"}
                  </span>
                ) : null}
              </div>
            </div>
          </div>

          <div
            className="transcript live"
            ref={transcriptRef}
            onScroll={() => {
              const el = transcriptRef.current;
              nearBottom.current = el ? el.scrollHeight - el.scrollTop - el.clientHeight < 80 : true;
            }}
          >
            {messages.length === 0 ? (
              <div className="starters">
                <p className="starters-title">What should the swarm do?</p>
                <p className="sub">Ask for work, check on it, or take up what is waiting. It also answers on Discord and GitHub.</p>
                {needs.length > 0 ? (
                  <div className="starter-needs">
                    <p className="starter-label">Waiting on you</p>
                    <ul>
                      {needs.map((row) => (
                        <li key={row.id}>
                          <button
                            type="button"
                            className="starter-line"
                            onClick={() => {
                              setText(`About "${row.title}": `);
                              inputRef.current?.focus();
                            }}
                          >
                            <span className="starter-mark" aria-hidden="true">›</span>
                            <span className="starter-text">{row.title}</span>
                            <span className="starter-why">{row.waiting_for}</span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  </div>
                ) : null}
                <p className="starter-label">Or ask</p>
                <ul className="starter-list">
                  {STARTERS.map((starter) => (
                    <li key={starter}>
                      <button
                        type="button"
                        className="starter-line"
                        onClick={() => {
                          setText(starter);
                          inputRef.current?.focus();
                        }}
                      >
                        <span className="starter-mark" aria-hidden="true">›</span>
                        <span className="starter-text">{starter}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            ) : (
              messages.map((item, n) =>
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
                ) : item.kind === "report" ? (
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
                ) : item.kind === "note" ? (
                  <article className="turn agent note" key={`${item.at_ms}-${n}`}>
                    <div className="turn-head">
                      <strong>Agent</strong>
                      <span>{item.note.kind === "answered" ? "replied" : "no ticket"}</span>
                      <When ms={item.at_ms} now={now} />
                    </div>
                    {item.note.kind === "answered" && item.note.text ? (
                      <div className="prose">
                        <Markdown>{item.note.text}</Markdown>
                      </div>
                    ) : (
                      <div className="prose">
                        <p>{item.note.reason}</p>
                      </div>
                    )}
                  </article>
                ) : (
                  <article className="turn person pending" key={`${item.at_ms}-${n}`}>
                    <div className="turn-head">
                      <strong>You</strong>
                      <span>{notes.some((note) => note.at_ms >= item.message.at_ms) ? "sent" : "sending"}</span>
                      <When ms={item.at_ms} now={now} />
                    </div>
                    <div className="prose">
                      <p>{item.message.text}</p>
                    </div>
                  </article>
                ),
              )
            )}
          </div>

          <form className="composer live" onSubmit={onSubmit}>
            <label className="sr-only" htmlFor="chat-text">Message the agent</label>
            <textarea
              id="chat-text"
              ref={inputRef}
              value={text}
              onChange={(event) => setText(event.target.value)}
              onKeyDown={onKeyDown}
              placeholder="Message the swarm…"
              rows={2}
              maxLength={8000}
            />
            <div className="composer-side">
              <button type="submit" className="button primary" disabled={sending || text.trim().length === 0}>
                {sending ? "Sending…" : "Send"}
              </button>
              <span className="why">Enter to send · Shift+Enter for a new line</span>
            </div>
          </form>
          {error ? <p className="notice composer-error">{error}</p> : null}
        </section>

        <aside className="panel activity" aria-label="What is happening">
          <div className="panel-head">
            <h2>What&apos;s happening</h2>
            <span className="legend">{(index?.tickets ?? []).filter((row) => row.open).length} open</span>
          </div>
          <div className="activity-body">
            {ticket ? (
              <dl className="kv activity-kv">
                <dt>Stage</dt>
                <dd>{ticket.stage.replace("_", " ")}</dd>
                <dt>Worker</dt>
                <dd>{ticket.assignments.at(-1)?.state ?? "none yet"}</dd>
                <dt>Spent</dt>
                <dd>{usd(ticket.spent_microusd)}{ticket.budget_microusd ? ` of ${usd(ticket.budget_microusd)}` : ""}</dd>
                <dt>Open</dt>
                <dd>{duration(now - ticket.created_ms)}</dd>
              </dl>
            ) : (
              <p className="sub activity-note">A new conversation starts a Ticket as soon as the agent picks it up.</p>
            )}

            {lastReport ? (
              <div className="activity-block">
                <div className="activity-head">
                  <span>Last report</span>
                  <span>{lastReport.status}</span>
                </div>
                <p className="activity-quote">{lastReport.text}</p>
              </div>
            ) : null}

            {messages.length > 0 ? (
              <div className="activity-block">
                <div className="activity-head">
                  <span>Needs you</span>
                  <Link href="/command">All</Link>
                </div>
                {needs.length === 0 ? (
                  <p className="sub activity-note">Nothing is waiting on you.</p>
                ) : (
                  <ul className="activity-list">
                    {needs.map((row) => (
                      <li key={row.id} className="need">
                        <Link href={`/tickets/${row.id}`}>{row.title}</Link>
                        <span className="sub">{row.waiting_for}</span>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            ) : null}

            <div className="activity-block">
              <div className="activity-head">
                <span>External jobs</span>
                <Link href="/jobs">All</Link>
              </div>
              {(jobs?.jobs ?? []).length === 0 ? (
                <p className="sub activity-note">The Watchdog has no jobs configured.</p>
              ) : (
                <ul className="activity-list">
                  {[...(jobs?.jobs ?? [])]
                    .sort((a, b) => Number(a.status === "up") - Number(b.status === "up"))
                    .slice(0, 3)
                    .map((job) => (
                      <li key={job.name}>
                        <span>{job.name}</span>
                        <JobBadge status={job.status} />
                      </li>
                    ))}
                </ul>
              )}
            </div>
          </div>
        </aside>
      </div>
    </>
  );
}
