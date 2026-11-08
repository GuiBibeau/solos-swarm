// The pages, rendered to HTML from the snapshots `agent publish` really builds (tests/fixtures are written by
// tests/agent_publish.rs in the Rust crate, see dump_fixtures there), with the viewer and the Blob store stubbed.

import { readFileSync } from "node:fs";
import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Index, Jobs, TicketSnapshot } from "@/lib/types";

const fixture = <T,>(name: string): T =>
  JSON.parse(readFileSync(new URL(`./fixtures/${name}.json`, import.meta.url), "utf8")) as T;

const index = fixture<Index>("mix-index");
// Written by the_jobs_blob_has_the_shape_the_dashboard_reads_and_feeds_its_fixtures in tests/agent_publish.rs.
const jobs = fixture<Jobs>("jobs");
const tickets = [0, 1, 2].map((n) => fixture<TicketSnapshot>(`mix-${n}`));
const byTitle = (title: string) => {
  const found = tickets.find((ticket) => ticket.title === title);
  if (!found) throw new Error(`no fixture ticket ${title}`);
  return found;
};
// Five minutes after the fixture index was written.
const NOW = Date.parse(index.generated_at ?? "") + 5 * 60 * 1000;

const readIndex = vi.fn();
const readTicket = vi.fn();
const readJobs = vi.fn();
vi.mock("@/lib/viewer", () => ({
  requireViewer: async () => ({ email: "g.bibeaulaviolette@gmail.com" }),
  currentViewer: async () => null,
}));
vi.mock("@/lib/clock", () => ({ nowMs: () => NOW }));
vi.mock("@/components/refresh", () => ({ Refresh: () => null }));
vi.mock("@/lib/blob", async () => ({
  ...(await vi.importActual<typeof import("@/lib/blob")>("@/lib/blob")),
  readIndex,
  readTicket,
  readJobs,
}));
vi.mock("next/navigation", () => ({
  redirect: (to: string) => {
    throw new Error(`redirect:${to}`);
  },
  notFound: () => {
    throw new Error("not-found");
  },
}));

const { default: Command } = await import("@/app/command/page");
const { default: Tickets } = await import("@/app/tickets/page");
const { default: TicketPage } = await import("@/app/tickets/[id]/page");
const { default: Chat } = await import("@/app/page");
const { default: ChatRedirect } = await import("@/app/chat/page");
const { default: Swarm } = await import("@/app/swarm/page");
const { default: JobsPage } = await import("@/app/jobs/page");
const { default: Login } = await import("@/app/login/page");

beforeEach(() => {
  readIndex.mockReset();
  readTicket.mockReset();
  readJobs.mockReset();
  readJobs.mockResolvedValue(null);
  vi.spyOn(console, "error").mockImplementation(() => {});
});

const html = async (element: Promise<React.ReactElement>) => renderToStaticMarkup(await element);
const page = (id: string) => TicketPage({ params: Promise.resolve({ id }) });

describe("the command center", () => {
  it("answers what the swarm is doing with the field, what needs a person, the numbers and the latest tickets", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockResolvedValue(jobs);
    const out = await html(Command());
    expect(out).toContain("<h1>Command</h1>");
    expect(out).toContain('role="img"');
    expect(out).toContain('aria-label="The swarm:');
    expect(out).toContain("Needs you");
    expect(out).toContain("Which staging environment should I rotate: eu or us?");
    expect(out).toContain("needs an answer");
    expect(out).toContain("Open tickets");
    expect(out).toContain("Committed");
    expect(out).toContain("Recent tickets");
    for (const row of index.tickets) expect(out).toContain(`href="/tickets/${row.id}"`);
    expect(out).toContain("alice");
    expect(out).not.toContain("may be old");
  });

  it("says so when the publisher has gone quiet", async () => {
    readIndex.mockResolvedValue({ ...index, generated_at: "2026-11-06T13:00:00Z" });
    expect(await html(Command())).toContain("may be old");
  });

  it("says nothing has been published, shows no error text from storage, and survives a failed read", async () => {
    readIndex.mockResolvedValue(null);
    expect(await html(Command())).toContain("Nothing has been published yet");
    readIndex.mockRejectedValue(new Error("token vercel_blob_rw_secret leaked"));
    const out = await html(Command());
    expect(out).toContain("could not be read from storage");
    expect(out).not.toContain("vercel_blob_rw_secret");
  });
});

