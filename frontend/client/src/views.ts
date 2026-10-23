import { checkBindingPath, choiceLabel, decodeObjectSchema, type Descriptor, type FieldSchema, type ObjectSchema } from "./protocol";

export interface ViewSource { invocation: Descriptor; item: ObjectSchema; identityPointer: string; itemsPointer: string | null; nextCursorPointer: string | null; completePointer: string | null }
export interface ViewAction {
  id: string; label: string; invocation: Descriptor; interactionId: string | null; interactionRevision: string | null;
  context: Record<string, string>; visibleWhen: { pointer: string; equals: unknown } | null;
}
export interface View {
  viewId: string; revision: string; audience: string; kind: "collection" | "record"; title: string; resource: string;
  labelPointer: string; source: ViewSource; columns: { id: string; pointer: string; label: string }[];
  filters: string[]; recordView: string | null; recordInput: Record<string, string>; actions: ViewAction[];
  missingFeatures: string[];
  sections: ViewSection[];
  availability?: { invocation: Descriptor; input: Record<string, string> };
}
export interface ViewSection { id: string; title: string; source: ViewSource; input: Record<string, string>; columns: View["columns"]; component: string | null; requiredComponent: boolean }
export interface ViewEntry { viewId: string; revision: string; audience: string; kind: string; title: string; url: string }
type Json = Record<string, unknown>;
export const VIEW_FEATURES = new Set(["views.collection@1", "views.record@1", "views.sections@1", "views.availability@1", "views.completeness@1"]);
export function object(value: unknown, label = "value"): Json {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error(label + " must be an object");
  return value as Json;
}
function keys(value: Json, allowed: string[]) {
  for (const key of Object.keys(value)) if (!allowed.includes(key)) throw new Error("Unknown field: " + key);
}
export function text(value: unknown): string { if (typeof value !== "string") throw new Error("Expected text"); return value; }
function nullable(value: unknown): string | null { return value === null ? null : text(value); }
function strings(value: unknown): string[] { if (!Array.isArray(value)) throw new Error("Expected array"); return value.map(text); }
function id(value: unknown): string {
  const result = text(value);
  if (!/^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$/.test(result)) throw new Error("Invalid stable ID");
  return result;
}
function pointer(value: unknown): string {
  const result = text(value);
  if (!/^\/[a-z][a-z0-9_]*$/.test(result) || ["/__proto__", "/constructor", "/prototype"].includes(result)) throw new Error("Unsupported pointer");
  return result;
}
export function at(value: unknown, path: string): unknown {
  for (const segment of path.slice(1).split("/")) {
    const key = segment.replace(/~1/g, "/").replace(/~0/g, "~");
    if (!value || typeof value !== "object" || !Object.hasOwn(value, key)) return undefined;
    value = (value as Json)[key];
  }
  return value;
}
function limits(value: unknown, depth = 0): void {
  if (depth > 32) throw new Error("Metadata nesting exceeds limit");
  if (value && typeof value === "object") for (const [key, child] of Object.entries(value)) {
    if (["__proto__", "prototype", "constructor", "$ref", "$dynamicRef", "$id", "$anchor"].includes(key)) throw new Error("Forbidden metadata key");
    limits(child, depth + 1);
  }
}
function envelope(value: unknown, bytes?: number): Json {
  if ((bytes ?? new TextEncoder().encode(JSON.stringify(value)).length) > 256 * 1024) throw new Error("Metadata exceeds byte limit");
  limits(value);
  const raw = object(value);
  if (raw.protocol !== "trestle.views" || raw.protocolVersion !== "1.0") throw new Error("Unsupported view protocol");
  return raw;
}
function mapped(value: unknown): Record<string, string> {
  return Object.fromEntries(Object.entries(object(value)).map(([name, ptr]) => {
    if (!/^[a-z][a-z0-9_]*$/.test(name)) throw new Error("Invalid input mapping");
    return [name, pointer(ptr)];
  }));
}
function checkMapping(item: ObjectSchema, input: ObjectSchema, name: string, ptr: string) {
  const from = item.fields.find((f) => f.name === ptr.slice(1)), to = input.fields.find((f) => f.name === name);
  if (!from || !to || from.kind !== to.kind || (from.nullable && !to.nullable) || (to.format && from.format !== to.format)) throw new Error("Incompatible input mapping");
}
/** URL values use the declared query scalar type; JSON bodies never coerce. */
export function routeInput(schema: ObjectSchema, raw: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(raw).map(([key, value]) => {
    const field = schema.fields.find((f) => f.name === key);
    if (field?.kind === "integer" && typeof value === "string" && /^-?(0|[1-9][0-9]*)$/.test(value) && Number.isSafeInteger(Number(value))) return [key, Number(value)];
    if (field?.kind === "boolean" && (value === "true" || value === "false")) return [key, value === "true"];
    return [key, value];
  }));
}
function contract(value: unknown) {
  const raw = object(value); keys(raw, ["id", "version", "schema"]);
  return { id: id(raw.id), version: text(raw.version), schema: decodeObjectSchema(raw.schema, "view contract") };
}

