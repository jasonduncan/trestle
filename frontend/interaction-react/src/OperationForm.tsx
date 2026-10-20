// Runtime-rendered form for one registered interaction (design doc §9).
//
// Loads the caller's descriptor, checks protocol features and widgets,
// validates locally from the descriptor's schema (feedback only), submits
// through the descriptor's binding with its revision, and maps problem+json
// responses back onto fields. It never retries a write on its own.

import { useCallback, useEffect, useId, useMemo, useRef, useState, useSyncExternalStore, type ReactNode, type FormEvent } from "react";

import { operationRuntime } from "@trestle-framework/client";

import { decodeDescriptor, type Descriptor, type FormLayout, type FieldSchema, type PresentedField } from "./protocol";
import { asProblem, etagVersion, fillPath, type RecordContext, type Transport } from "./transport";
import { validateInstance, type FieldError } from "./validate";
import { defaultWidgets, words, type Widget, type WidgetRegistry } from "./widgets";

export interface OperationFormProps {
  /** Interaction ID, e.g. `lessons.create.form`. */
  interaction: string;
  transport: Transport;
  widgets?: WidgetRegistry;
  apiPrefix?: string;
  /**
   * The record an edit form changes: its path values, `ETag` and current
   * field values. Only fields the person changes are sent (§9.3).
   */
  record?: RecordContext & { values?: Record<string, unknown> };
  expectedRevision?: string;
  flowId?: string;
  controllerKey?: string;
  boundInputs?: Record<string, unknown>;
  onDirtyChange?: (dirty: boolean) => void;
  onSuccess?: (output: unknown, descriptor: Descriptor, etag?: string) => void;
}

interface RenderField {
  field: FieldSchema;
  presented: PresentedField;
  widget: Widget;
}

type Loaded =
  | { phase: "loading" }
  | { phase: "blocked"; message: string }
  | { phase: "ready"; descriptor: Descriptor; fields: RenderField[]; unsupported: string[] };

type Values = Record<string, string | boolean>;

/** Friendly text for stable problem codes; anything else uses detail/title. */
const MESSAGES: Record<string, string> = {
  permission_denied: "You don't have permission to do this.",
  tenant_not_permitted: "You're not a member of the selected tenant.",
  credentials_missing: "Please sign in.",
  credentials_invalid: "Your session is no longer valid. Please sign in again.",
  outcome_unknown: "We couldn't confirm whether this was saved. Check before trying again.",
  precondition_failed: "Someone changed this since you opened it. Reload to see the latest version, then try again.",
  precondition_required: "This form needs the record's version. Reload and try again.",
  stale_version: "Someone changed this since you opened it. Reload to see the latest version, then try again.",
};


function prepare(descriptor: Descriptor, widgets: WidgetRegistry, boundInputs: Record<string, unknown> = {}): { fields: RenderField[]; unsupported: string[] } {
  const unsupported = descriptor.missingFeatures.map((f) => `needs renderer feature ${f}`);
  const fields: RenderField[] = [];
  for (const presented of descriptor.presentation.fields) {
    const field = descriptor.input.schema.fields.find((f) => f.name === presented.name);
    const widget = widgets[presented.widget];
    if (!field || Object.hasOwn(boundInputs, field.name)) continue;
    if (!widget || !widget.accepts(field)) {
      // An optional field this app cannot edit is left out; a required one
      // makes the form unusable (§9.2).
      if (field.required) unsupported.push(`field "${presented.label}" needs control ${presented.widget}`);
      continue;
    }
    fields.push({ field, presented, widget });
  }
  // Path fields and the precondition version come from the record, not a person.
  const bound = new Set([...descriptor.operation.pathParams, descriptor.operation.precondition?.field, ...Object.keys(boundInputs)]);
  for (const field of descriptor.input.schema.fields) {
    if (bound.has(field.name)) continue;
    if (field.required && !fields.some((f) => f.field.name === field.name) && !unsupported.some((u) => u.includes(field.name))) {
      unsupported.push(`required field ${field.name} is not presented`);
    }
  }
  return { fields, unsupported };
}

function initialValues(
  fields: RenderField[],
  previous: Values,
  previousFields: RenderField[],
  record?: Record<string, unknown>,
): Values {
  const values: Values = {};
  for (const { field, widget } of fields) {
    const before = previousFields.find((p) => p.field.name === field.name);
    const kept = before && before.field.kind === field.kind ? previous[field.name] : undefined;
    const fromRecord = record && field.name in record ? widget.fromJson(record[field.name], field) : undefined;
    values[field.name] = kept ?? fromRecord ?? widget.empty(field);
  }
  return values;
}

