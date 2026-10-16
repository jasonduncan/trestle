// Client transport: the only place that knows URLs and credentials.
// Descriptors never carry tokens (§12.2); the application supplies headers.
// Requests are same-origin paths; bindings were already checked to stay
// under the API prefix when the descriptor was decoded.

import type { Descriptor } from "./protocol";

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
  | { kind: "response"; status: number; body: unknown; bytes: number; etag?: string }
  /** No usable response: the outcome of a write is unknown. */
  | { kind: "network-error"; message: string };

/** Values bound from a record rather than typed by a person. */
export interface RecordContext {
  /** Path parameters, e.g. `{ id }`. */
  path?: Record<string, string>;
  /** The record's `ETag`, sent as `If-Match`. */
  etag?: string;
}

export interface Transport {
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
  fetch?: typeof fetch;
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
  };
}

export function fillPath(template: string, params: Record<string, unknown>): string {
  return template.replace(/\{([a-z_]+)\}/g, (_, name: string) => encodeURIComponent(String(params[name] ?? "")));
}

function withQuery(path: string, query?: Record<string, string>): string {
  const entries = Object.entries(query ?? {}).filter(([, v]) => v !== "");
  return entries.length === 0 ? path : `${path}?${new URLSearchParams(entries).toString()}`;
}

export function createTransport(options: TransportOptions = {}): Transport {
  const doFetch = options.fetch ?? ((input, init) => fetch(input, init));
  const meta = options.metaPrefix ?? "/_meta/v1";
  const headers = () => ({ Accept: "application/json", ...(options.headers?.() ?? {}) });

  async function request(path: string, init: RequestInit): Promise<Reply> {
    try {
      return await read(await doFetch(path, { credentials: "same-origin", cache: "no-store", ...init }));
    } catch (error) {
      return { kind: "network-error", message: error instanceof Error ? error.message : String(error) };
    }
  }

  return {
    getManifest: () => request(`${meta}/manifest`, { headers: headers() }),
    getInteraction: (id) => request(`${meta}/interactions/${encodeURIComponent(id)}`, { headers: headers() }),
    list: (path, query) => request(withQuery(path, query), { headers: headers() }),
    submit: (descriptor, input, record) => {
      const op = descriptor.operation;
      const body: Record<string, unknown> = { ...input };
      for (const name of op.pathParams) delete body[name];
      const extra: Record<string, string> = { [op.revisionHeader]: descriptor.revision };
      if (op.precondition) {
        delete body[op.precondition.field];
        if (record?.etag) extra["If-Match"] = record.etag;
      }
      const path = fillPath(op.path, { ...input, ...(record?.path ?? {}) });
      return request(op.body === "none" ? withQuery(path, body as Record<string, string>) : path, {
        method: op.method,
        headers: { ...headers(), ...extra, ...(op.body === "input" ? { "Content-Type": "application/json" } : {}) },
        ...(op.body === "input" ? { body: JSON.stringify(body) } : {}),
      });
    },
    send: async ({ method, path, query, headers: extra, body }) => {
      const reply = await request(withQuery(path, query), {
        method,
        headers: { ...headers(), ...(extra ?? {}), ...(body !== undefined ? { "Content-Type": "application/json" } : {}) },
        ...(body !== undefined && method !== "GET" && method !== "DELETE" ? { body: JSON.stringify(body) } : {}),
      });
      if (reply.kind === "network-error") return { status: 0, body: { code: "network_error", detail: reply.message } };
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
