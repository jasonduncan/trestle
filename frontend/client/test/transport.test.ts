import { expect, it, vi } from "vitest";
import { createTransport } from "../src/transport";

it("discards a prior access generation even when cancellation cannot stop its reply", async () => {
  let resolve!: (response: Response) => void;
  const fetch = vi.fn(() => new Promise<Response>((r) => { resolve = r; }));
  const transport = createTransport({ fetch: fetch as typeof globalThis.fetch });
  const read = transport.list("/api/records");
  transport.invalidate?.();
  resolve(Response.json({ private: "old tenant" }));
  expect(await read).toMatchObject({ kind: "network-error", message: "Access context changed" });
  expect(fetch.mock.calls).toHaveLength(1);
});