export function OperationForm({
  interaction,
  transport,
  widgets = defaultWidgets,
  apiPrefix,
  record,
  onSuccess,
  expectedRevision,
  flowId = "default",
  controllerKey,
  boundInputs,
  onDirtyChange,
}: OperationFormProps) {
  const baseId = useId();
  const runtime = operationRuntime(transport);
  const epoch = useSyncExternalStore(runtime.subscribe, runtime.epoch);
  const observedEpoch = useRef(epoch);
  const target = JSON.stringify(record?.path ?? {});
  const controller = useMemo(() => operationRuntime(transport).operation(controllerKey ?? interaction + ":" + target + ":" + flowId, transport), [transport, controllerKey, interaction, target, flowId]);
  const attempt = useSyncExternalStore(controller.subscribe, controller.getSnapshot);
  const unresolved = ["submitting", "outcome_unknown", "committed_unreadable", "queued"].includes(attempt.state);
  const [loaded, setLoaded] = useState<Loaded>({ phase: "loading" });
  const [values, setValues] = useState<Values>({});
  const [fieldErrors, setFieldErrors] = useState<Record<string, FieldError[]>>({});
  const [formErrors, setFormErrors] = useState<string[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const submitting = attempt.state === "submitting";
  const [focusErrors, setFocusErrors] = useState(0);
  const summaryRef = useRef<HTMLDivElement>(null);
  const loadGeneration = useRef(0);
  const current = useRef<{ values: Values; fields: RenderField[] }>({ values: {}, fields: [] });
  /** Edit forms: the values as loaded, to send only what changed. */
  const baseline = useRef<Values>({});
  const [etag, setEtag] = useState(record?.etag);
  const recordValues = record?.values;
  useEffect(() => {
    if (observedEpoch.current === epoch) return;
    observedEpoch.current = epoch;
    loadGeneration.current++;
    current.current = { values: {}, fields: [] }; baseline.current = {};
    setValues({}); setEtag(undefined); setFieldErrors({}); setFormErrors([]);
    setLoaded({ phase: "blocked", message: "Access context changed. Reload before continuing." });
  }, [epoch]);

  const load = useCallback(
    async (keep: boolean) => {
      const generation = ++loadGeneration.current;
      const reply = await transport.getInteraction(interaction);
      if (generation !== loadGeneration.current) return;
      if (reply.kind === "network-error") {
        setLoaded({ phase: "blocked", message: "This form could not be loaded. Check your connection and try again." });
        return;
      }
      if (reply.status !== 200) {
        const problem = asProblem(reply.body);
        const message = (problem.code && MESSAGES[problem.code]) ?? problem.detail ?? problem.title ?? `This form could not be loaded (${reply.status}).`;
        setLoaded({ phase: "blocked", message });
        return;
      }
      const decoded = decodeDescriptor(reply.body, { apiPrefix, bytes: reply.bytes });
      if (!decoded.ok) {
        setLoaded({ phase: "blocked", message: `This version of the app can't show this form (${decoded.error}).` });
        return;
      }
      const mixedRevision = expectedRevision && decoded.descriptor.revision !== expectedRevision;
      if (mixedRevision && !keep) {
        setLoaded({ phase: "blocked", message: "The view and form are from different releases. Reload the workspace before continuing." });
        return;
      }
      const precondition = decoded.descriptor.operation.precondition;
      if (precondition?.target) {
        const target = precondition.target;
        const value = boundInputs?.[target.inputField] ?? record?.path?.[target.inputField] ?? recordValues?.[target.inputField];
        const current = value === undefined ? undefined : await transport.list(fillPath(target.path, { [target.parameter]: value }));
        if (generation !== loadGeneration.current) return;
        if (current?.kind !== "response" || current.status !== 200 || !current.etag?.startsWith('"' + precondition.etagOf + ':')) {
          setLoaded({ phase: "blocked", message: "The concurrency target is unavailable. Refresh your access or record." }); return;
        }
        setEtag(current.etag);
      }
      const { fields, unsupported } = prepare(decoded.descriptor, widgets, boundInputs);
      if (mixedRevision) unsupported.push("The workspace and form are from different releases. Copy your retained entries before reloading the workspace.");
      baseline.current = initialValues(fields, {}, [], recordValues);
      const next = keep
        ? initialValues(fields, current.current.values, current.current.fields, recordValues)
        : baseline.current;
      current.current = { values: next, fields };
      setValues(next);
      setLoaded({ phase: "ready", descriptor: decoded.descriptor, fields, unsupported });
    },
    [interaction, transport, widgets, apiPrefix, recordValues, expectedRevision, boundInputs],
  );

  useEffect(() => {
    setLoaded({ phase: "loading" });
    void load(false);
    return () => { loadGeneration.current++; };
  }, [load]);

  useEffect(() => {
    const warn = (event: BeforeUnloadEvent) => {
      if (JSON.stringify(current.current.values) !== JSON.stringify(baseline.current)) { event.preventDefault(); event.returnValue = ""; }
    };
    const navigate = (event: Event) => {
      const dirty = JSON.stringify(current.current.values) !== JSON.stringify(baseline.current);
      const unresolved = ["submitting", "outcome_unknown", "queued"].includes(controller.getSnapshot().state);
      if (dirty && !unresolved && !window.confirm("Discard unsaved changes?")) event.preventDefault();
    };
    window.addEventListener("beforeunload", warn);
    window.addEventListener("trestle:navigate", navigate);
    return () => { window.removeEventListener("beforeunload", warn); window.removeEventListener("trestle:navigate", navigate); };
  }, []);

  useEffect(() => {
    if (focusErrors > 0) summaryRef.current?.focus();
  }, [focusErrors]);

  if (loaded.phase === "loading") {
    return <p aria-busy="true">Loading form…</p>;
  }
  if (loaded.phase === "blocked") {
    return <p role="alert">{loaded.message}</p>;
  }

  const { descriptor, fields, unsupported } = loaded;
  const available = descriptor.availability?.available !== false;
  // Separate namespaces so a field named "title" cannot collide with the heading.
  const fieldId = (name: string) => `${baseId}-field-${name}`;
  const titleId = `${baseId}-heading`;

  const setValue = (name: string, value: string | boolean) => {
    const next = { ...current.current.values, [name]: value };
    current.current.values = next;
    setValues(next);
    onDirtyChange?.(JSON.stringify(next) !== JSON.stringify(baseline.current));
  };

  const showErrors = (errors: FieldError[], fallback: string[]) => {
    const byField: Record<string, FieldError[]> = {};
    const general = [...fallback];
    for (const error of errors) {
      // `/price/amount` belongs to field `price`; the member names the part.
      const [name = "", ...members] = error.pointer.slice(1).split("/");
      const named = members.length > 0 ? { ...error, message: `${members.map(words).join(" ")}: ${error.message}` } : error;
      if (fields.some((f) => f.field.name === name)) (byField[name] ??= []).push(named);
      else general.push(named.message);
    }
    setFieldErrors(byField);
    setFormErrors(general);
    setFocusErrors((n) => n + 1);
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (unresolved || unsupported.length || descriptor.availability?.available === false) return;
    setNotice(null);
    const editing = descriptor.operation.method === "PATCH";
    const payload: Record<string, unknown> = { ...boundInputs };
    for (const { field, widget } of fields) {
      const raw = values[field.name] ?? widget.empty(field);
      if (editing && raw === baseline.current[field.name]) continue;
      let json = widget.toJson(raw, field);
      // Clearing a nullable field in an edit is an explicit null (§5.3).
      if (json === undefined && editing && field.nullable) json = null;
      if (json !== undefined) payload[field.name] = json;
    }
    // Values bound from the record, not typed: path fields and the version.
    for (const name of descriptor.operation.pathParams) {
      const bound = record?.path?.[name];
      if (bound !== undefined) payload[name] = bound;
    }
    const precondition = descriptor.operation.precondition;
    if (precondition) {
      const version = etagVersion(etag);
      if (version !== undefined) payload[precondition.field] = version;
    }
    const local = validateInstance(descriptor.input.schema, payload);
    if (!local.ok) {
      showErrors(local.errors, local.errors.length === 0 ? ["This form could not be submitted."] : []);
      setStatus("");
      return;
    }

    const prepared = controller.prepare(descriptor, local.value, { path: record?.path, etag });
    if (!prepared) {
      const snapshot = controller.getSnapshot();
      showErrors(snapshot.errors, [snapshot.message]);
      return;
    }
    await controller.execute(prepared);
    const outcome = controller.getSnapshot();
    if (outcome.state === "succeeded" || outcome.state === "queued") {
      onDirtyChange?.(false);
      setFieldErrors({});
      setFormErrors([]);
      if (editing) {
        // The saved values are the new baseline; the new ETag guards the next edit.
        baseline.current = { ...values };
        setEtag(outcome.etag);
      } else {
        const cleared = initialValues(fields, {}, []);
        current.current.values = cleared;
        setValues(cleared);
      }
      setStatus("Saved.");
      onSuccess?.(outcome.result, descriptor, outcome.etag);
      return;
    }
    setStatus("");
    if (outcome.code === "interaction_stale") {
      setFieldErrors({}); setFormErrors([]);
      await load(true);
      setNotice("This form changed since you opened it. Review your entries, then submit again.");
      return;
    }
    showErrors(outcome.errors, outcome.errors.length ? [] : [outcome.message]);
  };

  const allErrors = fields.flatMap((f) => (fieldErrors[f.field.name] ?? []).map((e) => ({ name: f.field.name, label: f.presented.label, message: e.message })));
  const blocked = unsupported.length > 0;

  const renderField = ({ field, presented, widget }: RenderField) => {
        const id = fieldId(field.name);
        const errors = fieldErrors[field.name] ?? [];
        const helpId = presented.help ? `${id}-help` : undefined;
        const errorId = errors.length > 0 ? `${id}-error` : undefined;
        const describedBy = [helpId, errorId].filter(Boolean).join(" ") || undefined;
        return (
          <div className="trestle-field" key={field.name}>
            <label htmlFor={id}>
              {presented.label}
              {field.required ? "" : " (optional)"}
            </label>
            {presented.help && (
              <p className="trestle-help" id={helpId}>
                {presented.help}
              </p>
            )}
            {widget.render({
              id,
              field,
              presented,
              transport,
              value: values[field.name] ?? widget.empty(field),
              onChange: (value) => setValue(field.name, value),
              invalid: errors.length > 0,
              describedBy,
            })}
            {errors.length > 0 && (
              <p className="trestle-error" id={errorId}>
                {errors.map((e) => e.message).join(" ")}
              </p>
            )}
          </div>
        );
  };

  const renderLayout = (node: FormLayout): ReactNode => {
    if (node.kind === "field") { const field = fields.find((f) => f.presented.pointer === node.pointer); return field ? renderField(field) : null; }
    const children = node.children?.map((child, i) => <div key={child.id ?? child.pointer ?? i}>{renderLayout(child)}</div>);
    if (node.kind === "section") return <fieldset className="trestle-section"><legend>{node.label}</legend>{children}</fieldset>;
    return <div className={node.kind === "columns" ? "trestle-columns" : "trestle-vertical"}>{children}</div>;
  };

  return (
    <form className="trestle-form" aria-labelledby={titleId} noValidate onSubmit={(e) => void submit(e)}>
      <h2 id={titleId}>{descriptor.presentation.title}</h2>
      {notice && (
        <p className="trestle-notice" role="status">
          {notice}
        </p>
      )}
      {(allErrors.length > 0 || formErrors.length > 0) && (
        <div className="trestle-summary" role="alert" tabIndex={-1} ref={summaryRef}>
          <p>Please fix the following:</p>
          <ul>
            {formErrors.map((message, i) => (
              <li key={`form-${i}`}>{message}</li>
            ))}
            {allErrors.map((e, i) => (
              <li key={`${e.name}-${i}`}>
                <a href={`#${fieldId(e.name)}`}>
                  {e.label}: {e.message}
                </a>
              </li>
            ))}
          </ul>
        </div>
      )}
      {blocked && (
        <div className="trestle-unsupported" role="alert">
          <p>This form needs a newer version of the app, so it can't be submitted here.</p>
          <ul>
            {unsupported.map((reason) => (
              <li key={reason}>{reason}</li>
            ))}
          </ul>
        </div>
      )}
      {!available && (
        <p className="trestle-unavailable" role="note">
          {(descriptor.availability?.reason && MESSAGES[descriptor.availability.reason]) ?? "You can't submit this form right now."}
        </p>
      )}
      {descriptor.presentation.layout ? renderLayout(descriptor.presentation.layout) : fields.map(renderField)}
      <button type="submit" disabled={unresolved || blocked || !available}>
        {descriptor.presentation.submitLabel}
      </button>
      <p className="trestle-status" aria-live="polite">
        {unresolved ? attempt.message : status}
      </p>
    </form>
  );
}
