import { createContext, createElement, useContext, useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { OperationForm, defaultWidgets, type WidgetRegistry } from "@trestle/interaction-react";
import { at, currentAvailability, decodeView, decodeViewManifest, etagVersion, fillPath, formatValue, object, operationRuntime, routeInput, validateInstance,
  type Descriptor, type OperationController, type Transport, type View, type ViewAction, type ViewEntry, type ViewSection } from "@trestle/client";

const Widgets = createContext<WidgetRegistry>(defaultWidgets);
class PresentationError extends Error {}
export interface AdminShellProps {
  widgets?: WidgetRegistry;
  transport: Transport;
  title?: string;
  profile?: "admin" | "product";
  manifestPath?: string;
  apiPrefix?: string;
  header?: ReactNode;
  /** Build-time components; never imports from metadata. */
  sections?: Record<string, (props: { view: View; record: Record<string, unknown>; transport: Transport }) => ReactNode>;
  components?: Record<string, (props: { data: unknown; transport: Transport }) => ReactNode>;
}

function useRead<T>(load: () => Promise<T>, dependencies: unknown[]) {
  const runtime = operationRuntime(dependencies[0] as Transport);
  const epoch = useSyncExternalStore(runtime.subscribe, runtime.epoch);
  dependencies = [...dependencies, epoch];
  const [state, setState] = useState<{ value?: T; error?: string; loading: boolean; generation?: number }>({ loading: true });
  const [revision, refresh] = useState(0);
  const identity = useRef({ dependencies: [] as unknown[], generation: 0 });
  const inputs = [...dependencies, revision];
  if (inputs.length !== identity.current.dependencies.length || inputs.some((value, i) => !Object.is(value, identity.current.dependencies[i]))) {
    identity.current = { dependencies: inputs, generation: identity.current.generation + 1 };
  }
  const generation = identity.current.generation;
  useEffect(() => {
    let current = true;
    setState({ loading: true, generation });
    void load().then((value) => { if (current && generation === identity.current.generation) setState({ value, loading: false, generation }); })
      .catch((error) => { if (current && generation === identity.current.generation) setState({ error: error instanceof PresentationError ? error.message : "This view could not be loaded. Refresh or check your access.", loading: false, generation }); });
    return () => { current = false; };
  // Each caller supplies primitive request identity and security-generation dependencies.
  }, [...dependencies, revision]);
  return { ...(state.generation === generation ? state : { loading: true, value: undefined, error: undefined }), refresh: () => refresh((n) => n + 1) };
}

export function OperationDialog({ title, children, onClose }: { title: string; children: ReactNode; onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const previous = document.activeElement;
    const dialog = ref.current;
    dialog?.showModal();
    return () => { dialog?.close(); if (previous instanceof HTMLElement) previous.focus(); };
  }, []);
  return <dialog ref={ref} className="trestle-dialog" aria-label={title} onCancel={(event) => { event.preventDefault(); onClose(); }}>
    {children}<button type="button" className="secondary" onClick={onClose}>Close</button>
  </dialog>;
}

export function Activity({ transport }: { transport: Transport }) {
  const runtime = operationRuntime(transport);
  const active = useSyncExternalStore(runtime.subscribe, runtime.getSnapshot);
  if (!active.length) return null;
  return <aside className="trestle-activity" aria-label="Activity"><h2>Activity</h2>{active.map((attempt, index) =>
    <div key={attempt.attemptId ?? index}><strong>{attempt.label}</strong><p role="status">{attempt.message}</p>{attempt.requestId && <small>Request {attempt.requestId}</small>}
      {attempt.result && typeof attempt.result === "object" && "reveal_once" in attempt.result && typeof attempt.result.reveal_once === "string" ? <div>
        <p>Copy this value now. It will not be displayed after dismissal.</p><input readOnly aria-label="One-time result" value={attempt.result.reveal_once} onFocus={(e) => e.currentTarget.select()} />
        <button onClick={() => runtime.dismissResult(attempt)}>Done</button></div> : null}
    </div>)}
  </aside>;
}

