import { useEffect, useMemo, useState, useRef } from "react";
import { AdminShell } from "./index";
import { operationRuntime, object, OperationRuntime, bindOperationRuntime } from "@trestle-framework/client";
import { createTransport, useSession, loadMethods, sessionHeaders, SignIn, type SignInMethods } from "@trestle-framework/interaction-react";
export interface TenantProfile { header: string; label: string; path: string; idField?: string; dev?: { id: string; name: string }[] }
export interface AdminHostProps { title?: string; tenantProfile?: TenantProfile | null; developmentUsers?: Record<string, { token: string; note?: string }>; }

export function AdminHost({ title: TITLE = "Trestle", tenantProfile: TENANT = null, developmentUsers: USERS = {} }: AdminHostProps) {
  const session = useSession();
  const refreshSession = session.refresh;
  const signedIn = session.state.kind === "signed-in";
  const [accessRefused, setAccessRefused] = useState(false);
  const [methods, setMethods] = useState<SignInMethods | null>(null);

  useEffect(() => {
    if (session.state.kind === "signed-out") void loadMethods().then(setMethods);
  }, [session.state.kind]);
  const [user, setUser] = useState<string>(Object.keys(USERS)[0] ?? "");
  const [tenants, setTenants] = useState<{ id: string; name: string }[]>(TENANT?.dev ?? []);
  const [tenant, setTenant] = useState(TENANT?.dev?.[0]?.id ?? "");

  useEffect(() => {
    document.title = TITLE;
  }, []);

  const runtimes = useRef(new Map<string, OperationRuntime>());
  const actor = session.state.kind === "signed-in" ? session.state.actor : user;
  const priorActor = useRef(actor);
  const scope = actor + ":" + tenant;
  if (!runtimes.current.has(scope)) runtimes.current.set(scope, new OperationRuntime());
  const runtime = runtimes.current.get(scope)!;
  const transport = useMemo(
    () =>
      createTransport({
        metaPrefix: "/_meta/v1/admin",
        onAccessRefused: () => { operationRuntime(transport).refresh(); transport.invalidate?.(); setAccessRefused(true); },
        headers: () => ({
          ...(session.state.kind === "unavailable" && USERS[user] ? { Authorization: `Bearer ${USERS[user]!.token}` } : sessionHeaders(session.state)),
          ...(TENANT && tenant ? { [TENANT.header]: tenant } : {}),
        }),
      }),
    [signedIn, session.state, user, tenant],
  );

  bindOperationRuntime(transport, runtime);
  useEffect(() => {
    if (priorActor.current !== actor) {
      for (const [key, previous] of runtimes.current) if (!key.startsWith(actor + ":")) { previous.clear(); runtimes.current.delete(key); }
      priorActor.current = actor;
    }
  }, [actor]);
  useEffect(() => { if (session.state.kind === "signed-out") { runtimes.current.forEach((r) => r.clear()); runtimes.current.clear(); setTenants([]); setTenant(""); setAccessRefused(false); } }, [session.state.kind]);
  useEffect(() => () => { operationRuntime(transport).refresh(); transport.invalidate?.(); }, [transport]);

  // Tenant discovery is an authorized operation; the host adds no role rule.
  useEffect(() => {
    if (!signedIn || !TENANT) return;
    let current = true;
    void transport.send({ method: "GET", path: TENANT.path }).then((reply) => {
      if (!current) return;
      if (reply.status !== 200) { setTenants([]); setTenant(""); return; }
      const idField = TENANT.idField ?? "id";
      const items = object(reply.body).items;
      if (!Array.isArray(items) || items.length > 100) { setTenants([]); setTenant(""); return; }
      const owned = items.map((m) => object(m)).filter((m) => typeof m[idField] === "string" && typeof m.name === "string").map((m) => ({ id: String(m[idField]), name: String(m.name) }));
      setTenants(owned);
      setTenant((current) => (owned.some((t) => t.id === current) ? current : (owned[0]?.id ?? "")));
    }).catch(() => { if (current) { setTenants([]); setTenant(""); } });
    return () => { current = false; };
  }, [signedIn, transport]);


  if (session.state.kind === "loading") return <p className="app">Loading…</p>;
  if (session.state.kind === "error") return <div className="app"><p role="alert">Your session could not be checked.</p><button onClick={() => void refreshSession()}>Refresh access</button></div>;
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

  if (!signedIn && !Object.keys(USERS).length) return <p role="alert">Browser identity is unavailable. Configure sign-in before using this workspace.</p>;
  if (accessRefused) return <div className="app"><h1>Access needs refreshing</h1><p>Your protected workspace has been cleared.</p><button onClick={() => void refreshSession().then(() => setAccessRefused(false))}>Refresh access</button><button onClick={() => void session.signOut()}>Sign out</button></div>;
  if (TENANT && !tenant) return <div className="app"><h1>{TITLE}</h1><p>No eligible tenants are available.</p><button onClick={() => void session.signOut()}>Sign out</button></div>;
  return <AdminShell transport={transport} title={TITLE} header={<div className="identity">
    {tenantPicker}
    {signedIn ? <button onClick={() => void session.signOut()}>Sign out</button> : <label>Development user<select value={user} onChange={(e) => setUser(e.target.value)}>{Object.keys(USERS).map((name) => <option key={name}>{name}</option>)}</select></label>}
  </div>} />;
}
