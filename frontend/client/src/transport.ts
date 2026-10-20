// Client transport: the only place that knows URLs and credentials.
// Descriptors never carry tokens (§12.2); the application supplies headers.
// Requests are same-origin paths. Bindings were checked to stay under the
// API prefix when the descriptor was decoded, and every destination is
// checked again here after parameter substitution, before anything is sent:
// a value can introduce a dot segment that the descriptor did not have.

import { hasDotSegment, underPrefix, type Descriptor } from "./protocol";

export interface Problem {
  type?: string;
  title?: string;
  status?: number;
  code?: string;
  detail?: string;
  request_id?: string;
  errors?: { pointer: string; code: string; message: string; params?: Record<string, unknown> }[];
}

export type Reply =
  | { kind: "response"; status: number; body: unknown; bytes: number; etag?: string; location?: string; contentType?: string }
  /** No usable response: the outcome of a write is unknown. */
  | { kind: "network-error"; message: string }
  /** Never sent: the destination left its permitted prefix. Nothing happened. */
  | { kind: "refused"; message: string };

/** Values bound from a record rather than typed by a person. */
export interface RecordContext {
  /** Path parameters, e.g. `{ id }`. */
  path?: Record<string, string>;
  /** The record's `ETag`, sent as `If-Match`. */
  etag?: string;
  idempotencyKey?: string;
}

export interface Transport {
  invalidate?(): void;
  getManifest(): Promise<Reply>;
  getInteraction(id: string): Promise<Reply>;
  /** GET a list binding (e.g. relation options) with query parameters. */
  list(path: string, query?: Record<string, string>): Promise<Reply>;
  /**
   * Submits `input` through the descriptor's binding: path fields go into
   * the URL, the precondition field becomes `If-Match`, the rest is the body.
   */
  submit(descriptor: Descriptor, input: Record<string, unknown>, record?: RecordContext): Promise<Reply>;
  /** Raw request for generated SDK clients. */
  send(request: {
    method: string;
    path: string;
    query?: Record<string, string>;
    headers?: Record<string, string>;
    body?: unknown;
  }): Promise<{ status: number; body: unknown; etag?: string }>;
}

export interface TransportOptions {
  /** Extra headers for every request, e.g. credentials and tenant selection. */
  headers?: () => Record<string, string>;
  /** `/_meta/v1` for the product profile, `/_meta/v1/admin` for admin. */
  metaPrefix?: string;
  /**
   * Operation bindings (`submit`) must resolve under this prefix; it should
   * match the `apiPrefix` descriptors are decoded with. Default `/api/`.
   */
  apiPrefix?: string;
  /**
   * `list` and `send` also reach application metadata (view manifests and
   * descriptors) under this prefix. Default `/_meta/`.
   */
  metaRoot?: string;
  /** The origin requests must stay on. Default: the page's origin. */
  origin?: string;
  fetch?: typeof fetch;
  onAccessRefused?: (status: number) => void;
}

async function read(response: Response): Promise<Reply> {
  const text = await response.text();
  let body: unknown = null;
  if (text.length > 0) {
    try {
      body = JSON.parse(text);
    } catch {
      body = null;
    }
  }
  return {
    kind: "response",
    status: response.status,
    body,
    bytes: new TextEncoder().encode(text).length,
    etag: response.headers.get("ETag") ?? undefined,
    location: response.headers.get("Location") ?? undefined,
    contentType: response.headers.get("Content-Type")?.split(";")[0],
  };
}

/** A path that would leave its permitted destination. Nothing was sent. */
export class PathRefused extends Error {
  constructor(message: string) {
    super(message);
    this.name = "PathRefused";
  }
}

/**
 * Substitutes `{name}` parameters, each percent-encoded as one segment.
 * `encodeURIComponent` leaves `.` and `..` intact, and URL resolution would
 * treat them as traversal, so a parameter that is (or decodes to) a dot
 * segment is refused rather than sent.
 */
export function fillPath(template: string, params: Record<string, unknown>): string {
  return template.replace(/\{([a-z_]+)\}/g, (_, name: string) => {
    const encoded = encodeURIComponent(String(params[name] ?? ""));
    if (hasDotSegment(encoded)) throw new PathRefused(`path parameter ${name} is a dot segment`);
    return encoded;
  });
}

function pageOrigin(): string {
  const location = (globalThis as { location?: { origin?: string } }).location;
  return location?.origin && location.origin !== "null" ? location.origin : "http://localhost";
}

/**
 * Resolves `path` against `origin` and returns the same-origin path to fetch,
 * or throws `PathRefused` unless it stays on `origin`, under one of
 * `prefixes` on a segment boundary, with no dot segment that resolution
 * could remove.
 */
