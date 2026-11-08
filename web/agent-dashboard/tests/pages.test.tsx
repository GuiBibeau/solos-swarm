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

const { default: Tickets } = await import("@/app/page");
const { default: TicketPage } = await import("@/app/tickets/[id]/page");
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

describe("the ticket list", () => {
  it("lists the open ticket first, with what it waits for, and each ticket links to its page", async () => {
    readIndex.mockResolvedValue(index);
    const out = await html(Tickets());
    expect(out.indexOf("<h2>Open</h2>")).toBeGreaterThan(-1);
    expect(out.indexOf("<h2>Open</h2>")).toBeLessThan(out.indexOf("<h2>Recently ended</h2>"));
    expect(out.indexOf("Rotate the staging keys")).toBeLessThan(out.indexOf("Read my notifications"));
    expect(out).toContain("Which staging environment should I rotate: eu or us?");
    expect(out).toContain("needs an answer");
    expect(out).toContain("completed");
    for (const row of index.tickets) expect(out).toContain(`href="/tickets/${row.id}"`);
    expect(out).toContain("alice");
    expect(out).toContain("1 open");
    expect(out).not.toContain("may be down");
  });

  it("says so when the publisher has gone quiet", async () => {
    readIndex.mockResolvedValue({ ...index, generated_at: "2026-11-06T13:00:00Z" });
    expect(await html(Tickets())).toContain("may be down");
  });

  it("says nothing has been published, shows no error text from storage, and survives a failed read", async () => {
    readIndex.mockResolvedValue(null);
    expect(await html(Tickets())).toContain("Nothing has been published yet");
    readIndex.mockRejectedValue(new Error("token vercel_blob_rw_secret leaked"));
    const out = await html(Tickets());
    expect(out).toContain("could not be read from storage");
    expect(out).not.toContain("vercel_blob_rw_secret");
  });
});

describe("the external jobs", () => {
  it("lists every job with how it is checked, the ones in trouble first with what was found, above the tickets", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockResolvedValue(jobs);
    const out = await html(Tickets());
    expect(readJobs).toHaveBeenCalledWith(expect.anything());
    expect(out).toContain("<h2>External jobs</h2>");
    expect(out).toContain("1 of 4 up");
    for (const job of jobs.jobs) {
      expect(out, job.name).toContain(`<span>${job.name}</span>`);
      expect(out, job.check).toContain(job.check);
    }
    const at = (name: string) => out.indexOf(`<span>${name}</span>`);
    expect(at("data-build")).toBeLessThan(at("modal-wave-3"));
    expect(at("modal-wave-3")).toBeLessThan(at("l2book-archive"));
    expect(at("l2book-archive")).toBeLessThan(at("l2book"));
    for (const label of ["down", "stalled", "low on disk", "up"]) expect(out).toContain(`>${label}</span>`);
    expect(out).toContain("can&#x27;t find session: data-build");
    expect(out).toContain("no new shard for 40m");
    expect(out).toContain("11.2 GB free on /data/l2book (floor 50.0 GB)");
    expect(out).toContain("active (running)");
    expect(out.indexOf("External jobs")).toBeLessThan(out.indexOf("<h2>Open</h2>"));
    expect(out).not.toContain("has not looked");
  });

  it("says so when the Watchdog has not looked for a long time", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockResolvedValue({ ...jobs, checked_ms: NOW - 40 * 60 * 1000 });
    const out = await html(Tickets());
    expect(out).toContain("The Watchdog has not looked at these jobs for 40m");
  });

  it("shows nothing for jobs when none are published or none are registered", async () => {
    readIndex.mockResolvedValue(index);
    expect(await html(Tickets())).not.toContain("External jobs");
    readJobs.mockResolvedValue({ ...jobs, jobs: [] });
    expect(await html(Tickets())).not.toContain("External jobs");
  });

  it("does not let a failed read of the jobs hide the tickets, or show what storage said", async () => {
    readIndex.mockResolvedValue(index);
    readJobs.mockRejectedValue(new Error("token vercel_blob_rw_secret leaked"));
    const out = await html(Tickets());
    expect(out).toContain("The external jobs could not be read from storage.");
    expect(out).not.toContain("vercel_blob_rw_secret");
    expect(out).toContain("Rotate the staging keys");
  });

  it("shows what a job's command printed as text, never as markup", async () => {
    readIndex.mockResolvedValue(index);
    const hostile = { ...jobs.jobs[1], detail: '<script>alert(1)</script><img src=x onerror="y">' };
    readJobs.mockResolvedValue({ ...jobs, jobs: [hostile] });
    const out = await html(Tickets());
    expect(out).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
    for (const bad of ["<script", "<img"]) expect(out, bad).not.toContain(bad);
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
    expect(out).toContain("<h2>Work</h2>");
    expect(out).toContain("Decisions");
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
    expect(out).toContain("<h2>Browser</h2>");
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