export function OperationAction({ action, record, etag, transport, onSuccess, available = true }: {
  action: ViewAction; record?: Record<string, unknown>; etag?: string; transport: Transport; onSuccess: () => void; available?: boolean;
}) {
  const widgets = useContext(Widgets);
  const [open, setOpen] = useState(false);
  const [dirty, setDirty] = useState(false);
  const close = () => { if (!dirty || pending || window.confirm("Discard unsaved changes?")) { setDirty(false); setOpen(false); } };
  const context = useMemo(() => Object.fromEntries(Object.entries(action.context).map(([key, ptr]) => [key, at(record, ptr)])), [action, record]);
  const path = useMemo(() => Object.fromEntries(action.invocation.operation.pathParams.map((name) => [name, String(context[name] ?? "")])), [action, context]);
  const target = JSON.stringify(path);
  const controller = useMemo(() => operationRuntime(transport).operation(action.invocation.operation.id + ":" + target, transport), [transport, action.invocation.operation.id, target]);
  const snapshot = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
  const pending = ["submitting", "outcome_unknown", "committed_unreadable", "queued"].includes(snapshot.state);
  const concurrency = useRead(async () => {
    const precondition = action.invocation.operation.precondition;
    if (!open || action.interactionId || !precondition?.target) return etag;
    const target = precondition.target;
    const reply = await transport.list(fillPath(target.path, { [target.parameter]: context[target.inputField] }), {});
    if (reply.kind !== "response" || reply.status !== 200 || !reply.etag?.startsWith('"' + precondition.etagOf + ":")) throw new Error("Concurrency target unavailable");
    return reply.etag;
  }, [transport, open, action.invocation.revision, target, etag]);
  if (!available) return <span><button type="button" disabled>{action.label}</button><small>Unavailable for this record or your current access.</small></span>;
  if (action.visibleWhen && at(record, action.visibleWhen.pointer) !== action.visibleWhen.equals) return <span><button type="button" disabled>{action.label}</button><small>Unavailable in the current record state.</small></span>;
  const execute = async () => {
    const input = { ...context };
    const precondition = action.invocation.operation.precondition;
    if (precondition) input[precondition.field] = etagVersion(concurrency.value);
    const attempt = controller.prepare(action.invocation, input, { path, etag: concurrency.value });
    if (!attempt) return;
    await controller.execute(attempt);
    if (controller.getSnapshot().state === "succeeded") { setOpen(false); onSuccess(); }
  };
  return <>
    <button type="button" onClick={() => setOpen(true)}>{action.label}</button>
    {open && <OperationDialog title={action.label} onClose={close}>
      {action.interactionId ? <OperationForm widgets={widgets} interaction={action.interactionId} expectedRevision={action.interactionRevision ?? undefined}
        transport={transport} boundInputs={context} controllerKey={action.invocation.operation.id + ":" + target} onDirtyChange={setDirty} record={record ? { values: record, path, etag } : undefined}
        onSuccess={() => { setDirty(false); setOpen(false); onSuccess(); }} /> : <>
        <h2>{action.label}</h2><p>Confirm this action on the current record.</p>
        <button type="button" disabled={pending || concurrency.loading || !!concurrency.error} onClick={() => void execute()}>Confirm {action.label.toLowerCase()}</button>
        {concurrency.error && <p role="alert">The concurrency target could not be loaded. Refresh the record before continuing.</p>}
        <p role="status">{snapshot.message}</p>
      </>}
    </OperationDialog>}
  </>;
}

function RecordActions({ view, record, etag, transport, onSuccess }: { view: View; record: Record<string, unknown>; etag?: string; transport: Transport; onSuccess: () => void }) {
  const hints = useRead(async () => {
    if (!view.availability) return undefined;
    const { invocation, input: mapping } = view.availability;
    const input = Object.fromEntries(Object.entries(mapping).map(([field, ptr]) => [field, at(record, ptr)]));
    if (!validateInstance(invocation.input.schema, input).ok) throw new Error("Invalid availability input");
    const query = Object.fromEntries(Object.entries(input).filter(([key]) => !invocation.operation.pathParams.includes(key)).map(([key, value]) => [key, String(value)]));
    const reply = await transport.list(fillPath(invocation.operation.path, input), query);
    if (reply.kind !== "response" || reply.status !== 200 || !validateInstance(invocation.output.schema, reply.body).ok) throw new Error("Availability unavailable");
    const current = currentAvailability(reply.body, view, record, etag);
    if (!current) throw new Error("Availability observation changed");
    return current;
  }, [transport, view.revision, JSON.stringify(record), etag]);
  return <div className="trestle-record-actions">{hints.error && <p role="status">Refresh the record to update action availability.</p>}{view.actions.filter((a) => Object.keys(a.context).length > 0).map((action) =>
    <OperationAction key={action.id} action={action} record={record} etag={etag} transport={transport} onSuccess={onSuccess} available={!view.availability || hints.value?.[action.id]?.available === true} />)}</div>;
}

