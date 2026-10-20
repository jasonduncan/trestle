import { describe, expect, it, vi } from "vitest";
import type { Descriptor } from "../src/protocol";
import { createTransport, fillPath, PathRefused } from "../src/transport";

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

// SR-05: every destination is confined to its prefix after substitution,
// before anything is dispatched.
const action: Descriptor = {
  interactionId: "deals.run.form", revision: "r-test", audience: "product", requiredFeatures: [], missingFeatures: [],
  operation: { id: "deals.run", version: "1", method: "POST", path: "/api/{id}/actions/run", pathParams: ["id"], body: "input", successStatus: 200, successBody: "output", errors: [400, 404] },
  input: { id: "deals.run.input", version: "1", schema: { fields: [{ name: "id", kind: "string", required: true, nullable: false }] } },
  output: { id: "deals.run.output", version: "1", schema: { fields: [] } },
  presentation: { title: "Run", submitLabel: "Run", fields: [] },
};
const withPath = (path: string): Descriptor => ({ ...action, operation: { ...action.operation, path, pathParams: [] } });
function spy() {
  const fetch = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => Response.json({}));
  return { fetch, transport: createTransport({ fetch: fetch as typeof globalThis.fetch, origin: "https://app.example.com" }) };
}

describe("request confinement", () => {
  const traversal = ["..", ".", "%2e%2e", "%2E%2E", "%2e.", ".%2E", "%2e"];

  it("refuses dot-segment path parameters before fetch", async () => {
    const { fetch, transport } = spy();
    for (const id of [".", ".."]) {
      expect(await transport.submit(action, { id }), id).toMatchObject({ kind: "refused" });
      expect(await transport.submit(action, {}, { path: { id } }), id).toMatchObject({ kind: "refused" });
      expect(() => fillPath("/api/{id}/actions/run", { id })).toThrow(PathRefused);
    }
    expect(fetch).not.toHaveBeenCalled();
    // Encoded forms are data, not traversal: their `%` is itself encoded.
    for (const id of traversal.filter((t) => t.includes("%"))) {
      expect(fillPath("/api/{id}/actions/run", { id })).toBe(`/api/${id.replace(/%/g, "%25")}/actions/run`);
    }
  });

  it("refuses descriptors and paths that resolve outside the prefix", async () => {
    const { fetch, transport } = spy();
    const escapes = [
      "/api/%2e%2e/admin/", "/api/%2E%2E/%2E%2E/metrics", "/api/%2e%2E/%2E%2e/%2e%2e/metrics", "/api/v1/../../metrics",
      "/api/./../x", "/api/%2e/x", "/api/.%2E/x", "/api/x/%2e%2e", "/apix/records", "/api", "/_meta/v1/manifest",
      "//evil.example/api/x", "https://evil.example/api/x", "/api\\..\\x", "api/x",
    ];
    for (const path of escapes) {
      expect(await transport.submit(withPath(path), {}), path).toMatchObject({ kind: "refused" });
    }
    for (const path of escapes.filter((p) => !p.startsWith("/_meta/"))) {
      expect(await transport.list(path), path).toMatchObject({ kind: "refused" });
      expect(await transport.send({ method: "GET", path }), path).toMatchObject({ status: 0, body: { code: "path_refused" } });
    }
    expect(await transport.getInteraction("..")).toMatchObject({ kind: "refused" });
    expect(fetch).not.toHaveBeenCalled();
    expect(await transport.getInteraction("%2e%2e")).toMatchObject({ kind: "response" });
    expect(fetch.mock.calls[0]![0]).toBe("/_meta/v1/interactions/%252e%252e");
  });

  it("sends ordinary encoded parameters unchanged, without following redirects", async () => {
    const { fetch, transport } = spy();
    for (const id of ["a b", "a/b", "a..b", "...", "café", "日本", "%2F"]) {
      expect(await transport.submit(action, { id }), id).toMatchObject({ kind: "response", status: 200 });
    }
    expect(fetch.mock.calls.map((c) => c[0])).toEqual([
      "/api/a%20b/actions/run", "/api/a%2Fb/actions/run", "/api/a..b/actions/run", "/api/.../actions/run",
      "/api/caf%C3%A9/actions/run", "/api/%E6%97%A5%E6%9C%AC/actions/run", "/api/%252F/actions/run",
    ]);
    expect(fetch.mock.calls.every((c) => c[1]?.redirect === "error")).toBe(true);
    await transport.list("/api/records", { q: "a b" });
    await transport.list("/_meta/v1/ui/manifest");
    expect(fetch.mock.calls.slice(-2).map((c) => c[0])).toEqual(["/api/records?q=a+b", "/_meta/v1/ui/manifest"]);
  });

  it("honours a configured API prefix on a segment boundary", async () => {
    const fetch = vi.fn(async () => Response.json({}));
    const transport = createTransport({ fetch: fetch as typeof globalThis.fetch, apiPrefix: "/v2" });
    expect(await transport.submit(withPath("/v2x/records"), {})).toMatchObject({ kind: "refused" });
    expect(await transport.submit(withPath("/v2/records"), {})).toMatchObject({ kind: "response" });
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