export function decodeInvocation(value: unknown, apiPrefix = "/api/"): Descriptor {
  const raw = object(value);
  keys(raw, ["operationId", "operationVersion", "bindingRevision", "method", "path", "body", "pathParams", "input", "output", "successStatus", "noContent", "idempotencyKey", "operationVersionHeader", "errors", "precondition", "location"]);
  const input = contract(raw.input), output = contract(raw.output);
  const method = text(raw.method), path = checkBindingPath(text(raw.path), apiPrefix), pathParams = strings(raw.pathParams);
  if (!["GET", "POST", "PUT", "PATCH", "DELETE"].includes(method) || !["none", "input"].includes(text(raw.body))) throw new Error("Invalid invocation binding");
  if ((["GET", "DELETE"].includes(method)) !== (raw.body === "none")) throw new Error("Ambiguous input source");
  const params = [...path.matchAll(/\{([a-z_]+)\}/g)].map((m) => m[1]);
  if (params.join() !== pathParams.join() || pathParams.some((p) => !input.schema.fields.some((f) => f.name === p))) throw new Error("Invalid path parameters");
  if (typeof raw.successStatus !== "number" || !Number.isInteger(raw.successStatus) || raw.successStatus < 200 || raw.successStatus > 299) throw new Error("Invalid success status");
  if (typeof raw.noContent !== "boolean" || typeof raw.idempotencyKey !== "boolean" || typeof raw.operationVersionHeader !== "boolean" || (raw.noContent && raw.successStatus !== 204)) throw new Error("Invalid response semantics");
  if (!Array.isArray(raw.errors) || raw.errors.some((s) => !Number.isInteger(s) || s < 400 || s > 599)) throw new Error("Invalid error statuses");
  if (raw.location !== null) checkBindingPath(text(raw.location), apiPrefix);
  const descriptor: Descriptor = {
    interactionId: id(raw.operationId), revision: text(raw.bindingRevision), audience: "", requiredFeatures: [], missingFeatures: [],
    operation: { id: id(raw.operationId), version: text(raw.operationVersion), method, path, pathParams, body: raw.body as "none" | "input",
      successStatus: raw.successStatus, successBody: raw.noContent ? "none" : "output", idempotencyKey: raw.idempotencyKey, operationVersionHeader: raw.operationVersionHeader === true, errors: raw.errors as number[] },
    input, output, presentation: { title: id(raw.operationId), submitLabel: "Confirm", fields: [] },
  };
  if (raw.precondition !== null) {
    const p = object(raw.precondition); keys(p, ["field", "etagOf", "target"]);
    const field = text(p.field);
    if (!input.schema.fields.some((f) => f.name === field && f.format === "digits")) throw new Error("Invalid precondition field");
    descriptor.operation.precondition = { field, etagOf: id(p.etagOf) };
    if (p.target) {
      const target = object(p.target); keys(target, ["operation", "path", "parameter", "inputField"]);
      const path = checkBindingPath(text(target.path), apiPrefix), parameter = text(target.parameter), inputField = text(target.inputField);
      if (path.match(/\{[a-z_]+\}/g)?.join() !== "{" + parameter + "}" || !input.schema.fields.some((f) => f.name === inputField)) throw new Error("Invalid target mapping");
      descriptor.operation.precondition.target = { operation: id(target.operation), path, parameter, inputField };
    }
  }
  return descriptor;
}

