import { validateInstance } from "./validate";
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
  return props.presented.options?.lookup ? <RelationLookup {...props} /> : <LegacyRelationSelect {...props} />;
}

function RelationLookup(props: WidgetProps) {
  const source = props.presented.options!;
  const lookup = source.lookup!;
  const [term, setTerm] = useState("");
  const [cursor, setCursor] = useState<string | undefined>();
  const [next, setNext] = useState<string | null>(null);
  const [options, setOptions] = useState<Options>({ state: "loading" });
  const [selected, setSelected] = useState<{ value: string; label: string } | null>(null);
  const value = String(props.value ?? "");
  useEffect(() => {
    let current = true;
    setSelected(null);
    if (value) void props.transport.list(lookup.resolvePath.replace("{id}", encodeURIComponent(value))).then((reply) => {
      if (!current) return;
      const record = reply.kind === "response" && reply.status === 200 && validateInstance(lookup.resolveBinding.output.schema, reply.body).ok ? reply.body as Record<string, unknown> : null;
      setSelected({ value, label: record?.[source.value] === value && typeof record[source.label] === "string" ? String(record[source.label]) : "Selection unavailable" });
    });
    return () => { current = false; };
  }, [value, props.transport, lookup.resolvePath, source.value, source.label]);
  useEffect(() => {
    let current = true;
    setOptions({ state: "loading" });
    const timer = setTimeout(() => {
      void props.transport.list(source.path, { limit: "100", [lookup.searchField]: term, ...(cursor ? { cursor } : {}) }).then((reply) => {
        if (!current) return;
        const page = reply.kind === "response" && reply.status === 200 && validateInstance(lookup.searchBinding.output.schema, reply.body).ok ? reply.body as Record<string, unknown> : null;
        if (!page || !Array.isArray(page.items) || page.items.length > 100 || (page.next_cursor !== null && typeof page.next_cursor !== "string")) { setOptions({ state: "error" }); return; }
        const items: { value: string; label: string }[] = [];
        for (const item of page.items) {
          if (!item || typeof item !== "object" || typeof item[source.value] !== "string" || typeof item[source.label] !== "string") { setOptions({ state: "error" }); return; }
          items.push({ value: item[source.value], label: item[source.label] });
        }
        setOptions({ state: "ready", items }); setNext(page.next_cursor as string | null);
      });
    }, 250);
    return () => { current = false; clearTimeout(timer); };
  }, [props.transport, source.path, source.value, source.label, lookup.searchField, term, cursor]);
  return <div className="trestle-relation">
    <input aria-label={"Search " + props.presented.label} type="search" maxLength={200} value={term} onChange={(event) => { setTerm(event.target.value); setCursor(undefined); }} />
    <select {...common(props)} value={value} onChange={(event) => props.onChange(event.target.value)} aria-busy={options.state === "loading"}>
      <option value="">{props.field.required ? "Choose…" : "None"}</option>
      {options.state === "ready" && options.items.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
      {value && (options.state !== "ready" || !options.items.some((o) => o.value === value)) && <option value={value}>{selected?.label ?? "Resolving selection…"}</option>}
    </select>
    <p role="status">{options.state === "loading" ? "Searching…" : options.state === "error" ? "Choices are unavailable." : !options.items.length ? "No matches." : ""}</p>
    {next && options.state === "ready" && <button type="button" onClick={() => setCursor(next)}>More choices</button>}
    {cursor && <button type="button" onClick={() => setCursor(undefined)}>First choices</button>}
  </div>;
}

function LegacyRelationSelect(props: WidgetProps) {
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

/** A string in one format, edited as typed text; empty optional is left out. */
function formatted(format: NonNullable<FieldSchema["format"]>, input: Partial<Record<string, string>>): Widget {
  return {
    accepts: (f) => f.kind === "string" && f.format === format,
    empty: () => "",
    fromJson: asText,
    toJson: (raw, field) => {
      const text = String(raw).trim();
      return text === "" ? (field.nullable ? null : undefined) : text;
    },
    render: (props) => (
      <input {...common(props)} {...input} value={String(props.value)} onChange={(e) => props.onChange(e.target.value)} />
    ),
  };
}

/** Decimals stay strings, so no float ever rounds them (ADR-022). */
const decimal = formatted("decimal", { type: "text", inputMode: "decimal" });
/** `<input type="date">` already speaks YYYY-MM-DD. */
const date = formatted("date", { type: "date" });
const duration = formatted("duration", { type: "text", placeholder: "PT30M" });

/** Local time in the picker; sent as an instant in UTC. */
const datetime: Widget = {
  accepts: (f) => f.kind === "string" && f.format === "date-time",
  empty: () => "",
  // `datetime-local` wants local "YYYY-MM-DDTHH:MM[:SS]".
  fromJson: (value) => {
    if (typeof value !== "string") return "";
    const at = new Date(value);
    if (Number.isNaN(at.getTime())) return "";
    const local = new Date(at.getTime() - at.getTimezoneOffset() * 60000);
    return local.toISOString().slice(0, 19);
  },
  toJson: (raw, field) => {
    const text = String(raw).trim();
    if (text === "") return field.nullable ? null : undefined;
    const at = new Date(text);
    // Not a time: send the text so validation reports the format error.
    return Number.isNaN(at.getTime()) ? text : at.toISOString();
  },
  render: (props) => (
    <input
      {...common(props)}
      type="datetime-local"
      step="1"
      value={String(props.value)}
      onChange={(e) => props.onChange(e.target.value)}
    />
  ),
};

/**
 * Amount and currency in one raw string, `"<amount> <currency>"`: neither
 * part can contain a space, so the split is exact (ADR-027).
 */
function moneyParts(raw: string | boolean): [string, string] {
  const text = String(raw);
  const space = text.indexOf(" ");
  return space === -1 ? [text, ""] : [text.slice(0, space), text.slice(space + 1)];
}

const money: Widget = {
  accepts: (f) => f.kind === "money",
  empty: (f) => (f.choices?.length === 1 ? ` ${f.choices[0]}` : " "),
  fromJson: (value) => {
    if (typeof value !== "object" || value === null) return " ";
    const v = value as { amount?: unknown; currency?: unknown };
    return `${asText(v.amount)} ${asText(v.currency)}`;
  },
  toJson: (raw, field) => {
    const [amount, currency] = moneyParts(raw).map((p) => p.trim()) as [string, string];
    if (amount === "" && (currency === "" || field.choices?.length === 1)) {
      return field.nullable ? null : undefined;
    }
    return { amount, currency };
  },
  render: (props) => {
    const [amount, currency] = moneyParts(props.value);
    const set = (a: string, c: string) => props.onChange(`${a.replace(/ /g, "")} ${c.replace(/ /g, "")}`);
    const choices = props.field.choices;
    return (
      <span role="group">
        <input
          {...common(props)}
          type="text"
          inputMode="decimal"
          value={amount}
          onChange={(e) => set(e.target.value, currency)}
        />
        {choices ? (
          <select
            name={`${props.field.name}.currency`}
            aria-label="Currency"
            value={currency}
            onChange={(e) => set(amount, e.target.value)}
          >
            <option value="">…</option>
            {choices.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        ) : (
          <input
            name={`${props.field.name}.currency`}
            aria-label="Currency"
            type="text"
            size={3}
            maxLength={3}
            value={currency}
            onChange={(e) => set(amount, e.target.value.toUpperCase())}
          />
        )}
      </span>
    );
  },
};

/** Field names as words, for members a descriptor does not label. */
export function words(name: string): string {
  const text = name.replace(/_/g, " ");
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** A uuid typed as text: a union member has no option source to pick from. */
const uuid = formatted("uuid", { type: "text", spellCheck: "false" });

/** The widget a union member gets, by its schema alone. */
function memberWidget(field: FieldSchema): Widget | undefined {
  return [checkbox, integer, money, select, decimal, date, datetime, duration, uuid, text].find((w) => w.accepts(field));
}

/**
 * The chosen variant and each variant's raw member values, as JSON in the
 * one raw string; switching variants keeps what was typed in the others.
 */
interface UnionRaw {
  variant: string;
  members: Record<string, Record<string, string | boolean>>;
}

function unionParts(raw: string | boolean): UnionRaw {
  try {
    const parsed = JSON.parse(String(raw)) as Partial<UnionRaw>;
    if (typeof parsed.variant === "string" && typeof parsed.members === "object" && parsed.members !== null) {
      return { variant: parsed.variant, members: parsed.members };
    }
  } catch {
    // Not ours: start empty.
  }
  return { variant: "", members: {} };
}

const union: Widget = {
  accepts: (f) => f.kind === "union" && (f.variants ?? []).every((v) => v.fields.every((m) => memberWidget(m) !== undefined)),
  empty: (f) => JSON.stringify({ variant: f.variants?.length === 1 ? f.variants[0]?.name : "", members: {} }),
  fromJson: (value, field) => {
    const tag = field.tag ?? "";
    const record = typeof value === "object" && value !== null ? (value as Record<string, unknown>) : {};
    const variant = field.variants?.find((v) => v.name === record[tag]);
    if (!variant) return union.empty(field);
    const members: Record<string, string | boolean> = {};
    for (const member of variant.fields) {
      const widget = memberWidget(member);
      if (widget) members[member.name] = widget.fromJson(record[member.name], member);
    }
    return JSON.stringify({ variant: variant.name, members: { [variant.name]: members } });
  },
  toJson: (raw, field) => {
    const { variant: name, members } = unionParts(raw);
    const variant = field.variants?.find((v) => v.name === name);
    if (!variant) return field.nullable ? null : undefined;
    const typed = members[name] ?? {};
    const value: Record<string, unknown> = { [field.tag ?? ""]: name };
    for (const member of variant.fields) {
      const widget = memberWidget(member);
      const json = widget?.toJson(typed[member.name] ?? widget.empty(member), member);
      if (json !== undefined) value[member.name] = json;
    }
    return value;
  },
  render: (props) => {
    const parts = unionParts(props.value);
    const variants = props.field.variants ?? [];
    const variant = variants.find((v) => v.name === parts.variant);
    const typed = parts.members[parts.variant] ?? {};
    const set = (next: UnionRaw) => props.onChange(JSON.stringify(next));
    return (
      <span role="group">
        <select
          {...common(props)}
          value={parts.variant}
          onChange={(e) => set({ ...parts, variant: e.target.value })}
        >
          <option value="">{props.field.required ? "Choose…" : "None"}</option>
          {variants.map((v) => (
            <option key={v.name} value={v.name}>
              {words(v.name)}
            </option>
          ))}
        </select>
        {variant?.fields.map((member) => {
          const widget = memberWidget(member);
          if (!widget) return null;
          const id = `${props.id}-${member.name}`;
          return (
            <span className="trestle-member" key={`${variant.name}.${member.name}`}>
              <label htmlFor={id}>
                {words(member.name)}
                {member.required ? "" : " (optional)"}
              </label>
              {widget.render({
                ...props,
                id,
                field: member,
                presented: { ...props.presented, pointer: `${props.presented.pointer}/${member.name}`, name: member.name, label: words(member.name) },
                value: typed[member.name] ?? widget.empty(member),
                onChange: (value) =>
                  set({ ...parts, members: { ...parts.members, [variant.name]: { ...typed, [member.name]: value } } }),
              })}
            </span>
          );
        })}
      </span>
    );
  },
};

export const defaultWidgets: WidgetRegistry = {
  union,
  money,
  text,
  textarea,
  integer,
  checkbox,
  select,
  relation,
  decimal,
  date,
  datetime,
  duration,
};
