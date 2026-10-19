// One runtime owns attempts across forms, dialogs and navigation. Prepared
// handles are opaque and single-use; reconciliation is read-only and optional.
import type { Descriptor } from "./protocol";
import { asProblem, fillPath, type RecordContext, type Reply, type Transport } from "./transport";
import { validateInstance, type FieldError } from "./validate";

export type OperationState = "ready" | "validating" | "submitting" | "succeeded" | "rejected" | "conflicted" | "outcome_unknown" | "committed_unreadable" | "queued";
export interface OperationSnapshot {
  state: OperationState;
  errors: FieldError[];
  message: string;
  result?: unknown;
  etag?: string;
  location?: string;
  requestId?: string;
  code?: string;
  dispatchedAt?: number;
  label: string;
  attemptId?: string;
}
export interface PreparedAttempt { readonly id: string }
interface Attempt {
  transport: Transport;
  descriptor: Descriptor;
  input: Record<string, unknown>;
  record?: RecordContext;
  generation: number;
  idempotencyKey?: string;
  dispatched?: Promise<void>;
}
const unresolved = (state: OperationState) => ["submitting", "outcome_unknown", "committed_unreadable", "queued"].includes(state);
const UNKNOWN = "We couldn't confirm whether this was saved. Inspect the record before taking further action. This attempt will not be sent again.";

function clone<T>(value: T): T { return structuredClone(value); }
function freeze<T>(value: T): T {
  if (value && typeof value === "object") {
    for (const member of Object.values(value)) freeze(member);
    Object.freeze(value);
  }
  return value;
}

export class OperationRuntime {
  private generation = 0;
  private controllers = new Map<string, OperationController>();
  private listeners = new Set<() => void>();
  private activity: readonly OperationSnapshot[] = [];
  epoch = () => this.generation;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  getSnapshot = () => this.activity;
  changed = () => {
    this.activity = [...this.controllers.values()].map((c) => c.getSnapshot()).filter((s) => unresolved(s.state) || !!(s.result && typeof s.result === "object" && "reveal_once" in s.result));
    this.listeners.forEach((listener) => listener());
  };
  /** Call on sign-out, tenant/actor change and permission refresh/refusal. */
  clear() {
    this.generation++;
    this.controllers.forEach((controller) => controller.invalidate());
    this.controllers.clear();
    this.changed();
  }
  dismissResult(snapshot: OperationSnapshot) { this.controllers.forEach((c) => { if (c.getSnapshot() === snapshot) c.dismissResult(); }); }
  refresh() {
    this.generation++;
    for (const [key, controller] of this.controllers) {
      if (unresolved(controller.getSnapshot().state)) controller.invalidate(true);
      else { controller.invalidate(); this.controllers.delete(key); }
    }
    this.changed();
  }
  operation(key: string, transport: Transport): OperationController {
    let controller = this.controllers.get(key);
    if (!controller) {
      controller = new OperationController(this, transport);
      this.controllers.set(key, controller);
    }
    controller.rebind(transport);
    return controller;
  }
}

const runtimes = new WeakMap<Transport, OperationRuntime>();
export function bindOperationRuntime(transport: Transport, runtime: OperationRuntime) { runtimes.set(transport, runtime); }
/** A host supplies one stable transport per security generation. */
export function operationRuntime(transport: Transport): OperationRuntime {
  let runtime = runtimes.get(transport);
  if (!runtime) { runtime = new OperationRuntime(); runtimes.set(transport, runtime); }
  return runtime;
}

