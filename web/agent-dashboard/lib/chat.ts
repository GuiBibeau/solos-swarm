// The web chat's transport: the signed-in person's messages, written to the private Blob store for the publisher
// to carry into the agent. The agent's replies come back through the published ticket snapshots, not through here.
// Server only: the Blob token and the local fixture fallback must never reach a client bundle.

import { randomBytes } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { get, put } from "@vercel/blob";
import { FIXTURES } from "./fixtures";
import type { ChatMessage } from "./types";
import type { Viewer } from "./viewer-core";

export const INBOX_PATH = "web/inbox.json";
export const INBOX_FORMAT = 1;
export const MAX_MESSAGES = 200;
export const MAX_TEXT = 8000;

export type ChatInbox = { v: number; messages: ChatMessage[] };

export class ChatStoreError extends Error {}

/** A new client-side conversation id. */
export function newThread(): string {
  return `wc_${randomBytes(16).toString("hex")}`;
}

/** The message id shape the publisher's bridge expects. */
export function newMessageId(): string {
  return `wm_${randomBytes(16).toString("hex")}`;
}

function isMessage(value: unknown): value is ChatMessage {
  if (typeof value !== "object" || value === null) return false;
  const m = value as Record<string, unknown>;
  return (
    typeof m.id === "string" &&
    typeof m.at_ms === "number" &&
    typeof m.author === "string" &&
    typeof m.text === "string" &&
    typeof m.thread === "string"
  );
}

/** Development only: the preview keeps its inbox in a temp file, seeded empty. */
function fixtureInbox(): ChatInbox {
  const file = path.join(tmpdir(), "solos-web-inbox.json");
  try {
    const parsed = JSON.parse(readFileSync(file, "utf8")) as ChatInbox;
    if (parsed.v === INBOX_FORMAT && Array.isArray(parsed.messages)) return parsed;
  } catch {
    // no preview inbox yet
  }
  return { v: INBOX_FORMAT, messages: [] };
}

/** The inbox as it is, or an empty one before the first message. */
export async function readInbox(): Promise<ChatInbox> {
  if (FIXTURES) return fixtureInbox();
  let result;
  try {
    result = await get(INBOX_PATH, { access: "private", useCache: false });
  } catch (error) {
    throw new ChatStoreError(`cannot read ${INBOX_PATH}: ${(error as Error).message}`);
  }
  if (!result || result.statusCode !== 200) return { v: INBOX_FORMAT, messages: [] };
  try {
    const json = JSON.parse(await new Response(result.stream).text()) as ChatInbox;
    if (json.v !== INBOX_FORMAT || !Array.isArray(json.messages)) {
      throw new ChatStoreError(`${INBOX_PATH} is in a format this dashboard does not know`);
    }
    return { v: INBOX_FORMAT, messages: json.messages.filter(isMessage) };
  } catch (error) {
    if (error instanceof ChatStoreError) throw error;
    throw new ChatStoreError(`${INBOX_PATH} is not JSON`);
  }
}

/** Append one message, keeping the last {@link MAX_MESSAGES}; the publisher reads the file's tail. */
export async function appendMessage(message: ChatMessage): Promise<void> {
  const inbox = await readInbox();
  const next: ChatInbox = { v: INBOX_FORMAT, messages: [...inbox.messages, message].slice(-MAX_MESSAGES) };
  if (FIXTURES) {
    writeFileSync(path.join(tmpdir(), "solos-web-inbox.json"), JSON.stringify(next));
    return;
  }
  try {
    await put(INBOX_PATH, JSON.stringify(next), {
      access: "private",
      addRandomSuffix: false,
      allowOverwrite: true,
      contentType: "application/json",
    });
  } catch (error) {
    throw new ChatStoreError(`cannot write ${INBOX_PATH}: ${(error as Error).message}`);
  }
}

/** The message a signed-in person is sending. */
export function messageFrom(viewer: Viewer, thread: string, text: string): ChatMessage {
  return {
    id: newMessageId(),
    at_ms: Date.now(),
    author: viewer.email,
    ...(viewer.name ? { name: viewer.name } : {}),
    text,
    thread,
  };
}
