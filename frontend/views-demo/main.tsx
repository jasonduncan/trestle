import { useEffect, useMemo, useState } from "react";
import { OperationForm, defaultWidgets, type WidgetRegistry } from "@trestle-framework/interaction-react";
import { createRoot } from "react-dom/client";
import { AdminShell, WorkStatus } from "@trestle-framework/admin-react";
import { createTransport, decodeView, formatValue, operationRuntime, type Descriptor, type Transport } from "@trestle-framework/client";
import "@trestle-framework/admin-react/styles.css";

// One fixture bundle, with build-time extensions. The renderer has no domain
// branches; browser tests bind it to each app's registry-generated metadata.
const profile = new URLSearchParams(location.search).get("profile") === "product" ? "product" : "admin";
const transport = createTransport({ metaPrefix: profile === "admin" ? "/_meta/v1/admin" : "/_meta/v1" });
function ExportPanel({ data, transport }: { data: unknown; transport: Transport }) {
  const id = String((data as { id: string }).id);
  const record = useMemo(() => ({ path: { case_id: id } }), [id]);
  const controllerKey = "support_work.start:" + JSON.stringify(record.path);
  const controller = operationRuntime(transport).operation(controllerKey, transport);
  const [work, setWork] = useState<string>(), [source, setSource] = useState<Descriptor>();
  useEffect(() => {
    let current = true;
    void transport.list("/_meta/v1/ui/views/support_work.status").then((reply) => {
      if (current && reply.kind === "response" && reply.status === 200) setSource(decodeView(reply.body).source.invocation);
    });
    return () => { current = false; };
  }, [transport]);
  return <div><p>Queue a durable preview of this case's authorized representation.</p>
    <OperationForm interaction="support_work.export.form" controllerKey={controllerKey} transport={transport} record={record} onSuccess={(result) => setWork(String((result as { id: string }).id))} />
    {work && source && <WorkStatus controller={controller} source={source} input={{ id: work }} transport={transport} interval={1000} />}
  </div>;
}
const widgets: WidgetRegistry = { ...defaultWidgets, text: { ...defaultWidgets.text!, render: (props) =>
  <input className="fixture-text" id={props.id} value={String(props.value)} aria-invalid={props.invalid || undefined} aria-describedby={props.describedBy}
    aria-required={props.field.required} onChange={(event) => props.onChange(event.target.value)} /> } };
const components = { "fixture.export_panel": ExportPanel, "fixture.account_summary": ({ data }: { data: unknown }) => <article aria-label="Account summary"><h3>Account summary</h3><p>{formatValue((data as { name: string }).name)}</p></article> };
createRoot(document.getElementById("root")!).render(<AdminShell widgets={widgets} transport={transport} title="Trestle workspace" profile={profile} components={components} />);
