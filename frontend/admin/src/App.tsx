// Trestle admin shell (design doc §10). Renders whatever the admin
// publication profile describes: resource lists with their columns and
// filters, domain-action buttons and edit forms. It holds no authority:
// every button calls the same operation a product screen would, and the
// server decides.

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  OperationForm,
  SignIn,
  asProblem,
  createTransport,
  fillPath,
  loadMethods,
  sessionHeaders,
  useSession,
  type SignInMethods,
  type Transport,
} from "@trestle/interaction-react";

// Deployment profile (Vite env, so the shell itself stays app-agnostic):
// - VITE_ADMIN_TITLE: heading and document title.
// - VITE_ADMIN_DEV_USERS: JSON {name: {token, note}} for development sign-in
//   without browser login (held in memory only). Empty: no dev sign-in.
// - VITE_ADMIN_TENANT: JSON {header, label, path, dev: [{id, name}]} when the
//   project selects a tenant per request; `path` lists the caller's
//   memberships ({items:[{<idField>, name, role}]}, owners only). Empty: none.
// The pilot example's values live in `.env`; other projects override them.
const TITLE = import.meta.env.VITE_ADMIN_TITLE || "Trestle Admin";
const USERS: Record<string, { token: string; note: string }> = parseJson(import.meta.env.VITE_ADMIN_DEV_USERS) ?? {};
interface TenantProfile { header: string; label: string; path: string; idField?: string; dev?: { id: string; name: string }[] }
const TENANT: TenantProfile | null = parseJson(import.meta.env.VITE_ADMIN_TENANT);
type User = string;

function parseJson<T>(raw: string | undefined): T | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw) as T;
  } catch {
    console.error("admin: invalid JSON in profile setting");
    return null;
  }
}

interface Binding {
  operation: string;
  method: string;
  path: string;
  form?: string | null;
}

interface AdminResource {
  id: string;
  title: string;
  label: string;
  path: string;
  columns: string[];
  filters: { name: string; schema: { type: string | string[]; enum?: string[]; minimum?: number; maximum?: number } }[];
  list: { operation: string; path: string };
  retrieve: Binding | null;
  update: Binding | null;
  create?: Binding | null;
  actions: {
    name: string;
    label: string;
    operation: string;
    path: string;
    bind: Record<string, string>;
    /** Advisory: show only when the row matches. The server still decides. */
    visibleWhen?: { pointer: string; equals: unknown };
  }[];
}

type Row = Record<string, unknown> & { id: string };

const MESSAGES: Record<string, string> = {
  stale_version: "Someone changed this record. The list has been refreshed; try again.",
  invalid_transition: "That action is not possible in the record's current state.",
  permission_denied: "You don't have permission to do this.",
  admin_forbidden: "You don't have access to the admin.",
  tenant_not_permitted: "You're not a member of the selected tenant.",
};

function describe(body: unknown, status: number): string {
  const problem = asProblem(body);
  return (problem.code && MESSAGES[problem.code]) ?? problem.detail ?? problem.title ?? `Request failed (${status}).`;
}

/** Value at a JSON Pointer such as `/version`. */
function at(row: Row, pointer: string): unknown {
  return pointer
    .split("/")
    .slice(1)
    .reduce<unknown>((value, key) => (value && typeof value === "object" ? (value as Record<string, unknown>)[key] : undefined), row);
}