export class OperationController {
  private snapshot: OperationSnapshot = { state: "ready", errors: [], message: "", label: "" };
  private listeners = new Set<() => void>();
  private attempts = new WeakMap<PreparedAttempt, Attempt>();
  private prepared?: PreparedAttempt;
  private valid = true;
  constructor(private runtime: OperationRuntime, private transport: Transport) {}
  getSnapshot = () => this.snapshot;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private set(next: Partial<OperationSnapshot>) {
    this.snapshot = freeze({ ...this.snapshot, ...next });
    this.listeners.forEach((listener) => listener());
    this.runtime.changed();
  }
  rebind(transport: Transport) { this.transport = transport; this.valid = true; }
  invalidate(preserveUnresolved = false) {
    this.valid = false; this.prepared = undefined; this.attempts = new WeakMap();
    this.set({ state: preserveUnresolved ? "outcome_unknown" : "rejected", errors: [], message: preserveUnresolved ? UNKNOWN : "Access context changed. Reload before continuing.", result: undefined, label: preserveUnresolved ? this.snapshot.label : "", etag: undefined, location: undefined, requestId: preserveUnresolved ? this.snapshot.requestId : undefined });
  }
  prepare(descriptor: Descriptor, input: unknown, record?: RecordContext): PreparedAttempt | undefined {
    if (!this.valid || unresolved(this.snapshot.state)) return undefined;
    this.prepared = undefined;
    this.set({ state: "validating", errors: [], message: "", result: undefined, code: undefined, label: descriptor.presentation.title });
    if (descriptor.missingFeatures.length || descriptor.availability?.available === false) {
      this.set({ state: "rejected", message: "This interaction is unavailable or needs a newer renderer." });
      return undefined;
    }
    if (descriptor.operation.precondition && (!record?.etag || !record.etag.startsWith('"' + descriptor.operation.precondition.etagOf + ":"))) {
      this.set({ state: "conflicted", message: "Load the current record before submitting.", code: "precondition_required" });
      return undefined;
    }
    const checked = validateInstance(descriptor.input.schema, input);
    if (!checked.ok) {
      this.set({ state: "rejected", errors: checked.errors, code: checked.code, message: "Check the highlighted fields." });
      return undefined;
    }
    const handle = Object.freeze({ id: crypto.randomUUID() });
    this.attempts.set(handle, {
      transport: this.transport,
      descriptor: freeze(clone(descriptor)), input: freeze(clone(checked.value)), record: record ? freeze(clone(record)) : undefined,
      generation: this.runtime.epoch(), idempotencyKey: descriptor.operation.idempotencyKey ? crypto.randomUUID() : undefined,
    });
    this.prepared = handle;
    this.set({ state: "ready" });
    return handle;
  }
  execute(handle: PreparedAttempt): Promise<void> {
    const attempt = this.attempts.get(handle);
    if (!attempt || !this.valid || attempt.generation !== this.runtime.epoch()) return Promise.reject(new Error("Invalid or expired attempt"));
    if (attempt.dispatched) return attempt.dispatched;
    if (handle !== this.prepared || unresolved(this.snapshot.state)) return Promise.reject(new Error("Attempt superseded"));
    // Consume synchronously, before transport (and before its first await).
    this.prepared = undefined;
    const dispatchedAt = Date.now();
    this.set({ state: "submitting", dispatchedAt, attemptId: handle.id, errors: [], message: "Saving…" });
    attempt.dispatched = Promise.resolve().then(async () => {
      if (!this.valid || attempt.generation !== this.runtime.epoch()) return;
      let reply: Reply;
      try { reply = await attempt.transport.submit(attempt.descriptor, attempt.input, { ...attempt.record, ...(attempt.idempotencyKey ? { idempotencyKey: attempt.idempotencyKey } : {}) }); }
      catch { reply = { kind: "network-error", message: "Transport failed" }; }
      if (!this.valid || attempt.generation !== this.runtime.epoch()) return;
      if (reply.kind === "network-error") { this.set({ state: "outcome_unknown", message: UNKNOWN }); return; }
      const op = attempt.descriptor.operation;
      if (reply.status === op.successStatus) {
        const valid = op.successBody === "none" ? reply.body === null : validateInstance(attempt.descriptor.output.schema, reply.body).ok;
        if (!valid) { this.set({ state: "outcome_unknown", message: UNKNOWN }); return; }
        this.set({ state: reply.status === 202 ? "queued" : "succeeded", message: reply.status === 202 ? "Work queued." : "Saved.", result: reply.body, etag: reply.etag, location: reply.location });
        return;
      }
      const problem = asProblem(reply.body);
      // Only a well-formed framework problem on a declared no-effect status
      // establishes refusal. Proxy errors and every 5xx remain uncertain.
      const refused = (!reply.contentType || reply.contentType === "application/problem+json") && op.errors.includes(reply.status) && reply.status >= 400 && reply.status < 500
        && problem.status === reply.status && typeof problem.type === "string"
        && problem.type.startsWith("urn:trestle:problem:") && typeof problem.code === "string";
      if (!refused || problem.code === "outcome_unknown") {
        this.set({ state: "outcome_unknown", message: UNKNOWN, requestId: problem.request_id }); return;
      }
      const conflict = ["stale_version", "precondition_failed", "precondition_required", "interaction_stale", "operation_version_stale"].includes(problem.code!);
      this.set({ state: conflict ? "conflicted" : "rejected", errors: problem.errors ?? [], code: problem.code,
        message: conflict ? "Someone changed this since you opened it. Your draft is preserved; review current data before submitting again." : problem.code === "permission_denied" ? "You don't have permission to do this." : "The request was refused. Check your inputs or access and try again.", requestId: problem.request_id });
    });
    return attempt.dispatched;
  }
  dismissResult() { if (!unresolved(this.snapshot.state)) this.set({ result: undefined }); }
  /** A WorkStatus read may settle the work reference returned by this 202.
   * The caller must validate the authorized status read before observing it. */
  observeWork(result: Record<string, unknown>) {
    const receipt = this.snapshot.result;
    if (!this.valid || this.snapshot.state !== "queued" || !receipt || typeof receipt !== "object" || !("id" in receipt) || typeof receipt.id !== "string" || result.id !== receipt.id) return;
    if (result.state === "succeeded") this.set({ state: "succeeded", message: "Background work completed." });
    else if (result.state === "failed" || result.state === "cancelled") this.set({ state: "rejected", code: "work_failed", message: "The request was accepted, but its background work did not complete." });
  }
  /** Application-specific read-only reconciliation. A refusal never resolves
   * the original attempt. `interpret` must use authoritative attempt evidence,
   * not merely the absence of a changed record field. */
  async reconcile(source: Descriptor, input: Record<string, unknown>, interpret: (result: unknown) => "succeeded" | "no_effect" | "unknown") {
    if (this.snapshot.state !== "outcome_unknown" || source.operation.method !== "GET" || source.missingFeatures.length || !validateInstance(source.input.schema, input).ok) return;
    const generation = this.runtime.epoch(), attempt = this.snapshot.attemptId;
    const query = Object.fromEntries(Object.entries(input).filter(([k]) => !source.operation.pathParams.includes(k)).map(([k, v]) => [k, String(v)]));
    const reply = await this.transport.list(fillPath(source.operation.path, input), query);
    if (!this.valid || generation !== this.runtime.epoch() || attempt !== this.snapshot.attemptId || reply.kind !== "response" || reply.status !== source.operation.successStatus || !validateInstance(source.output.schema, reply.body).ok) return;
    const outcome = interpret(reply.body);
    if (outcome === "succeeded") this.set({ state: "succeeded", message: "The application confirmed this attempt succeeded." });
    else if (outcome === "no_effect") this.set({ state: "rejected", message: "The application confirmed this attempt had no effect." });
  }
  /** No generic execute-capable retry/recovery API is exposed. */
  dispose() { this.listeners.clear(); }
}