export function decodeView(value: unknown, options: { bytes?: number; apiPrefix?: string } = {}): View {
  const raw = envelope(value, options.bytes);
  keys(raw, ["protocol", "protocolVersion", "viewId", "revision", "audience", "kind", "requiredFeatures", "title", "resource", "labelPointer", "source", "columns", "filters", "recordView", "recordInput", "actions", "sections", "availability"]);
  if (raw.kind !== "collection" && raw.kind !== "record") throw new Error("Unsupported view kind");
  if (raw.audience !== "product" && raw.audience !== "admin") throw new Error("Unsupported audience");
  const source = object(raw.source); keys(source, ["invocation", "item", "itemsPointer", "identityPointer", "nextCursorPointer", "completePointer"]);
  const invocation = decodeInvocation(source.invocation, options.apiPrefix), item = contract(source.item).schema;
  if (invocation.operation.method !== "GET") throw new Error("View source is not read-only");
  const outputPointer = (value: unknown) => {
    const p = pointer(value);
    if (!item.fields.some((f) => f.name === p.slice(1))) throw new Error("Unknown output pointer " + p);
    return p;
  };
  const identityPointer = outputPointer(source.identityPointer);
  const expected = raw.kind === "collection" ? invocation.output.schema.fields.find((f) => f.name === "items")?.items : invocation.output.schema;
  if (JSON.stringify(item) !== JSON.stringify(expected)) throw new Error("Item contract mismatches published output");
  if (!["string", "integer"].includes(item.fields.find((f) => f.name === identityPointer.slice(1))!.kind)) throw new Error("Identity must be scalar");
  if (!Array.isArray(raw.columns) || raw.columns.length > 32 || !Array.isArray(raw.actions) || raw.actions.length + raw.columns.length > 64) throw new Error("View presentation exceeds limit");
  const columns = raw.columns.map((c) => { const col = object(c); keys(col, ["id", "pointer", "label"]); return { id: text(col.id), pointer: outputPointer(col.pointer), label: text(col.label) }; });
  if (new Set(columns.map((c) => c.id)).size !== columns.length) throw new Error("Duplicate column");
  const filters = strings(raw.filters);
  if (filters.some((f) => !invocation.input.schema.fields.some((field) => field.name === f && ["string", "integer", "boolean"].includes(field.kind)))) throw new Error("Invalid filter input");
  const actions = raw.actions.map((a): ViewAction => {
    const action = object(a); keys(action, ["id", "label", "invocation", "interactionId", "interactionRevision", "context", "visibleWhen"]);
    const invocation = decodeInvocation(action.invocation, options.apiPrefix), context = mapped(action.context);
    for (const [name, ptr] of Object.entries(context)) {
      if (!invocation.input.schema.fields.some((f) => f.name === name)) throw new Error("Invalid action mapping");
      outputPointer(ptr);
      checkMapping(item, invocation.input.schema, name, ptr);
    }
    const interactionId = nullable(action.interactionId), interactionRevision = nullable(action.interactionRevision);
    if ((interactionId === null) !== (interactionRevision === null)) throw new Error("Incomplete interaction reference");
    if (!interactionId && invocation.input.schema.fields.some((f) => f.required && !context[f.name] && invocation.operation.precondition?.field !== f.name)) throw new Error("Action requires a form");
    invocation.presentation.title = text(action.label);
    let visibleWhen = null;
    if (action.visibleWhen !== null) { const c = object(action.visibleWhen); keys(c, ["pointer", "equals"]); visibleWhen = { pointer: outputPointer(c.pointer), equals: c.equals }; }
    return { id: text(action.id), label: text(action.label), invocation, context, interactionId, interactionRevision, visibleWhen };
  });
  if (new Set(actions.map((a) => a.id)).size !== actions.length) throw new Error("Duplicate action");
  const itemsPointer = nullable(source.itemsPointer), nextCursorPointer = nullable(source.nextCursorPointer);
  if (raw.kind === "collection" && (itemsPointer !== "/items" || nextCursorPointer !== "/next_cursor")) throw new Error("Unsupported page shape");
  if (raw.kind === "record" && (itemsPointer !== null || nextCursorPointer !== null)) throw new Error("Record must not be a page");
  // ADR-045: a required boolean of the page, false when rows were left out.
  const completePointer = source.completePointer === undefined ? null : pointer(source.completePointer);
  if (completePointer !== null && (raw.kind !== "collection"
    || !invocation.output.schema.fields.some((f) => f.name === completePointer.slice(1) && f.kind === "boolean" && f.required && !f.nullable))) throw new Error("Invalid completeness flag");
  const recordInput = mapped(raw.recordInput); Object.values(recordInput).forEach(outputPointer);
  if (!Array.isArray(raw.sections) || raw.sections.length > 16 || (raw.kind === "collection" && raw.sections.length)) throw new Error("Invalid record sections");
  const sections = raw.sections.map((value): ViewSection => {
    const section = object(value); keys(section, ["id", "title", "source", "input", "columns", "component", "requiredComponent"]);
    const source = object(section.source), sectionId = text(section.id);
    const nested = decodeView({ ...raw, viewId: id(raw.viewId) + "." + sectionId, kind: source.itemsPointer === null ? "record" : "collection",
      source, labelPointer: source.identityPointer, columns: section.columns, filters: [], actions: [], recordView: null, recordInput: {}, sections: [], availability: undefined }, options);
    const input = mapped(section.input); Object.values(input).forEach(outputPointer);
    Object.entries(input).forEach(([name, ptr]) => checkMapping(item, nested.source.invocation.input.schema, name, ptr));
    if (nested.source.invocation.input.schema.fields.some((f) => f.required && !input[f.name])) throw new Error("Missing section input");
    if (typeof section.requiredComponent !== "boolean") throw new Error("Invalid component requirement");
    return { id: sectionId, title: text(section.title), source: nested.source, input, columns: nested.columns, component: nullable(section.component), requiredComponent: section.requiredComponent };
  });
  if (new Set(sections.map((s) => s.id)).size !== sections.length) throw new Error("Duplicate section");
  if (actions.length + columns.length + sections.reduce((n, s) => n + 1 + s.columns.length, 0) > 64) throw new Error("View presentation exceeds limit");
  let availability: View["availability"];
  if (raw.availability !== undefined) {
    const hints = object(raw.availability); keys(hints, ["invocation", "input"]);
    const read = decodeInvocation(hints.invocation, options.apiPrefix), input = mapped(hints.input);
    if (raw.kind !== "record" || read.operation.method !== "GET" || !strings(raw.requiredFeatures).includes("views.availability@1")) throw new Error("Invalid availability read");
    Object.values(input).forEach(outputPointer);
    Object.entries(input).forEach(([name, ptr]) => checkMapping(item, read.input.schema, name, ptr));
    if (read.input.schema.fields.some((f) => f.required && !input[f.name])) throw new Error("Missing availability input");
    availability = { invocation: read, input };
  }
  return { viewId: id(raw.viewId), revision: text(raw.revision), audience: raw.audience, kind: raw.kind, title: text(raw.title), resource: id(raw.resource),
    source: { invocation, item, identityPointer, itemsPointer, nextCursorPointer, completePointer }, columns, filters, actions, labelPointer: outputPointer(raw.labelPointer),
    recordView: nullable(raw.recordView), recordInput, sections, availability, missingFeatures: strings(raw.requiredFeatures).filter((f) => !VIEW_FEATURES.has(f)) };
}

