// Reads the snapshots `agent publish` uploads, from a private Blob store, on the server. Credentials come from the
// environment as @vercel/blob resolves them: OIDC (VERCEL_OIDC_TOKEN with BLOB_STORE_ID) when the store is connected
// to the project, else BLOB_READ_WRITE_TOKEN. A private blob has no public URL; this is the only way to the data.
//
// Every function here takes a Viewer, which only a signed-in, allowed person's request can make (viewer.ts).

import { get } from "@vercel/blob";
import { FORMAT, JOBS_FORMAT, type Index, type Jobs, type TicketSnapshot } from "./types";
import type { Viewer } from "./viewer-core";

export const INDEX_PATH = "tickets/index.json";
export const JOBS_PATH = "jobs/index.json";

/** Ticket ids are `tk_` and hex; anything else is not a path into the store. */
export function validTicketId(id: string): boolean {
  return /^[A-Za-z0-9_-]{1,100}$/.test(id);
}

export class StoreError extends Error {}

async function readJson(pathname: string): Promise<unknown | null> {
  let result;
  try {
    // useCache: false, because a blob is overwritten at the same pathname and the CDN may serve the old one for a
    // minute. These are small files read by one person.
    result = await get(pathname, { access: "private", useCache: false });
  } catch (error) {
    throw new StoreError(`cannot read ${pathname} from the Blob store: ${(error as Error).message}`);
  }
  if (!result || result.statusCode !== 200) return null;
  try {
    return JSON.parse(await new Response(result.stream).text());
  } catch {
    throw new StoreError(`${pathname} is not JSON`);
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The index, or null when nothing has been published yet. */
export async function readIndex(viewer: Viewer): Promise<Index | null> {
  void viewer;
  const json = await readJson(INDEX_PATH);
  if (json === null) return null;
  if (!isRecord(json) || json.v !== FORMAT || !Array.isArray(json.tickets)) {
    throw new StoreError(`${INDEX_PATH} is in a format this dashboard does not know`);
  }
  return json as unknown as Index;
}

/** The external jobs, or null when the Watchdog has none to show (or the publisher has not uploaded them). */
export async function readJobs(viewer: Viewer): Promise<Jobs | null> {
  void viewer;
  const json = await readJson(JOBS_PATH);
  if (json === null) return null;
  if (!isRecord(json) || json.v !== JOBS_FORMAT || !Array.isArray(json.jobs)) {
    throw new StoreError(`${JOBS_PATH} is in a format this dashboard does not know`);
  }
  return json as unknown as Jobs;
}

/** One ticket's snapshot, or null when there is none (yet) or the id is not one. */
export async function readTicket(id: string, viewer: Viewer): Promise<TicketSnapshot | null> {
  void viewer;
  if (!validTicketId(id)) return null;
  const json = await readJson(`tickets/${id}.json`);
  if (json === null) return null;
  if (!isRecord(json) || json.v !== FORMAT || json.id !== id) {
    throw new StoreError(`tickets/${id}.json is in a format this dashboard does not know`);
  }
  return json as unknown as TicketSnapshot;
}
