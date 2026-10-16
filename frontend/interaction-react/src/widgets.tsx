// Local widget registry (§9.2). A widget key in a descriptor is a request
// for a renderer shipped in this bundle, never a package name or URL.
// Applications may add or override widgets by key.

import { useEffect, useState, type ReactNode } from "react";

import type { FieldSchema, PresentedField } from "./protocol";
import type { Transport } from "./transport";

export interface WidgetProps {
  id: string;
  field: FieldSchema;
  presented: PresentedField;
  transport: Transport;
  /** Raw UI value: string for text-like inputs, boolean for checkboxes. */
  value: string | boolean;
  onChange: (value: string | boolean) => void;
  invalid: boolean;
  describedBy?: string;
}

export interface Widget {
  /** Whether this widget can edit a field of this schema. */
  accepts(field: FieldSchema): boolean;
  /** Initial raw value. */
  empty(field: FieldSchema): string | boolean;
  /** Raw UI value → JSON value, or `undefined` to leave the field out. */
  toJson(raw: string | boolean, field: FieldSchema): unknown;
  /** A record's JSON value → raw UI value (edit forms). */
  fromJson(value: unknown, field: FieldSchema): string | boolean;
  render(props: WidgetProps): ReactNode;
}

export type WidgetRegistry = Readonly<Record<string, Widget>>;

function common(props: WidgetProps) {
  return {
    id: props.id,
    name: props.field.name,
    "aria-invalid": props.invalid || undefined,
    "aria-describedby": props.describedBy,
    "aria-required": props.field.required || undefined,
  };
}

/** Empty optional text is left out; empty required text is sent as "". */
function textToJson(raw: string | boolean, field: FieldSchema): unknown {
  const text = String(raw);
  return text === "" && !field.required ? undefined : text;
}

const asText = (value: unknown) => (value === null || value === undefined ? "" : String(value));

const text: Widget = {
  accepts: (f) => f.kind === "string" && !f.choices && f.format === undefined,
  empty: () => "",
  toJson: textToJson,
  fromJson: asText,
  render: (props) => (
    <input
      {...common(props)}
      type="text"
      value={String(props.value)}
      onChange={(e) => props.onChange(e.target.value)}
    />
  ),
};

const textarea: Widget = {
  ...text,
  render: (props) => (
    <textarea {...common(props)} value={String(props.value)} onChange={(e) => props.onChange(e.target.value)} />
  ),
};

const integer: Widget = {
  accepts: (f) => f.kind === "integer",
  empty: () => "",
  fromJson: asText,
  toJson: (raw) => {
    const text = String(raw).trim();
    if (text === "") return undefined;
    // Not a number: send the text so validation reports the type error.
    return /^-?\d+$/.test(text) ? Number(text) : text;
  },
  render: (props) => (
    <input
      {...common(props)}
      type="text"
      inputMode="numeric"
      value={String(props.value)}
      onChange={(e) => props.onChange(e.target.value)}
    />
  ),
};

const checkbox: Widget = {
  accepts: (f) => f.kind === "boolean",
  empty: () => false,
  fromJson: (value) => value === true,
  toJson: (raw) => raw === true,
  render: (props) => (
    <input {...common(props)} type="checkbox" checked={props.value === true} onChange={(e) => props.onChange(e.target.checked)} />
  ),
};

const select: Widget = {
  accepts: (f) => f.kind === "string" && Array.isArray(f.choices),
  empty: () => "",
  fromJson: asText,
  toJson: (raw, field) => (raw === "" ? (field.nullable ? null : undefined) : String(raw)),
  render: (props) => (
    <select {...common(props)} value={String(props.value)} onChange={(e) => props.onChange(e.target.value)}>
      <option value="">Choose…</option>
      {(props.field.choices ?? []).map((choice) => (
        <option key={choice} value={choice}>
          {choice}
        </option>
      ))}
    </select>
  ),
};

/** Most options a relation picker renders (§8.5). */
export const MAX_OPTIONS = 100;

type Options = { state: "loading" } | { state: "error" } | { state: "ready"; items: { value: string; label: string }[] };

/**
 * Options come from a scoped list operation the server authorizes; the
 * chosen ID is revalidated on write (§9.3). Values stay IDs; labels are text.
 */
function RelationSelect(props: WidgetProps) {
  const source = props.presented.options;
  const [options, setOptions] = useState<Options>({ state: "loading" });
  useEffect(() => {
    if (!source) return;
    let cancelled = false;
    void props.transport.list(source.path, { limit: String(MAX_OPTIONS) }).then((reply) => {
      if (cancelled) return;
      const items = reply.kind === "response" && reply.status === 200 ? (reply.body as { items?: unknown }).items : undefined;
      if (!Array.isArray(items)) {
        setOptions({ state: "error" });
        return;
      }
      setOptions({
        state: "ready",
        items: items
          .slice(0, MAX_OPTIONS)
          .filter((i): i is Record<string, unknown> => typeof i === "object" && i !== null)
          .map((i) => ({ value: String(i[source.value] ?? ""), label: String(i[source.label] ?? "") }))
          .filter((o) => o.value !== ""),
      });
    });
    return () => {
      cancelled = true;
    };
  }, [source, props.transport]);

  const value = String(props.value);
  return (
    <select
      {...common(props)}
      value={value}
      onChange={(e) => props.onChange(e.target.value)}
      aria-busy={options.state === "loading" || undefined}
    >
      <option value="">{props.field.required ? "Choose…" : "None"}</option>
      {options.state === "ready" &&
        options.items.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      {/* Keep a current value visible even if it is not in the first page. */}
      {options.state === "ready" && value !== "" && !options.items.some((o) => o.value === value) && (
        <option value={value}>{value}</option>
      )}
    </select>
  );
}

const relation: Widget = {
  accepts: (f) => f.kind === "string" && f.format === "uuid",
  empty: () => "",
  fromJson: asText,
  toJson: (raw, field) => (raw === "" ? (field.nullable ? null : undefined) : String(raw)),
  render: (props) => <RelationSelect {...props} />,
};

export const defaultWidgets: WidgetRegistry = { text, textarea, integer, checkbox, select, relation };