export function currentAvailability(value: unknown, view: View, record: unknown, etag: string | undefined): Record<string, { available: boolean; reason: string | null }> | undefined {
  if (!view.availability || !etag) return undefined;
  const hints = object(value);
  if (hints.resource !== view.resource || hints.record_key !== String(at(record, view.source.identityPointer)) || hints.etag !== etag || !Array.isArray(hints.actions) || hints.actions.length > 64) return undefined;
  const result: Record<string, { available: boolean; reason: string | null }> = Object.create(null);
  for (const value of hints.actions) {
    const action = object(value);
    if (typeof action.id !== "string" || !view.actions.some((a) => a.id === action.id) || Object.hasOwn(result, action.id) || typeof action.available !== "boolean" || (action.reason !== null && typeof action.reason !== "string")) return undefined;
    result[action.id] = { available: action.available, reason: action.reason as string | null };
  }
  return result;
}

export function decodeViewManifest(value: unknown, options: { bytes?: number; metaPrefix?: string } = {}): ViewEntry[] {
  const raw = envelope(value, options.bytes); keys(raw, ["protocol", "protocolVersion", "views"]);
  if (!Array.isArray(raw.views) || raw.views.length > 512) throw new Error("Invalid view manifest");
  const entries = raw.views.map((v) => {
    const entry = object(v); keys(entry, ["viewId", "revision", "audience", "kind", "title", "url"]);
    if (!["admin", "product"].includes(text(entry.audience)) || !["collection", "record"].includes(text(entry.kind))) throw new Error("Invalid manifest profile");
    return { viewId: id(entry.viewId), revision: text(entry.revision), audience: text(entry.audience), kind: text(entry.kind), title: text(entry.title), url: checkBindingPath(text(entry.url), options.metaPrefix ?? "/_meta/v1/ui/") };
  });
  if (new Set(entries.map((e) => e.viewId)).size !== entries.length) throw new Error("Duplicate manifest view");
  return entries;
}

/** The top-level field a `/name` pointer names in `schema`, if any. */
export function fieldAt(schema: ObjectSchema, pointer: string): FieldSchema | undefined {
  return schema.fields.find((f) => f.name === pointer.slice(1));
}

/** Display text for a value; with its field, an enum value shows its label (ADR-035). */
export function formatValue(value: unknown, field?: FieldSchema): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "string") return choiceLabel(field, value);
  if (typeof value !== "object") return String(value);
  const money = value as Json;
  if (typeof money.amount === "string" && typeof money.currency === "string") return money.amount + " " + money.currency;
  return JSON.stringify(value);
}