export function CollectionView({ view, transport, navigate }: { view: View; transport: Transport; navigate: (id: string, input?: Record<string, unknown>) => void }) {
  const [filters, setFilters] = useState<Record<string, string>>({});
  const [cursors, setCursors] = useState<string[]>([]);
  const queryKey = JSON.stringify(filters), cursor = cursors.at(-1);
  const data = useRead(async () => {
    const query = { ...(view.source.invocation.input.schema.fields.some((f) => f.name === "limit") ? { limit: "20" } : {}), ...filters, ...(cursor ? { cursor } : {}) };
    const reply = await transport.list(view.source.invocation.operation.path, query);
    if (reply.kind !== "response" || reply.status !== 200 || !validateInstance(view.source.invocation.output.schema, reply.body).ok) throw new Error("Invalid page");
    const page = object(reply.body), rows = at(page, view.source.itemsPointer!);
    if (!Array.isArray(rows) || rows.length > 100) throw new Error("Invalid page size");
    const identities = rows.map((row) => String(at(row, view.source.identityPointer)));
    if (new Set(identities).size !== identities.length) throw new Error("Duplicate record identity");
    return { rows: rows as Record<string, unknown>[], next: at(page, view.source.nextCursorPointer!) };
  }, [transport, view.revision, queryKey, cursor]);
  const newActions = view.actions.filter((a) => !Object.keys(a.context).length);
  return <section aria-label={view.title}>
    <div className="trestle-toolbar"><div><p className="trestle-eyebrow">Workspace</p><h1>{view.title}</h1></div>
      <div>{newActions.map((action) => <OperationAction key={action.id} action={action} transport={transport} onSuccess={data.refresh} />)}</div></div>
    <div className="trestle-filters">{view.filters.map((name) => {
      const field = view.source.invocation.input.schema.fields.find((f) => f.name === name)!;
      const change = (value: string) => { setFilters((before) => ({ ...before, [name]: value })); setCursors([]); };
      return <label key={name}>{name === "q" ? "Search" : name.replace(/_/g, " ")}{field.choices ? <select value={filters[name] ?? ""} onChange={(e) => change(e.target.value)}>
        <option value="">Any</option>{field.choices.map((v) => <option key={v}>{v}</option>)}
      </select> : <input type={field.kind === "integer" ? "number" : "search"} min={field.minimum} max={field.maximum} value={filters[name] ?? ""} onChange={(e) => change(e.target.value)} />}</label>;
    })}</div>
    {data.loading ? <p role="status">Loading records…</p> : data.error ? <p role="alert">{data.error}</p> : <>
      {!data.value?.rows.length ? <div className="trestle-empty"><h2>No records yet</h2><p>Records will appear here when they are available.</p></div> :
      <div className="trestle-table-wrap"><table><thead><tr>{view.columns.map((col) => <th scope="col" key={col.id}>{col.label}</th>)}{view.recordView && <th scope="col">Record</th>}</tr></thead>
        <tbody>{data.value.rows.map((row) => <tr key={String(at(row, view.source.identityPointer))}>
          {view.columns.map((col) => <td key={col.id}>{formatValue(at(row, col.pointer))}</td>)}
          {view.recordView && <td><button className="trestle-link" onClick={() => navigate(view.recordView!, Object.fromEntries(Object.entries(view.recordInput).map(([key, ptr]) => [key, at(row, ptr)])))}>
            Open {formatValue(at(row, view.labelPointer))}</button></td>}
        </tr>)}</tbody></table></div>}
      <div className="trestle-pagination"><button disabled={!cursors.length} onClick={() => setCursors((history) => history.slice(0, -1))}>Previous</button>
        <span>Page {cursors.length + 1}</span><button disabled={typeof data.value?.next !== "string"} onClick={() => setCursors((history) => [...history, String(data.value?.next)])}>Next</button></div>
    </>}
  </section>;
}

export function RelatedCollection({ section, record, transport, components = {} }: { section: ViewSection; record: Record<string, unknown>; transport: Transport; components?: AdminShellProps["components"] }) {
  const [open, setOpen] = useState(false);
  return <section className="trestle-related"><h2><button type="button" aria-expanded={open} onClick={() => setOpen((v) => !v)}>{section.title}</button></h2>
    {open && <RelatedSource section={section} record={record} transport={transport} components={components} />}
  </section>;
}