describe("the external jobs", () => {
  it("lists every job with how it is checked, the ones in trouble first with what was found", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockResolvedValue(jobs);
    const out = await html(Command());
    expect(readJobs).toHaveBeenCalledWith(expect.anything());
    expect(out).toContain("External jobs");
    expect(out).toContain("3 in trouble");
    // The command center shows the three jobs in trouble and links to the full list.
    for (const job of jobs.jobs.filter((row) => row.status !== "up")) {
      expect(out, job.name).toContain(`<span class="name">${job.name}</span>`);
      expect(out, job.check).toContain(job.check);
    }
    expect(out).toContain('href="/jobs"');
    const at = (name: string) => out.indexOf(`<span class="name">${name}</span>`);
    expect(at("data-build")).toBeLessThan(at("modal-wave-3"));
    expect(at("modal-wave-3")).toBeLessThan(at("l2book-archive"));
    for (const label of ["down", "stalled", "low on disk"]) expect(out).toContain(`>${label}</span>`);
    expect(out).toContain("can&#x27;t find session: data-build");
    expect(out).toContain("no new shard for 40m");
    expect(out).toContain("11.2 GB free on /data/l2book (floor 50.0 GB)");
    expect(out).not.toContain("has not looked");
  });

  it("says so when the Watchdog has not looked for a long time", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockResolvedValue({ ...jobs, checked_ms: NOW - 40 * 60 * 1000 });
    const out = await html(Command());
    expect(out).toContain("The Watchdog has not looked at these jobs for 40m");
  });

  it("keeps the frame and says so when no job is published", async () => {
    readIndex.mockResolvedValue(index);
    expect(await html(Command())).toContain("The Watchdog has no external jobs configured.");
    readJobs.mockResolvedValue({ ...jobs, jobs: [] });
    expect(await html(Command())).toContain("The Watchdog has no external jobs configured.");
  });

  it("does not let a failed read of the jobs hide the tickets, or show what storage said", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockRejectedValue(new Error("token vercel_blob_rw_secret leaked"));
    const out = await html(Command());
    expect(out).toContain("The external jobs could not be read from storage.");
    expect(out).not.toContain("vercel_blob_rw_secret");
    expect(out).toContain("Rotate the staging keys");
  });

  it("shows what a job's command printed as text, never as markup", async () => {
    readIndex.mockResolvedValue(index);
    const hostile = { ...jobs.jobs[1], detail: '<script>alert(1)</script><img src=x onerror="y">' };
    readJobs.mockResolvedValue({ ...jobs, jobs: [hostile] });
    const out = await html(Command());
    expect(out).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
    for (const bad of ["<script", "<img"]) expect(out, bad).not.toContain(bad);
  });

  it("the jobs page lists them all with their state and check", async () => {
    readJobs.mockResolvedValue(jobs);
    const out = await html(JobsPage());
    expect(out).toContain("<h1>Jobs</h1>");
    for (const job of jobs.jobs) expect(out).toContain(job.name);
    expect(out).toContain("3 in trouble");
    expect(out).toContain("active (running)");
  });
});

describe("the ticket list", () => {
  it("lists the open ticket first, with what it waits for, and each ticket links to its page", async () => {
    readIndex.mockResolvedValue(index);
    const out = await html(Tickets());
    expect(out.indexOf('id="open-title"')).toBeGreaterThan(-1);
    expect(out.indexOf('id="open-title"')).toBeLessThan(out.indexOf('id="ended-title"'));
    expect(out.indexOf("Rotate the staging keys")).toBeLessThan(out.indexOf("Read my notifications"));
    expect(out).toContain("Which staging environment should I rotate: eu or us?");
    expect(out).toContain("needs an answer");
    expect(out).toContain("completed");
    for (const row of index.tickets) expect(out).toContain(`href="/tickets/${row.id}"`);
    expect(out).toContain("alice");
    expect(out).toContain("1 open");
  });

  it("says nothing has been published, shows no error text from storage, and survives a failed read", async () => {
    readIndex.mockResolvedValue(null);
    expect(await html(Tickets())).toContain("No tickets have been published yet.");
    readIndex.mockRejectedValue(new Error("token vercel_blob_rw_secret leaked"));
    const out = await html(Tickets());
    expect(out).toContain("could not be read from storage");
    expect(out).not.toContain("vercel_blob_rw_secret");
  });
});

describe("the chat page", () => {
  const thread = `wc_${"a".repeat(32)}`;
  const webRow = {
    ...index.tickets[0],
    id: "tk_web000000000000",
    title: "Move the release notes",
    source: "web",
    source_url: `https://agent.guibibeau.com/?t=${thread}`,
  };

  it("renders a conversation from its thread, with the transcript and the activity beside it", async () => {
    readIndex.mockResolvedValue({ ...index, tickets: [webRow, ...index.tickets] });
    const ticket = byTitle("Summarise the incident report");
    readTicket.mockResolvedValue({ ...ticket, id: webRow.id, title: webRow.title, source: "web", source_url: webRow.source_url });
    const out = await html(Chat({ searchParams: Promise.resolve({ t: thread }) }));
    expect(out).toContain("<h1>Chat</h1>");
    expect(out).toContain("Move the release notes");
    expect(out).toContain("Looking into it.");
    expect(out).toContain("<strong>retry test</strong>");
    expect(out).toContain("What&#x27;s happening");
    expect(out).toContain("Message the swarm…");
    for (const bad of ["<script", "<img", "onerror"]) expect(out, bad).not.toContain(bad);
  });

  it("starts a new conversation with the starters when the thread has no ticket yet", async () => {
    // The fixture index has one web conversation; this case is the empty list.
    readIndex.mockResolvedValue({
      ...index,
      tickets: index.tickets.filter((row) => row.source !== "web"),
    });
    const out = await html(Chat({ searchParams: Promise.resolve({ t: thread }) }));
    expect(out).toContain("New conversation");
    expect(out).toContain("What needs me right now?");
    expect(out).toContain("No web conversation yet");
  });

  it("sends an old /chat link to the chat, which is the home now", () => {
    expect(() => ChatRedirect()).toThrow("redirect:/");
  });
});