function ResourcePanel({ resource, transport }: { resource: AdminResource; transport: Transport }) {
  const [rows, setRows] = useState<Row[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [filters, setFilters] = useState<Record<string, string>>({});
  const [message, setMessage] = useState("");
  const [editing, setEditing] = useState<{ row: Row; etag?: string } | null>(null);
  const [creating, setCreating] = useState(false);
  // Convention: an output with `reveal_once` (+ `reveal_once_label`) carries a secret
  // the server will never show again (an API key, an invite link).
  const [revealed, setRevealed] = useState<{ label: string; value: string } | null>(null);
  const reveal = (output: unknown) => {
    const out = output as { reveal_once?: unknown; reveal_once_label?: unknown } | null;
    if (out && typeof out.reveal_once === "string" && out.reveal_once)
      setRevealed({ label: typeof out.reveal_once_label === "string" ? out.reveal_once_label : "Secret", value: out.reveal_once });
  };

  const load = useCallback(
    async (after?: string) => {
      const query: Record<string, string> = { limit: "20", ...filters };
      if (after) query.cursor = after;
      const reply = await transport.list(resource.list.path, query);
      if (reply.kind !== "response" || reply.status !== 200) {
        setMessage(reply.kind === "response" ? describe(reply.body, reply.status) : "The list could not be loaded.");
        return;
      }
      const page = reply.body as { items: Row[]; next_cursor: string | null };
      setRows((previous) => (after ? [...previous, ...page.items] : page.items));
      setCursor(page.next_cursor);
    },
    [transport, resource.list.path, filters],
  );

  useEffect(() => {
    void load();
  }, [load]);

  const act = async (row: Row, action: AdminResource["actions"][number]) => {
    const body = Object.fromEntries(Object.entries(action.bind).map(([field, pointer]) => [field, at(row, pointer)]));
    const reply = await transport.send({ method: "POST", path: fillPath(action.path, row), body });
    const name = String(row[resource.label]);
    setMessage(reply.status === 200 ? `${action.label}: “${name}” done.` : describe(reply.body, reply.status));
    if (reply.status === 200) reveal(reply.body);
    await load();
  };

  const edit = async (row: Row) => {
    if (!resource.retrieve) return;
    const reply = await transport.send({ method: "GET", path: fillPath(resource.retrieve.path, row) });
    if (reply.status !== 200) {
      setMessage(describe(reply.body, reply.status));
      return;
    }
    setEditing({ row: reply.body as Row, etag: reply.etag });
  };

  return (
    <section aria-labelledby={`${resource.id}-heading`}>
      <h2 id={`${resource.id}-heading`}>{resource.title}</h2>
      <p className="message" role="status" aria-live="polite">
        {message}
      </p>
      {revealed && (
        <div className="reveal" role="alert">
          <p>
            <strong>{revealed.label}</strong> — copy it now; it will not be shown again.
          </p>
          <input readOnly value={revealed.value} aria-label={revealed.label} onFocus={(e) => e.currentTarget.select()} />
          <button type="button" onClick={() => void navigator.clipboard?.writeText(revealed.value)}>
            Copy
          </button>
          <button type="button" className="secondary" onClick={() => setRevealed(null)}>
            Done
          </button>
        </div>
      )}
      {resource.create?.form && !creating && (
        <button type="button" onClick={() => setCreating(true)}>
          New…
        </button>
      )}
      {creating && resource.create?.form && (
        <div className="form-panel">
          <OperationForm
            interaction={resource.create.form}
            transport={transport}
            onSuccess={(output) => {
              setCreating(false);
              setMessage("Created.");
              reveal(output);
              void load();
            }}
          />
          <button type="button" className="secondary" onClick={() => setCreating(false)}>
            Cancel
          </button>
        </div>
      )}
      <div className="filters">
        {resource.filters.map((filter) => (
          <label key={filter.name}>
            {filter.name}{" "}
            {filter.schema.enum ? (
              <select
                value={filters[filter.name] ?? ""}
                onChange={(e) => setFilters((f) => ({ ...f, [filter.name]: e.target.value }))}
              >
                <option value="">Any</option>
                {filter.schema.enum
                  .filter((v): v is string => v !== null)
                  .map((v) => (
                    <option key={v} value={v}>
                      {v}
                    </option>
                  ))}
              </select>
            ) : (
              <input
                inputMode="numeric"
                size={4}
                value={filters[filter.name] ?? ""}
                onChange={(e) => setFilters((f) => ({ ...f, [filter.name]: e.target.value.trim() }))}
              />
            )}
          </label>
        ))}
      </div>
      <table>
        <thead>
          <tr>
            {resource.columns.map((column) => (
              <th key={column} scope="col">
                {column}
              </th>
            ))}
            <th scope="col">
              <span className="visually-hidden">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.id}>
              {resource.columns.map((column) => (
                <td key={column}>{row[column] === null || row[column] === undefined ? "—" : String(row[column])}</td>
              ))}
              <td className="actions">
                {resource.actions
                  .filter((action) => !action.visibleWhen || at(row, action.visibleWhen.pointer) === action.visibleWhen.equals)
                  .map((action) => (
                  <button
                    key={action.name}
                    type="button"
                    onClick={() => void act(row, action)}
                    aria-label={`${action.label} ${String(row[resource.label])}`}
                  >
                    {action.label}
                  </button>
                ))}
                {resource.update?.form && (
                  <button type="button" onClick={() => void edit(row)} aria-label={`Edit ${String(row[resource.label])}`}>
                    Edit
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {cursor && (
        <button type="button" onClick={() => void load(cursor)}>
          Load more
        </button>
      )}
      {editing && resource.update?.form && (
        <div className="form-panel">
          <OperationForm
            key={`${editing.row.id}:${editing.etag}`}
            interaction={resource.update.form}
            transport={transport}
            record={{ path: { id: editing.row.id }, etag: editing.etag, values: editing.row }}
            onSuccess={() => {
              setMessage(`Saved “${String(editing.row[resource.label])}”.`);
              setEditing(null);
              void load();
            }}
          />
          <button type="button" className="secondary" onClick={() => setEditing(null)}>
            Close
          </button>
        </div>
      )}
    </section>
  );
}

export function App() {
  const session = useSession();
  const refreshSession = session.refresh;
  const signedIn = session.state.kind === "signed-in";
  const [methods, setMethods] = useState<SignInMethods | null>(null);

  useEffect(() => {
    if (session.state.kind === "signed-out") void loadMethods().then(setMethods);
  }, [session.state.kind]);
  const [user, setUser] = useState<User>(Object.keys(USERS)[0] ?? "");
  const [tenants, setTenants] = useState<{ id: string; name: string }[]>(TENANT?.dev ?? []);
  const [tenant, setTenant] = useState(TENANT?.dev?.[0]?.id ?? "");
  const [resources, setResources] = useState<AdminResource[] | string>("Loading…");
  useEffect(() => {
    document.title = TITLE;
  }, []);

  const transport = useMemo(
    () =>
      createTransport({
        metaPrefix: "/_meta/v1/admin",
        headers: () => ({
          ...(signedIn || !USERS[user] ? sessionHeaders(session.state) : { Authorization: `Bearer ${USERS[user]!.token}` }),
          ...(TENANT && tenant ? { [TENANT.header]: tenant } : {}),
        }),
      }),
    [signedIn, session.state, user, tenant],
  );

  // Signed in with tenants: offer the ones the caller owns.
  useEffect(() => {
    if (!signedIn || !TENANT) return;
    void transport.send({ method: "GET", path: TENANT.path }).then((reply) => {
      if (reply.status !== 200) return;
      const idField = TENANT.idField ?? "id";
      const items = (reply.body as { items: Record<string, string>[] }).items;
      const owned = items.filter((m) => m.role === "owner").map((m) => ({ id: m[idField]!, name: m.name! }));
      setTenants(owned);
      setTenant((current) => (owned.some((t) => t.id === current) ? current : (owned[0]?.id ?? "")));
    });
  }, [signedIn, transport]);

  useEffect(() => {
    if (session.state.kind === "loading" || session.state.kind === "signed-out") return;
    let cancelled = false;
    setResources("Loading…");
    void transport.getManifest().then((reply) => {
      if (cancelled) return;
      if (reply.kind === "response" && reply.status === 200) {
        setResources((reply.body as { resources: AdminResource[] }).resources);
      } else {
        setResources(reply.kind === "response" ? describe(reply.body, reply.status) : "The admin could not be loaded.");
      }
    });
    return () => {
      cancelled = true;
    };
  }, [transport, session.state.kind]);

  if (session.state.kind === "loading") return <p className="app">Loading…</p>;
  if (session.state.kind === "signed-out") {
    return (
      <div className="app">
        <header>
          <h1>{TITLE}</h1>
        </header>
        <main>
          {methods ? (
            <SignIn methods={methods} onSignedIn={() => void refreshSession()} returnTo={window.location.pathname} />
          ) : (
            <p>You are signed out.</p>
          )}
        </main>
      </div>
    );
  }

  const tenantPicker = TENANT && (
    <label>
      {TENANT.label}{" "}
      <select value={tenant} onChange={(e) => setTenant(e.target.value)}>
        {tenants.map((t) => (
          <option key={t.id} value={t.id}>
            {t.name}
          </option>
        ))}
      </select>
    </label>
  );

  return (
    <div className="app">
      <header>
        <h1>{TITLE}</h1>
        {signedIn ? (
          <fieldset className="identity">
            <legend>Signed in</legend>
            {tenantPicker}
            <button type="button" onClick={() => void session.signOut()}>
              Sign out
            </button>
          </fieldset>
        ) : (
          <fieldset className="identity">
            <legend>Development sign-in</legend>
            <label>
              User{" "}
              <select value={user} onChange={(e) => setUser(e.target.value)}>
                {Object.entries(USERS).map(([name, { note }]) => (
                  <option key={name} value={name}>
                    {name} ({note})
                  </option>
                ))}
              </select>
            </label>
            {tenantPicker}
          </fieldset>
        )}
      </header>
      <main>
        {typeof resources === "string" ? (
          <p role="alert">{resources}</p>
        ) : (
          resources.map((resource) => <ResourcePanel key={`${signedIn ? "session" : user}:${tenant}:${resource.id}`} resource={resource} transport={transport} />)
        )}
      </main>
    </div>
  );
}
