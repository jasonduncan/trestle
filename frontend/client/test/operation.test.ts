import { describe, expect, it, vi } from "vitest";
import { OperationRuntime } from "../src/operation";
import type { Descriptor } from "../src/protocol";
import type { Reply, Transport } from "../src/transport";

const descriptor: Descriptor = {
  interactionId: "notes.create.form", revision: "r-test", audience: "product", requiredFeatures: [], missingFeatures: [],
  operation: { id: "notes.create", version: "1", method: "POST", path: "/api/notes", pathParams: [], body: "input", successStatus: 201, successBody: "output", errors: [400, 401, 403, 409, 422, 500] },
  input: { id: "notes.input", version: "1", schema: { fields: [{ name: "title", kind: "string", required: true, nullable: false }] } },
  output: { id: "notes.output", version: "1", schema: { fields: [{ name: "id", kind: "string", required: true, nullable: false }] } },
  presentation: { title: "Create note", submitLabel: "Create", fields: [] },
};
const response = (status: number, body: unknown): Reply => ({ kind: "response", status, body, bytes: 1 });
function setup(reply: () => Promise<Reply>) {
  const submit = vi.fn(reply);
  const transport = { submit } as unknown as Transport;
  const runtime = new OperationRuntime(), controller = runtime.operation("notes:create-flow", transport);
  return { submit, transport, runtime, controller };
}
describe("immutable single-dispatch attempts", () => {
  it("dispatches once even for concurrent execute calls and no server idempotency", async () => {
    let resolve!: (reply: Reply) => void;
    const { controller, submit } = setup(() => new Promise((r) => { resolve = r; }));
    const payload = { title: "First" }, d = structuredClone(descriptor);
    const attempt = controller.prepare(d, payload)!;
    payload.title = "Second"; d.operation.path = "/api/wrong";
    const first = controller.execute(attempt), second = controller.execute(attempt);
    await Promise.resolve();
    expect(submit).toHaveBeenCalledOnce();
    expect(submit.mock.calls[0]).toEqual([expect.objectContaining({ operation: expect.objectContaining({ path: "/api/notes" }) }), { title: "First" }, {}]);
    resolve(response(201, { id: "1" })); await Promise.all([first, second]);
    await controller.execute(attempt); expect(submit).toHaveBeenCalledOnce();
  });
  it("treats a destination refused before dispatch as a rejection, not an unknown outcome", async () => {
    const { controller } = setup(async () => ({ kind: "refused", message: "outside the API" }));
    await controller.execute(controller.prepare(descriptor, { title: "x" })!);
    expect(controller.getSnapshot()).toMatchObject({ state: "rejected", code: "path_refused" });
  });
  it.each([response(500, {}), response(201, null), response(403, { code: "permission_denied" }), { kind: "network-error", message: "lost" } as Reply])("retains unknown outcomes across remount, with no replay", async (reply) => {
    const { runtime, controller, transport, submit } = setup(async () => reply);
    await controller.execute(controller.prepare(descriptor, { title: "x" })!);
    expect(controller.getSnapshot().state).toBe("outcome_unknown");
    controller.dispose();
    const remounted = runtime.operation("notes:create-flow", transport);
    expect(remounted).toBe(controller);
    expect(remounted.prepare(descriptor, { title: "x" })).toBeUndefined();
    expect(runtime.getSnapshot()).toHaveLength(1); expect(submit).toHaveBeenCalledOnce();
  });
  it("recognizes an authoritative no-effect refusal and preserves field errors", async () => {
    const { controller } = setup(async () => response(422, { type: "urn:trestle:problem:validation", status: 422, code: "validation_failed", errors: [{ pointer: "/title", code: "required", message: "Required." }] }));
    await controller.execute(controller.prepare(descriptor, { title: "x" })!);
    expect(controller.getSnapshot().state).toBe("rejected"); expect(controller.getSnapshot().errors[0]?.pointer).toBe("/title");
    expect(controller.prepare(descriptor, { title: "corrected" })).toBeDefined();
  });
  it("invalidates prepared handles and discards late protected replies", async () => {
    let resolve!: (reply: Reply) => void;
    const { controller, runtime } = setup(() => new Promise((r) => { resolve = r; }));
    const handle = controller.prepare(descriptor, { title: "secret" })!;
    const executing = controller.execute(handle); await Promise.resolve();
    runtime.clear(); resolve(response(201, { id: "private" })); await executing;
    expect(controller.getSnapshot().result).toBeUndefined(); expect(runtime.getSnapshot()).toEqual([]);
    await expect(controller.execute(handle)).rejects.toThrow();
  });
  it("keeps unresolved tombstones through access refresh without retaining payloads or replaying", async () => {
    const { controller, runtime, transport, submit } = setup(async () => ({ kind: "network-error", message: "lost" }));
    const handle = controller.prepare(descriptor, { title: "secret" })!;
    await controller.execute(handle);
    runtime.refresh();
    const restored = runtime.operation("notes:create-flow", transport);
    expect(restored.getSnapshot()).toMatchObject({ state: "outcome_unknown", label: "Create note", attemptId: handle.id });
    expect(restored.prepare(descriptor, { title: "secret" })).toBeUndefined();
    await expect(restored.execute(handle)).rejects.toThrow();
    expect(submit).toHaveBeenCalledOnce();
  });
  it("a denied reconciliation read leaves the original outcome unresolved", async () => {
    const { controller, transport } = setup(async () => ({ kind: "network-error", message: "lost" }));
    await controller.execute(controller.prepare(descriptor, { title: "x" })!);
    transport.list = vi.fn(async () => response(403, { code: "permission_denied" }));
    const read = structuredClone(descriptor); read.operation.method = "GET"; read.operation.body = "none";
    await controller.reconcile(read, { title: "x" }, () => "no_effect");
    expect(controller.getSnapshot().state).toBe("outcome_unknown");
  });
  it.each([202, 204])("honors declared %s semantics", async (status) => {
    const d = structuredClone(descriptor); d.operation.successStatus = status; d.operation.successBody = status === 204 ? "none" : "output";
    const { controller } = setup(async () => response(status, status === 204 ? null : { id: "job-1" }));
    await controller.execute(controller.prepare(d, { title: "x" })!);
    expect(controller.getSnapshot().state).toBe(status === 204 ? "succeeded" : "queued");
  });
  it("settles only the accepted job identified by an authorized status read", async () => {
    const d = structuredClone(descriptor); d.operation.successStatus = 202;
    const { controller } = setup(async () => response(202, { id: "job-1" }));
    await controller.execute(controller.prepare(d, { title: "x" })!);
    controller.observeWork({ id: "job-2", state: "succeeded" });
    expect(controller.getSnapshot().state).toBe("queued");
    controller.observeWork({ id: "job-1", state: "succeeded" });
    expect(controller.getSnapshot().state).toBe("succeeded");
    expect(controller.prepare(d, { title: "new request" })).toBeDefined();
  });
});
