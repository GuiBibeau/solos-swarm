import { ChatLive } from "@/components/chat-live";
import { readIndex, readJobs, readNotes, readTicket } from "@/lib/blob";
import { nowMs } from "@/lib/clock";
import { threadOf } from "@/lib/threads";
import type { Note } from "@/lib/types";
import { requireViewer } from "@/lib/viewer";

export const dynamic = "force-dynamic";
export const metadata = { title: "Chat" };

type Props = { searchParams: Promise<{ t?: string }> };

export default async function ChatPage({ searchParams }: Props) {
  const viewer = await requireViewer();
  const { t } = await searchParams;
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
  let threadNotes: Note[] = [];
  try {
    const notes = await readNotes(viewer);
    threadNotes = t ? (notes?.notes ?? []).filter((note) => note.thread === `web:${t}`) : [];
  } catch (error) {
    console.error(error);
  }
  let ticket = null;
  if (t) {
    const row = index?.tickets.find((candidate) => threadOf(candidate) === t);
    if (row) {
      try {
        ticket = await readTicket(row.id, viewer);
      } catch (error) {
        console.error(error);
      }
    }
  }
  return (
    <ChatLive
      initialThread={t ?? null}
      initialTicket={ticket}
      initialIndex={index}
      initialJobs={jobs}
      initialNotes={threadNotes}
      initialNow={nowMs()}
    />
  );
}