function RelatedSource({ section, record, transport, components }: { section: ViewSection; record: Record<string, unknown>; transport: Transport; components: NonNullable<AdminShellProps["components"]> }) {
  const input = Object.fromEntries(Object.entries(section.input).map(([field, ptr]) => [field, at(record, ptr)]));
  const data = useRead(async () => {
    const invocation = section.source.invocation;
    if (!validateInstance(invocation.input.schema, input).ok) throw new Error("Section input unavailable");
    const query = Object.fromEntries(Object.entries(input).filter(([field]) => !invocation.operation.pathParams.includes(field)).map(([field, value]) => [field, String(value)]));
    const reply = await transport.list(fillPath(invocation.operation.path, input), query);
    if (reply.kind !== "response" || reply.status !== 200 || !validateInstance(invocation.output.schema, reply.body).ok) throw new Error("Section unavailable");
    return reply.body;
  }, [transport, section.source.invocation.revision, JSON.stringify(input)]);
  if (data.loading) return <p role="status">Loading {section.title}…</p>;
  if (data.error) return <p role="alert">{data.error}</p>;
  if (section.component && components[section.component]) return createElement(components[section.component]!, { data: data.value, transport });
  if (section.requiredComponent) return <p role="alert">This section requires an installed component.</p>;
  const rows = section.source.itemsPointer ? at(data.value, section.source.itemsPointer) : [data.value];
  if (!Array.isArray(rows) || rows.length > 100) return <p role="alert">Section response exceeds its limit.</p>;
  return <table><thead><tr>{section.columns.map((c) => <th scope="col" key={c.id}>{c.label}</th>)}</tr></thead><tbody>
    {rows.map((row) => <tr key={String(at(row, section.source.identityPointer))}>{section.columns.map((c) => <td key={c.id}>{formatValue(at(row, c.pointer))}</td>)}</tr>)}
  </tbody></table>;
}

/** Status comes from an application-authorized read, never a raw job helper. */
export function WorkStatus({ source, input, transport, interval = 3000, controller }: { source: Descriptor; input: Record<string, unknown>; transport: Transport; interval?: number; controller?: OperationController }) {
  const [tick, setTick] = useState(0);
  const data = useRead(async () => {
    if (source.operation.method !== "GET" || !validateInstance(source.input.schema, input).ok) throw new Error("Invalid status read");
    const query = Object.fromEntries(Object.entries(input).filter(([key]) => !source.operation.pathParams.includes(key)).map(([key, value]) => [key, String(value)]));
    const reply = await transport.list(fillPath(source.operation.path, input), query);
    if (reply.kind !== "response" || reply.status !== 200 || !validateInstance(source.output.schema, reply.body).ok) throw new Error("Status unavailable");
    return object(reply.body);
  }, [transport, source.revision, JSON.stringify(input), tick]);
  const state = data.value?.state;
  useEffect(() => { if (data.value) controller?.observeWork(data.value); }, [data.value, controller]);
  useEffect(() => {
    if (data.error || ["succeeded", "failed", "cancelled"].includes(String(state))) return;
    const timer = setTimeout(() => setTick((n) => n + 1), Math.max(1000, interval));
    return () => clearTimeout(timer);
  }, [state, data.error, interval, tick]);
  return <p role="status">{data.error ?? (state ? "Work: " + String(state) : "Loading work status…")}</p>;
}

export function RecordView({ view, input, transport, sections = {}, components }: { view: View; input: Record<string, unknown>; transport: Transport; sections?: AdminShellProps["sections"]; components?: AdminShellProps["components"] }) {
  input = routeInput(view.source.invocation.input.schema, input);
  const inputKey = JSON.stringify(input);
  const data = useRead(async () => {
    const checked = validateInstance(view.source.invocation.input.schema, input);
    if (!checked.ok) throw new Error("Invalid record input");
    const op = view.source.invocation.operation;
    const query = Object.fromEntries(Object.entries(input).filter(([key]) => !op.pathParams.includes(key)).map(([key, value]) => [key, String(value)]));
    const reply = await transport.list(fillPath(op.path, input), query);
    if (reply.kind !== "response" || reply.status !== 200) throw new Error("Record unavailable");
    const output = validateInstance(view.source.invocation.output.schema, reply.body);
    if (!output.ok) throw new Error("Invalid record");
    return { record: output.value, etag: reply.etag };
  }, [transport, view.revision, inputKey]);
  if (data.loading) return <p role="status">Loading record…</p>;
  if (!data.value) return <p role="alert">{data.error}</p>;
  const { record, etag } = data.value;
  return <section><div className="trestle-toolbar"><div><p className="trestle-eyebrow">{view.title}</p><h1>{formatValue(at(record, view.labelPointer))}</h1></div><button onClick={data.refresh}>Refresh record</button></div>
    <RecordActions view={view} record={record} etag={etag} transport={transport} onSuccess={data.refresh} />
    <dl className="trestle-record">{view.columns.map((col) => <div key={col.id}><dt>{col.label}</dt><dd>{formatValue(at(record, col.pointer))}</dd></div>)}</dl>
    {sections[view.viewId] && createElement(sections[view.viewId]!, { view, record, transport })}
    {view.sections.map((section) => <RelatedCollection key={section.id} section={section} record={record} transport={transport} components={components} />)}
  </section>;
}

