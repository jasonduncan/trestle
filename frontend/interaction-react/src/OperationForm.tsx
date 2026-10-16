// Runtime-rendered form for one registered interaction (design doc §9).
//
// Loads the caller's descriptor, checks protocol features and widgets,
// validates locally from the descriptor's schema (feedback only), submits
// through the descriptor's binding with its revision, and maps problem+json
// responses back onto fields. It never retries a write on its own.

import { useCallback, useEffect, useId, useRef, useState, type FormEvent } from "react";

import { decodeDescriptor, type Descriptor, type FieldSchema, type PresentedField } from "./protocol";
import { asProblem, etagVersion, type RecordContext, type Transport } from "./transport";
import { validateInstance, type FieldError } from "./validate";
import { defaultWidgets, type Widget, type WidgetRegistry } from "./widgets";

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
  tenant_not_permitted: "You're not a member of the selected school.",
  credentials_missing: "Please sign in.",
  credentials_invalid: "Your session is no longer valid. Please sign in again.",
  outcome_unknown: "We couldn't confirm whether this was saved. Check before trying again.",
  precondition_failed: "Someone changed this since you opened it. Reload to see the latest version, then try again.",
  precondition_required: "This form needs the record's version. Reload and try again.",
  stale_version: "Someone changed this since you opened it. Reload to see the latest version, then try again.",
};

const UNKNOWN_OUTCOME = "We couldn't confirm whether this was saved. Check before trying again.";

function prepare(descriptor: Descriptor, widgets: WidgetRegistry): { fields: RenderField[]; unsupported: string[] } {
  const unsupported = descriptor.missingFeatures.map((f) => `needs renderer feature ${f}`);
  const fields: RenderField[] = [];
  for (const presented of descriptor.presentation.fields) {
    const field = descriptor.input.schema.fields.find((f) => f.name === presented.name);
    const widget = widgets[presented.widget];
    if (!field) continue;
    if (!widget || !widget.accepts(field)) {
      // An optional field this app cannot edit is left out; a required one
      // makes the form unusable (§9.2).
      if (field.required) unsupported.push(`field "${presented.label}" needs control ${presented.widget}`);
      continue;
    }
    fields.push({ field, presented, widget });
  }
  // Path fields and the precondition version come from the record, not a person.
  const bound = new Set([...descriptor.operation.pathParams, descriptor.operation.precondition?.field]);
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
}: OperationFormProps) {
  const baseId = useId();
  const [loaded, setLoaded] = useState<Loaded>({ phase: "loading" });
  const [values, setValues] = useState<Values>({});
  const [fieldErrors, setFieldErrors] = useState<Record<string, FieldError[]>>({});
  const [formErrors, setFormErrors] = useState<string[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [status, setStatus] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [focusErrors, setFocusErrors] = useState(0);
  const summaryRef = useRef<HTMLDivElement>(null);
  const current = useRef<{ values: Values; fields: RenderField[] }>({ values: {}, fields: [] });
  /** Edit forms: the values as loaded, to send only what changed. */
  const baseline = useRef<Values>({});
  const [etag, setEtag] = useState(record?.etag);
  const recordValues = record?.values;

  const load = useCallback(
    async (keep: boolean) => {
      const reply = await transport.getInteraction(interaction);
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
      const { fields, unsupported } = prepare(decoded.descriptor, widgets);
      baseline.current = initialValues(fields, {}, [], recordValues);
      const next = keep
        ? initialValues(fields, current.current.values, current.current.fields, recordValues)
        : baseline.current;
      current.current = { values: next, fields };
      setValues(next);
      setLoaded({ phase: "ready", descriptor: decoded.descriptor, fields, unsupported });
    },
    [interaction, transport, widgets, apiPrefix, recordValues],
  );

  useEffect(() => {
    setLoaded({ phase: "loading" });
    void load(false);
  }, [load]);

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
  };

  const showErrors = (errors: FieldError[], fallback: string[]) => {
    const byField: Record<string, FieldError[]> = {};
    const general = [...fallback];
    for (const error of errors) {
      const name = error.pointer.slice(1);
      if (fields.some((f) => f.field.name === name)) (byField[name] ??= []).push(error);
      else general.push(error.message);
    }
    setFieldErrors(byField);
    setFormErrors(general);
    setFocusErrors((n) => n + 1);
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (submitting) return;
    setNotice(null);
    const editing = descriptor.operation.method === "PATCH";
    const payload: Record<string, unknown> = {};
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

    setSubmitting(true);
    setStatus("Saving…");
    const reply = await transport.submit(descriptor, local.value, { path: record?.path, etag });
    setSubmitting(false);
    if (reply.kind === "network-error") {
      setFieldErrors({});
      setFormErrors([UNKNOWN_OUTCOME]);
      setStatus("");
      setFocusErrors((n) => n + 1);
      return;
    }
    if (reply.status === descriptor.operation.successStatus) {
      setFieldErrors({});
      setFormErrors([]);
      if (editing) {
        // The saved values are the new baseline; the new ETag guards the next edit.
        baseline.current = { ...values };
        setEtag(reply.etag);
      } else {
        const cleared = initialValues(fields, {}, []);
        current.current.values = cleared;
        setValues(cleared);
      }
      setStatus("Saved.");
      onSuccess?.(reply.body, descriptor, reply.etag);
      return;
    }
    const problem = asProblem(reply.body);
    setStatus("");
    if (reply.status === 409 && problem.code === "interaction_stale") {
      // The server refused this revision before doing anything. Reload the
      // form, keep compatible entries, and let the person review (§11.4).
      setFieldErrors({});
      setFormErrors([]);
      await load(true);
      setNotice("This form changed since you opened it. Review your entries, then submit again.");
      return;
    }
    const message = (problem.code && MESSAGES[problem.code]) ?? problem.detail ?? problem.title ?? `The request failed (${reply.status}).`;
    const errors = problem.errors ?? [];
    showErrors(errors, errors.length > 0 ? [] : [message]);
  };

  const allErrors = fields.flatMap((f) => (fieldErrors[f.field.name] ?? []).map((e) => ({ name: f.field.name, label: f.presented.label, message: e.message })));
  const blocked = unsupported.length > 0;

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
      {fields.map(({ field, presented, widget }) => {
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
      })}
      <button type="submit" disabled={submitting || blocked || !available}>
        {descriptor.presentation.submitLabel}
      </button>
      <p className="trestle-status" aria-live="polite">
        {status}
      </p>
    </form>
  );
}
