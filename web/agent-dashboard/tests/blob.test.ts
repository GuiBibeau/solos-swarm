import { beforeEach, describe, expect, it, vi } from "vitest";

const get = vi.fn();
vi.mock("@vercel/blob", () => ({ get }));

const { readIndex, readTicket, validTicketId, StoreError } = await import("@/lib/blob");
import type { Viewer } from "@/lib/viewer-core";

const viewer = { email: "g.bibeaulaviolette@gmail.com" } as Viewer;

function found(body: unknown) {
  return {
    statusCode: 200,
    stream: new Response(JSON.stringify(body)).body,
    blob: {},
  };
}

beforeEach(() => get.mockReset());

describe("reading the published snapshots", () => {
  it("reads the index and a ticket from the private store, and not from the cache", async () => {
    get.mockResolvedValueOnce(found({ v: 1, open: 0, tickets: [] }));
    expect(await readIndex(viewer)).toEqual({ v: 1, open: 0, tickets: [] });
    expect(get).toHaveBeenCalledWith("tickets/index.json", { access: "private", useCache: false });

    get.mockResolvedValueOnce(found({ v: 1, id: "tk_0123456789abcdef", title: "t" }));
    const ticket = await readTicket("tk_0123456789abcdef", viewer);
    expect(ticket?.title).toBe("t");
    expect(get).toHaveBeenLastCalledWith("tickets/tk_0123456789abcdef.json", { access: "private", useCache: false });
  });

  it("answers null for nothing published yet", async () => {
    get.mockResolvedValue(null);
    expect(await readIndex(viewer)).toBeNull();
    expect(await readTicket("tk_0123456789abcdef", viewer)).toBeNull();
  });

  it("does not touch the store for an id that is not a ticket id", async () => {
    for (const id of ["../index", "tk_1/../../x", "a b", "", "x".repeat(101), "tk_1.json"]) {
      expect(validTicketId(id), id).toBe(false);
      expect(await readTicket(id, viewer)).toBeNull();
    }
    expect(get).not.toHaveBeenCalled();
    expect(validTicketId("tk_0123456789abcdef")).toBe(true);
  });

  it("refuses a file in a format it does not know, or for another ticket", async () => {
    get.mockResolvedValueOnce(found({ v: 2, tickets: [] }));
    await expect(readIndex(viewer)).rejects.toBeInstanceOf(StoreError);
    get.mockResolvedValueOnce(found({ v: 1, id: "tk_other" }));
    await expect(readTicket("tk_0123456789abcdef", viewer)).rejects.toBeInstanceOf(StoreError);
  });

  it("turns a failed read into a StoreError that carries no credentials", async () => {
    get.mockRejectedValueOnce(new Error("boom"));
    await expect(readIndex(viewer)).rejects.toThrow(StoreError);
    get.mockResolvedValueOnce({ statusCode: 200, stream: new Response("not json").body, blob: {} });
    await expect(readIndex(viewer)).rejects.toThrow(/not JSON/);
  });
});