export function AdminShell({ widgets = defaultWidgets, transport, title = "Workspace", profile = "admin", manifestPath = "/_meta/v1/ui/manifest", apiPrefix, header, sections, components }: AdminShellProps) {
  const manifest = useRead(async () => {
    const reply = await transport.list(manifestPath);
    if (reply.kind === "response" && reply.status === 404) throw new PresentationError("This server does not publish application views. Use a compatible frontend and backend release.");
    if (reply.kind !== "response" || reply.status !== 200) throw new Error("Manifest unavailable");
    return decodeViewManifest(reply.body, { bytes: reply.bytes }).filter((v) => v.audience === profile);
  }, [transport, manifestPath, profile]);
  const [route, setRoute] = useState(() => window.location.hash.slice(1));
  useEffect(() => { const changed = () => { if (window.dispatchEvent(new Event("trestle:navigate", { cancelable: true }))) setRoute(window.location.hash.slice(1)); else window.history.replaceState(null, "", "#" + route); }; window.addEventListener("hashchange", changed); return () => window.removeEventListener("hashchange", changed); }, [route]);
  const navigate = useCallback((id: string, input: Record<string, unknown> = {}) => {
    window.location.hash = encodeURIComponent(id) + "?" + new URLSearchParams(Object.entries(input).map(([key, value]) => [key, String(value)])).toString();
  }, []);
  const [viewId = "", query = ""] = route.split("?");
  let decodedId = "";
  try { decodedId = decodeURIComponent(viewId); } catch { /* invalid route cannot select protected data */ }
  const entry = decodedId ? manifest.value?.find((v) => v.viewId === decodedId) : manifest.value?.find((v) => v.kind === "collection");
  const navigation = manifest.value?.filter((v) => v.kind === "collection") ?? [];
  return <Widgets.Provider value={widgets}><div className="trestle-admin"><aside className="trestle-navigation"><a className="trestle-brand" href="#">{title}</a><p className="trestle-eyebrow">Collections</p>
    <nav aria-label="Workspace">{navigation.map((v) => <button key={v.viewId} aria-current={entry?.viewId === v.viewId ? "page" : undefined} onClick={() => navigate(v.viewId)}>{v.title}</button>)}</nav>
    {header}</aside><main className="trestle-content">{manifest.loading ? <p role="status">Loading workspace…</p> : manifest.error ? <p role="alert">{manifest.error}</p> : entry ?
      <LoadedView key={entry.viewId + ":" + query} entry={entry} transport={transport} input={Object.fromEntries(new URLSearchParams(query))} apiPrefix={apiPrefix} navigate={navigate} sections={sections} components={components} /> :
      <div className="trestle-empty"><h1>No views available</h1><p>Your workspace has no published views.</p></div>}
      <Activity transport={transport} />
    </main></div></Widgets.Provider>;
}

function LoadedView({ entry, transport, input, apiPrefix, navigate, sections, components }: { entry: ViewEntry; transport: Transport; input: Record<string, unknown>; apiPrefix?: string; navigate: (id: string, input?: Record<string, unknown>) => void; sections?: AdminShellProps["sections"]; components?: AdminShellProps["components"] }) {
  const data = useRead(async () => {
    const reply = await transport.list(entry.url);
    if (reply.kind !== "response" || reply.status !== 200) throw new Error("Descriptor unavailable");
    const view = decodeView(reply.body, { bytes: reply.bytes, apiPrefix });
    if (view.viewId !== entry.viewId || view.revision !== entry.revision || view.audience !== entry.audience) throw new Error("Incompatible metadata deployment");
    return view;
  }, [transport, entry.viewId, entry.revision, apiPrefix]);
  if (data.loading) return <p role="status">Loading view…</p>;
  if (!data.value) return <p role="alert">{data.error}</p>;
  const view = data.value;
  if (view.missingFeatures.length) return <p role="alert">This view needs a newer renderer: {view.missingFeatures.join(", ")}.</p>;
  if (view.sections.some((s) => s.requiredComponent && (!s.component || !components?.[s.component]))) return <p role="alert">This view requires a component missing from this build.</p>;
  return view.kind === "collection" ? <CollectionView view={view} transport={transport} navigate={navigate} /> : <RecordView view={view} input={input} transport={transport} sections={sections} components={components} />;
}

export { AdminHost, type AdminHostProps, type TenantProfile } from "./host";