export function confinePath(path: string, prefixes: readonly string[], origin: string = pageOrigin()): string {
  const refuse = (): never => { throw new PathRefused(`request path ${JSON.stringify(path)} leaves its permitted prefix`); };
  if (!path.startsWith("/") || path.startsWith("//") || path.includes("\\") || hasDotSegment(path)) refuse();
  let url: URL;
  try { url = new URL(path, origin); } catch { return refuse(); }
  if (url.origin !== new URL(origin).origin || url.hash !== "" || hasDotSegment(url.pathname)) refuse();
  if (!prefixes.some((prefix) => underPrefix(url.pathname, prefix))) refuse();
  return url.pathname + url.search;
}

function withQuery(path: string, query?: Record<string, string>): string {
  const entries = Object.entries(query ?? {}).filter(([, v]) => v !== "");
  return entries.length === 0 ? path : `${path}?${new URLSearchParams(entries).toString()}`;
}

export function createTransport(options: TransportOptions = {}): Transport {
  const doFetch = options.fetch ?? ((input, init) => fetch(input, init));
  const meta = options.metaPrefix ?? "/_meta/v1";
  const api = options.apiPrefix ?? "/api/";
  const headers = () => ({ Accept: "application/json", ...(options.headers?.() ?? {}) });
  // Trusted metadata (manifest, descriptors) and operation bindings are
  // confined separately: `submit` reaches only the API, and `list`/`send`
  // only the API or the metadata root.
  const scopes = { meta: [meta], api: [api], read: [api, options.metaRoot ?? "/_meta/"] };

  let epoch = 0;
  const reads = new Set<AbortController>();
  async function request(target: string | (() => string), scope: readonly string[], init: RequestInit): Promise<Reply> {
    let path: string;
    try {
      path = confinePath(typeof target === "string" ? target : target(), scope, options.origin);
    } catch (error) {
      if (error instanceof PathRefused) return { kind: "refused", message: error.message };
      throw error;
    }
    const generation = epoch;
    const abort = new AbortController();
    const readOnly = !init.method || init.method === "GET";
    if (readOnly) reads.add(abort);
    try {
      // The API never redirects; following one could leave the checked path.
      const reply = await read(await doFetch(path, { credentials: "same-origin", cache: "no-store", redirect: "error", ...init, signal: abort.signal }));
      if (generation !== epoch) return { kind: "network-error", message: "Access context changed" };
      if (reply.kind === "response" && [401, 403].includes(reply.status)) options.onAccessRefused?.(reply.status);
      return reply;
    } catch (error) {
      return { kind: "network-error", message: error instanceof Error ? error.message : String(error) };
    } finally { reads.delete(abort); }
  }

  return {
    invalidate: () => { epoch++; reads.forEach((read) => read.abort()); reads.clear(); },
    getManifest: () => request(`${meta}/manifest`, scopes.meta, { headers: headers() }),
    getInteraction: (id) => request(() => fillPath(`${meta}/interactions/{id}`, { id }), scopes.meta, { headers: headers() }),
    list: (path, query) => request(withQuery(path, query), scopes.read, { headers: headers() }),
    submit: (descriptor, input, record) => {
      const op = descriptor.operation;
      const body: Record<string, unknown> = { ...input };
      for (const name of op.pathParams) delete body[name];
      const extra: Record<string, string> = op.revisionHeader ? { [op.revisionHeader]: descriptor.revision } : {};
      if (op.operationVersionHeader) extra["Trestle-Operation-Version"] = op.version;
      if (op.idempotencyKey && record?.idempotencyKey) extra["Idempotency-Key"] = record.idempotencyKey;
      if (op.precondition) {
        delete body[op.precondition.field];
        if (record?.etag) extra["If-Match"] = record.etag;
      }
      const target = () => {
        const path = fillPath(op.path, { ...input, ...(record?.path ?? {}) });
        return op.body === "none" ? withQuery(path, body as Record<string, string>) : path;
      };
      return request(target, scopes.api, {
        method: op.method,
        headers: { ...headers(), ...extra, ...(op.body === "input" ? { "Content-Type": "application/json" } : {}) },
        ...(op.body === "input" ? { body: JSON.stringify(body) } : {}),
      });
    },
    send: async ({ method, path, query, headers: extra, body }) => {
      const reply = await request(withQuery(path, query), scopes.read, {
        method,
        headers: { ...headers(), ...(extra ?? {}), ...(body !== undefined ? { "Content-Type": "application/json" } : {}) },
        ...(body !== undefined && method !== "GET" && method !== "DELETE" ? { body: JSON.stringify(body) } : {}),
      });
      if (reply.kind === "network-error") return { status: 0, body: { code: "network_error", detail: reply.message } };
      if (reply.kind === "refused") return { status: 0, body: { code: "path_refused", detail: reply.message } };
      return { status: reply.status, body: reply.body, etag: reply.etag };
    },
  };
}

export function asProblem(body: unknown): Problem {
  return typeof body === "object" && body !== null ? (body as Problem) : {};
}

/** The version in an ETag such as `"lessons.view:3"`. */
export function etagVersion(etag: string | undefined): string | undefined {
  return etag?.match(/:(\d+)"$/)?.[1];
}
