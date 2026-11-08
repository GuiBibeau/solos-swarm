// The web chat's live side: read the conversation and the command state for a thread, and send a message.
// Everything is behind the same session and allowlist check as the pages; the message is stored by the server, so the
// browser never holds the Blob token.

import { appendMessage, ChatStoreError, MAX_TEXT, messageFrom } from "@/lib/chat";
import { readIndex, readJobs, readTicket } from "@/lib/blob";
import { THREAD_RE, threadOf } from "@/lib/threads";
import { currentViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";

const NO_STORE = { "content-type": "application/json", "cache-control": "no-store" };
const ok = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: NO_STORE });
const bad = (status: number, error: string) => new Response(JSON.stringify({ error }), { status, headers: NO_STORE });

export async function GET(request: Request): Promise<Response> {
  const viewer = await currentViewer();
  if (!viewer) return bad(401, "Sign in to read the chat.");
  const thread = new URL(request.url).searchParams.get("t");
  let index = null;
  let jobs = null;
  try {
    index = await readIndex(viewer);
  } catch (error) {
    console.error(error);
  }
  try {
    jobs = await readJobs(viewer);
  } catch (error) {
    console.error(error);
  }
  let ticket = null;
  if (thread) {
    const row = index?.tickets.find((candidate) => threadOf(candidate) === thread);
    if (row) {
      try {
        ticket = await readTicket(row.id, viewer);
      } catch (error) {
        console.error(error);
      }
    }
  }
  return ok({ thread, ticket, index, jobs, author: viewer.email });
}

export async function POST(request: Request): Promise<Response> {
  const viewer = await currentViewer();
  if (!viewer) return bad(401, "Sign in to send a message.");
  let body: unknown;
  try {
    body = await request.json();
  } catch {
    return bad(400, "That was not a message.");
  }
  const data = body as { thread?: unknown; text?: unknown };
  const thread = typeof data.thread === "string" ? data.thread : "";
  const text = typeof data.text === "string" ? data.text.trim() : "";
  if (!THREAD_RE.test(thread)) return bad(400, "That conversation id is not one this dashboard made.");
  if (text.length === 0) return bad(400, "Write something first.");
  if (text.length > MAX_TEXT) return bad(400, `A message is at most ${MAX_TEXT} characters.`);
  const message = messageFrom(viewer, thread, text);
  try {
    await appendMessage(message);
  } catch (error) {
    console.error(error);
    if (error instanceof ChatStoreError) return bad(502, "The message could not be stored; try again.");
    return bad(500, "The message could not be stored; try again.");
  }
  return ok({ ok: true, message });
}