describe("the swarm page", () => {
  it("draws the field and the worker list from the index", async () => {
    readIndex.mockResolvedValue(index);
    const out = await html(Swarm());
    expect(out).toContain("<h1>Swarm</h1>");
    expect(out).toContain('role="img"');
    expect(out).toContain("Workers");
    expect(out).toContain("Open tickets");
    expect(out).toContain("Rotate the staging keys");
  });
});

describe("a ticket's page", () => {
  it("shows the request, the agent's replies as rendered markdown and the work, and nothing a model could script", async () => {
    const ticket = byTitle("Summarise the incident report");
    readTicket.mockResolvedValue(ticket);
    const out = await html(page(ticket.id));
    expect(readTicket).toHaveBeenCalledWith(ticket.id, expect.anything());
    expect(out).toContain("<h1>Summarise the incident report</h1>");
    expect(out).toContain("Looking into it.");
    expect(out).toContain("CI failed on main: test_retry timed out");
    expect(out).toContain("<h2>Incident summary</h2>");
    expect(out).toContain("<strong>retry test</strong>");
    expect(out).toContain('<div class="md-scroll"><table');
    expect(out).toContain('href="https://example.com/acme/app/pull/7" target="_blank" rel="noopener noreferrer nofollow"');
    expect(out).toContain("[image: chart]");
    expect(out).toContain("replied");
    for (const bad of ["<script", "<img", "onerror", "javascript:"]) expect(out, bad).not.toContain(bad);
    expect(out).toContain('id="work-title"');
    expect(out).toContain('id="decisions-title"');
    expect(out).not.toContain("not delivered yet");
  });

  it("shows what a ticket waits for, and a browser trail without queries", async () => {
    const asking = byTitle("Rotate the staging keys");
    readTicket.mockResolvedValue(asking);
    const waiting = await html(page(asking.id));
    expect(waiting).toContain("Waiting for a person.");
    expect(waiting).toContain("needs an answer");

    const browsing = byTitle("Read my notifications");
    readTicket.mockResolvedValue(browsing);
    const out = await html(page(browsing.id));
    expect(out).toContain('id="browser-title"');
    expect(out).toContain("2 steps");
    expect(out).toContain('href="https://example.com/reset"');
    expect(out).not.toContain("SECRETVALUE");
  });

  it("answers an id that is not one with not-found, without asking the store", async () => {
    await expect(page("../index")).rejects.toThrow("not-found");
    expect(readTicket).not.toHaveBeenCalled();
  });

  it("says a ticket has not reached the dashboard yet, and does not break on a failed read", async () => {
    readTicket.mockResolvedValue(null);
    expect(await html(page("tk_0123456789abcdef"))).toContain("has not reached the dashboard yet");
    readTicket.mockRejectedValue(new Error("secret detail"));
    const out = await html(page("tk_0123456789abcdef"));
    expect(out).toContain("could not be read from storage");
    expect(out).not.toContain("secret detail");
  });

  it("shows an undelivered reply as undelivered", async () => {
    const ticket = structuredClone(byTitle("Summarise the incident report"));
    const reply = ticket.conversation.find((turn) => turn.from === "agent");
    if (!reply) throw new Error("no reply in the fixture");
    reply.delivery = { delivered: false, attempts: 2, error: "rate limited" };
    readTicket.mockResolvedValue(ticket);
    expect(await html(page(ticket.id))).toContain("not delivered yet (2 failed): rate limited");
  });
});

describe("the sign-in page", () => {
  it("offers Sign in with Vercel, and prints a message of its own for an error code and never the code", async () => {
    const out = await html(Login({ searchParams: Promise.resolve({ error: "forbidden", next: "/tickets/tk_1" }) }));
    expect(out).toContain("Sign in with Vercel");
    expect(out).toContain('href="/auth/login?next=%2Ftickets%2Ftk_1"');
    expect(out).toContain("not allowed here");
    const odd = await html(Login({ searchParams: Promise.resolve({ error: "<script>x</script>", next: "//evil.example" }) }));
    expect(odd).not.toContain("<script>x");
    expect(odd).toContain('href="/auth/login?next=%2F"');
  });
});
