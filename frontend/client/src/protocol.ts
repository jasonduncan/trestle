import { decodeInvocation } from "./views";
// Decoder for `trestle.interactions/1.x` descriptors (design doc §8, App. B).
//
// A descriptor is untrusted network input. Decoding checks the protocol,
// limits, the supported JSON Schema subset and the HTTP binding, and turns
// the wire format into the small typed model the renderer uses. Anything
// outside the supported profile is rejected rather than approximated.

export const PROTOCOL = "trestle.interactions";
export const SUPPORTED_MAJOR = 1;
export const REVISION_HEADER = "Trestle-Interaction-Revision";
export const VALIDATION_PROFILE = "trestle.portable@1";
export const JSON_SCHEMA_2020_12 = "https://json-schema.org/draft/2020-12/schema";

/** Features this renderer implements. Unknown required features block submission. */
export const SUPPORTED_FEATURES: ReadonlySet<string> = new Set([
  "forms.core@1",
  "forms.layout@1",
  "forms.relation-lookup@1",
  "http.json@1",
  "forms.relation@1",
  "http.precondition@1",
  "http.precondition-target@1",
  "http.operation-version@1",
  "forms.decimal@1",
  "forms.temporal@1",
  "forms.money@1",
  "forms.union@1",
]);

/** Same numbers as `trestle_protocol::limits` (§8.5). */
export const LIMITS = {
  descriptorBytes: 256 * 1024,
  formFields: 64,
  contractFields: 64,
} as const;

const FORBIDDEN_NAMES = new Set(["__proto__", "constructor", "prototype"]);
const FIELD_NAME = /^[a-z][a-z0-9_]*$/;
const DIGITS_PATTERN = "^[0-9]+$";
/** `trestle_protocol::scalar::DURATION_PATTERN`, character for character. */
export const DURATION_PATTERN =
  "^P([0-9]+D(T([0-9]+H([0-9]+M)?([0-9]+S)?|[0-9]+M([0-9]+S)?|[0-9]+S))?|T([0-9]+H([0-9]+M)?([0-9]+S)?|[0-9]+M([0-9]+S)?|[0-9]+S))$";

/** The pattern `trestle_protocol::scalar::decimal_pattern` writes. */
export function decimalPattern(precision: number, scale: number): string {
  const whole = precision - scale === 0 ? "0" : `(0|[1-9][0-9]{0,${precision - scale - 1}})`;
  const fraction = scale === 0 ? "" : `(\\.[0-9]{1,${scale}})?`;
  return `^-?${whole}${fraction}$`;
}

/** Precision and scale of a pattern only `decimalPattern` produces. */
function decimalDigits(pattern: string): { precision: number; scale: number } | undefined {
  const m = /^\^-\?(?:0|\(0\|\[1-9\]\[0-9\]\{0,(\d+)\}\))(?:\(\\\.\[0-9\]\{1,(\d+)\}\)\?)?\$$/.exec(pattern);
  if (!m) return undefined;
  const scale = m[2] === undefined ? 0 : Number(m[2]);
  const precision = (m[1] === undefined ? 0 : Number(m[1]) + 1) + scale;
  return decimalPattern(precision, scale) === pattern && precision >= 1 && precision <= 38 ? { precision, scale } : undefined;
}

export type FieldKind = "string" | "integer" | "boolean" | "money" | "union" | "items";

/** One shape of a union field, named by its tag value (ADR-028). */
export interface UnionVariant {
  name: string;
  /** The members besides the tag, in schema order. */
  fields: FieldSchema[];
}

export interface FieldSchema {
  name: string;
  kind: FieldKind;
  required: boolean;
  nullable: boolean;
  minLength?: number;
  maxLength?: number;
  minimum?: number;
  maximum?: number;
  choices?: string[];
  /** Portable string formats (ADR-022 adds decimal, date, date-time, duration). */
  format?: "uuid" | "digits" | "decimal" | "date" | "date-time" | "duration";
  /** For `decimal` and `money`: total and fractional digits. For money,
   * `choices` lists the currencies when they are limited. */
  precision?: number;
  scale?: number;
  /** For `union`: the member that names the variant, and the variants. */
  tag?: string;
  variants?: UnionVariant[];
  description?: string;
  items?: ObjectSchema;
  maxItems?: number;
}

export interface ObjectSchema {
  /** In schema property order; presentation decides display order. */
  fields: FieldSchema[];
}

export interface RelationOptions {
  operation: string;
  /** A list binding under the API prefix; items carry `value` and `label`. */
  path: string;
  value: string;
  label: string;
  lookup?: { resolveOperation: string; resolvePath: string; searchField: string; searchBinding: Descriptor; resolveBinding: Descriptor };
}

export interface PresentedField {
  pointer: string;
  name: string;
  widget: string;
  label: string;
  help?: string;
  options?: RelationOptions;
}

export interface Availability {
  available: boolean;
  reason?: string;
}

export interface FormLayout { kind: "field" | "vertical" | "section" | "columns"; pointer?: string; id?: string; label?: string; children?: FormLayout[] }

export interface Descriptor {
  interactionId: string;
  revision: string;
  audience: string;
  requiredFeatures: string[];
  /** Required features this renderer lacks; non-empty means "do not submit". */
  missingFeatures: string[];
  operation: {
    id: string;
    version: string;
    method: string;
    /** May contain `{name}` segments filled from `pathParams`. */
    path: string;
    pathParams: string[];
    body: "input" | "none";
    revisionHeader?: string;
    idempotencyKey?: boolean;
    operationVersionHeader?: boolean;
    /** When set, submit with `If-Match` and fill `field` from its version. */
    precondition?: { field: string; etagOf: string; target?: { operation: string; path: string; parameter: string; inputField: string } };
    successStatus: number;
    successBody: "output" | "none";
    errors: number[];
  };
  input: { id: string; version: string; schema: ObjectSchema };
  output: { id: string; version: string; schema: ObjectSchema };
  presentation: { title: string; fields: PresentedField[]; submitLabel: string; layout?: FormLayout };
  availability?: Availability;
}

export type Decoded = { ok: true; descriptor: Descriptor } | { ok: false; error: string };

export interface DecodeOptions {
  /** Bindings must stay under this path prefix on the API origin. */
  apiPrefix?: string;
  /** Size of the raw response body, when known. */
  bytes?: number;
}

class DecodeError extends Error {}

function fail(message: string): never {
  throw new DecodeError(message);
}

type Json = Record<string, unknown>;

function object(value: unknown, what: string): Json {
  if (typeof value !== "object" || value === null || Array.isArray(value)) fail(`${what} must be an object`);
  return value as Json;
}

function string(value: unknown, what: string): string {
  if (typeof value !== "string") fail(`${what} must be a string`);
  return value;
}

function integer(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value)) fail(`${what} must be an integer`);
  return value;
}

function onlyKeys(value: Json, allowed: string[], what: string): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) fail(`${what}: unsupported keyword ${JSON.stringify(key)}`);
  }
}

/** `trestle_protocol::scalar::CURRENCY_PATTERN`. */
export const CURRENCY_PATTERN = "^[A-Z]{3}$";

/**
 * Money is the one object a portable field may be (ADR-027), and only in
 * the exact shape the server writes: `{amount: decimal, currency: code}`.
 */
function decodeMoney(name: string, s: Json, required: boolean, nullable: boolean): FieldSchema {
  onlyKeys(s, ["type", "additionalProperties", "required", "properties", "description"], `schema for ${name}`);
  const props = object(s.properties, `${name}.properties`);
  const req = Array.isArray(s.required) ? s.required : [];
  if (s.additionalProperties !== false || req.length !== 2 || req[0] !== "amount" || req[1] !== "currency" || Object.keys(props).length !== 2) {
    fail(`schema for ${name}: unsupported object`);
  }
  const amount = object(props.amount, `${name}.amount`);
  const currency = object(props.currency, `${name}.currency`);
  onlyKeys(amount, ["type", "pattern"], `${name}.amount`);
  onlyKeys(currency, ["type", "pattern", "enum"], `${name}.currency`);
  const digits = amount.type === "string" && typeof amount.pattern === "string" ? decimalDigits(amount.pattern) : undefined;
  if (!digits) fail(`schema for ${name}: unsupported amount`);
  const field: FieldSchema = { name, kind: "money", required, nullable, precision: digits.precision, scale: digits.scale };
  if (currency.type !== "string") fail(`schema for ${name}: unsupported currency`);
  if (currency.enum !== undefined && currency.pattern === undefined && Array.isArray(currency.enum)) {
    field.choices = currency.enum.map((v) => string(v, `${name}.currency value`));
  } else if (currency.pattern !== CURRENCY_PATTERN || currency.enum !== undefined) {
    fail(`schema for ${name}: unsupported currency`);
  }
  if (s.description !== undefined) field.description = string(s.description, `${name}.description`);
  return field;
}

/**
 * A tagged union (ADR-028), in the exact shape the server writes: `oneOf`
 * object branches that each pin the same tag member with `const`, then a
 * `{type: "null"}` branch when nullable. Variants hold no unions.
 */
function decodeUnion(name: string, s: Json, required: boolean): FieldSchema {
  onlyKeys(s, ["oneOf", "description"], `schema for ${name}`);
  const branches = Array.isArray(s.oneOf) ? s.oneOf.map((b, i) => object(b, `${name}.oneOf[${i}]`)) : [];
  const last = branches.at(-1);
  const nullable = last !== undefined && last.type === "null" && Object.keys(last).length === 1;
  if (nullable) branches.pop();
  const first = branches[0];
  if (!first) fail(`schema for ${name}: a union needs variants`);
  const pins = Object.entries(object(first.properties, `${name}.oneOf[0].properties`)).filter(
    ([, v]) => typeof v === "object" && v !== null && "const" in v,
  );
  const tag = pins.length === 1 ? pins[0]?.[0] : undefined;
  if (tag === undefined || !FIELD_NAME.test(tag) || FORBIDDEN_NAMES.has(tag)) fail(`schema for ${name}: unsupported union`);
  const variants = branches.map((branch, i): UnionVariant => {
    const what = `${name}.oneOf[${i}]`;
    onlyKeys(branch, ["type", "additionalProperties", "required", "properties"], what);
    const props = object(branch.properties, `${what}.properties`);
    const pin = object(props[tag], `${what}.${tag}`);
    onlyKeys(pin, ["const"], `${what}.${tag}`);
    const variant = string(pin.const, `${what}.${tag}`);
    const req = Array.isArray(branch.required) ? branch.required : [];
    if (!FIELD_NAME.test(variant) || FORBIDDEN_NAMES.has(variant) || !req.includes(tag)) fail(`${what}: unsupported variant`);
    const inner = decodeObjectSchema(
      {
        type: branch.type,
        additionalProperties: branch.additionalProperties,
        required: req.filter((r) => r !== tag),
        properties: Object.fromEntries(Object.entries(props).filter(([k]) => k !== tag)),
      },
      `${name}.${variant}`,
    );
    if (inner.fields.some((f) => f.kind === "union")) fail(`${what}: a variant cannot hold a union`);
    return { name: variant, fields: inner.fields };
  });
  if (new Set(variants.map((v) => v.name)).size !== variants.length) fail(`schema for ${name}: duplicate variants`);
  const field: FieldSchema = { name, kind: "union", required, nullable, tag, variants };
  if (s.description !== undefined) field.description = string(s.description, `${name}.description`);
  return field;
}

function decodeField(name: string, raw: unknown, required: boolean): FieldSchema {
  if (!FIELD_NAME.test(name) || FORBIDDEN_NAMES.has(name)) fail(`unsupported field name ${JSON.stringify(name)}`);
  const s = object(raw, `schema for ${name}`);
  if (s.oneOf !== undefined) return decodeUnion(name, s, required);
  let nullable = false;
  let type = s.type;
  if (Array.isArray(type)) {
    if (type.length !== 2 || type[1] !== "null") fail(`schema for ${name}: unsupported type union`);
    nullable = true;
    type = type[0];
  }
  if (type === "array") {
    onlyKeys(s, ["type", "maxItems", "items", "description"], "array schema");
    const maxItems = integer(s.maxItems, "maxItems");
    if (maxItems < 0 || maxItems > 1000) fail("unbounded output items");
    return { name, kind: "items", required, nullable, maxItems, items: decodeObjectSchema(s.items, name + ".items") };
  }
  if (type === "object") return decodeMoney(name, s, required, nullable);
  onlyKeys(
    s,
    ["type", "minLength", "maxLength", "minimum", "maximum", "enum", "format", "pattern", "description"],
    `schema for ${name}`,
  );
  if (type !== "string" && type !== "integer" && type !== "boolean") {
    fail(`schema for ${name}: unsupported type ${JSON.stringify(type)}`);
  }
  const field: FieldSchema = { name, kind: type, required, nullable };
  if (s.description !== undefined) field.description = string(s.description, `${name}.description`);
  if (s.minLength !== undefined) field.minLength = integer(s.minLength, `${name}.minLength`);
  if (s.maxLength !== undefined) field.maxLength = integer(s.maxLength, `${name}.maxLength`);
  if (s.minimum !== undefined) field.minimum = integer(s.minimum, `${name}.minimum`);
  if (s.maximum !== undefined) field.maximum = integer(s.maximum, `${name}.maximum`);
  if (s.enum !== undefined) {
    if (!Array.isArray(s.enum)) fail(`${name}.enum must be an array`);
    field.choices = s.enum.filter((v) => v !== null).map((v) => string(v, `${name}.enum value`));
  }
  if (s.format !== undefined) {
    if (s.format !== "uuid" && s.format !== "date" && s.format !== "date-time" && s.format !== "duration") {
      fail(`${name}: unsupported format ${JSON.stringify(s.format)}`);
    }
    field.format = s.format;
  }
  if (s.pattern !== undefined) {
    // Arbitrary regexes stay server-side (§5.4). The portable patterns are
    // digits, decimals and the duration subset, exactly as the server writes them.
    const digits = typeof s.pattern === "string" ? decimalDigits(s.pattern) : undefined;
    if (s.pattern === DIGITS_PATTERN && field.format === undefined) field.format = "digits";
    else if (s.pattern === DURATION_PATTERN && field.format === "duration") {
      // The format says what it is; the pattern narrows it to the subset.
    } else if (digits && field.format === undefined) {
      field.format = "decimal";
      field.precision = digits.precision;
      field.scale = digits.scale;
    } else fail(`${name}: unsupported pattern`);
  }
  if (field.format === "duration" && s.pattern !== DURATION_PATTERN) fail(`${name}: duration needs its pattern`);
  const lengthOnString = field.minLength !== undefined || field.maxLength !== undefined || field.format !== undefined;
  if (lengthOnString && field.kind !== "string") fail(`${name}: string keywords on a ${field.kind} field`);
  if ((field.minimum !== undefined || field.maximum !== undefined) && field.kind !== "integer") {
    fail(`${name}: numeric bounds on a ${field.kind} field`);
  }
  return field;
}

export function decodeObjectSchema(raw: unknown, what: string): ObjectSchema {
  const s = object(raw, what);
  onlyKeys(s, ["$schema", "type", "additionalProperties", "required", "properties"], what);
  if (s.$schema !== undefined && s.$schema !== JSON_SCHEMA_2020_12) fail(`${what}: unsupported dialect`);
  if (s.type !== "object") fail(`${what}: root must be an object schema`);
  if (s.additionalProperties !== false) fail(`${what}: additionalProperties must be false`);
  const required = Array.isArray(s.required) ? s.required.map((r) => string(r, `${what}.required`)) : fail(`${what}.required must be an array`);
  const properties = object(s.properties, `${what}.properties`);
  const names = Object.keys(properties);
  if (names.length > LIMITS.contractFields) fail(`${what}: more than ${LIMITS.contractFields} fields`);
  for (const r of required) if (!names.includes(r)) fail(`${what}: required field ${r} is not a property`);
  return { fields: names.map((name) => decodeField(name, properties[name], required.includes(name))) };
}

/** No `$ref`-style keyword anywhere: schemas must be self-contained (§8.5). */
function assertNoReferences(value: unknown, path: string): void {
  if (Array.isArray(value)) {
    value.forEach((v, i) => assertNoReferences(v, `${path}/${i}`));
  } else if (typeof value === "object" && value !== null) {
    for (const [key, v] of Object.entries(value)) {
      if (key === "$ref" || key === "$dynamicRef" || key === "$id" || key === "$anchor") {
        fail(`external or internal references are not allowed (${path}/${key})`);
      }
      assertNoReferences(v, `${path}/${key}`);
    }
  }
}

/**
 * True when some segment of `path` (before any query) is a dot segment,
 * literal or percent-encoded in any case: `.`, `..`, `%2e`, `.%2E`, `%2e%2e`.
 * URL resolution removes those segments (WHATWG URL, "double-dot segment"),
 * so a path containing one can resolve outside the prefix it starts with.
 */
export function hasDotSegment(path: string): boolean {
  const end = path.search(/[?#]/);
  return (end < 0 ? path : path.slice(0, end))
    .split("/")
    .some((segment) => /^\.{1,2}$/.test(segment.replace(/%2e/gi, ".")));
}

/** Whether `pathname` lies under `prefix` on a segment boundary (`/api/` admits `/api/x`, never `/apix`). */
export function underPrefix(pathname: string, prefix: string): boolean {
  if (prefix.endsWith("/")) return pathname.startsWith(prefix);
  return pathname === prefix || pathname.startsWith(prefix + "/");
}

/** A binding is a path on the configured API origin, never a URL. */
export function checkBindingPath(path: string, apiPrefix: string): string {
  const escapes =
    !path.startsWith(apiPrefix) ||
    !underPrefix(path, apiPrefix) ||
    path.startsWith("//") ||
    path.includes("\\") ||
    path.includes("..") ||
    hasDotSegment(path) ||
    /[?#\s]/.test(path) ||
    /^[a-z][a-z0-9+.-]*:/i.test(path);
  if (escapes) fail(`binding path ${JSON.stringify(path)} escapes the API origin`);
  return path;
}

export function decodeDescriptor(raw: unknown, options: DecodeOptions = {}): Decoded {
  const apiPrefix = options.apiPrefix ?? "/api/";
  try {
    if (options.bytes !== undefined && options.bytes > LIMITS.descriptorBytes) {
      fail(`descriptor is larger than ${LIMITS.descriptorBytes} bytes`);
    }
    const d = object(raw, "descriptor");
    if (d.protocol !== PROTOCOL) fail(`not a ${PROTOCOL} descriptor`);
    const version = string(d.protocolVersion, "protocolVersion");
    if (Number(version.split(".")[0]) !== SUPPORTED_MAJOR) fail(`unsupported protocol version ${version}`);
    assertNoReferences(d, "");

    const requiredFeatures = Array.isArray(d.requiredFeatures)
      ? d.requiredFeatures.map((f) => string(f, "requiredFeatures[]"))
      : fail("requiredFeatures must be an array");

    const op = object(d.operation, "operation");
    const binding = object(op.binding, "operation.binding");
    const success = object(op.success, "operation.success");
    const body = binding.body;
    if (body !== "input" && body !== "none") fail("operation.binding.body must be input or none");
    const method = string(binding.method, "binding.method");
    if (!["GET", "POST", "PUT", "PATCH", "DELETE"].includes(method)) fail(`unsupported method ${method}`);

    const input = object(d.input, "input");
    if (input.validationProfile !== VALIDATION_PROFILE) fail(`unsupported validation profile`);
    const output = object(d.output, "output");
    const inputSchema = decodeObjectSchema(input.schema, "input.schema");

    const presentation = object(d.presentation, "presentation");
    if (presentation.kind !== "form") fail("presentation.kind must be form");
    const layout = object(presentation.layout, "presentation.layout");
    if (layout.kind === "vertical" && Array.isArray(layout.children) && layout.children.length > LIMITS.formFields) fail(`more than ${LIMITS.formFields} form fields`);
    const flattened: unknown[] = [];
    let nodes = 0;
    const nodeIds = new Set<string>();
    function layoutNode(raw: unknown, depth: number): FormLayout {
      if (++nodes > 64 || depth > 8) fail("layout exceeds bounds");
      const node = object(raw, "layout node");
      if (node.kind === "field") { flattened.push(node); return { kind: "field", pointer: string(node.pointer, "field pointer") }; }
      if (node.kind !== "vertical" && node.kind !== "section" && node.kind !== "columns") fail("unsupported layout node");
      if (node.kind !== "vertical" && !requiredFeatures.includes("forms.layout@1")) fail("layout requires feature");
      if (!Array.isArray(node.children) || (node.kind === "columns" && (node.children.length < 1 || node.children.length > 4))) fail("invalid layout children");
      const id = node.kind === "vertical" ? undefined : string(node.id, "layout id");
      if (id && nodeIds.has(id)) fail("duplicate layout id");
      if (id) nodeIds.add(id);
      return { kind: node.kind, id, label: node.kind === "section" ? string(node.label, "section label") : undefined, children: node.children.map((n) => layoutNode(n, depth + 1)) };
    }
    const decodedLayout = layoutNode(layout, 0);
    if (flattened.length > LIMITS.formFields) fail("too many fields");
    const fields = flattened.map((c, i): PresentedField => {
      const child = object(c, `layout.children[${i}]`);
      if (child.kind !== "field") fail(`layout.children[${i}]: unsupported kind`);
      const pointer = string(child.pointer, "pointer");
      const name = pointer.slice(1);
      if (!pointer.startsWith("/") || !inputSchema.fields.some((f) => f.name === name)) {
        fail(`presentation points at ${pointer}, which the input schema does not define`);
      }
      const field: PresentedField = {
        pointer,
        name,
        widget: string(child.widget, "widget"),
        label: string(child.label, "label"),
      };
      if (child.help !== undefined) field.help = string(child.help, "help");
      if (field.widget === "relation") {
        const options = object(child.options, `options for ${pointer}`);
        field.options = {
          operation: string(options.operation, "options.operation"),
          path: checkBindingPath(string(options.path, "options.path"), apiPrefix),
          value: string(options.value, "options.value"),
          label: string(options.label, "options.label"),
        };
        if (options.lookup !== undefined) {
          if (!requiredFeatures.includes("forms.relation-lookup@1")) fail("lookup requires feature");
          const lookup = object(options.lookup, "relation lookup");
          const resolvePath = checkBindingPath(string(lookup.resolvePath, "resolve path"), apiPrefix);
          if (!resolvePath.endsWith("/{id}")) fail("resolve path must bind id");
          const readBinding = (value: unknown): Descriptor => {
            try { return decodeInvocation(value, apiPrefix); }
            catch { return fail("invalid relation read binding"); }
          };
          const searchBinding = readBinding(lookup.searchBinding);
          const resolveBinding = readBinding(lookup.resolveBinding);
          if (searchBinding.operation.method !== "GET" || resolveBinding.operation.method !== "GET" || searchBinding.operation.path !== field.options.path || resolveBinding.operation.path !== resolvePath) fail("invalid relation read bindings");
          if (searchBinding.operation.id !== field.options.operation || resolveBinding.operation.id !== lookup.resolveOperation) fail("relation identity mismatch");
          field.options.lookup = { resolvePath, resolveOperation: string(lookup.resolveOperation, "resolve operation"), searchField: string(lookup.searchField, "search field"), searchBinding, resolveBinding };
        }
      }
      return field;
    });

    if (new Set(fields.map((f) => f.pointer)).size !== fields.length) fail("duplicate presented field");
    const descriptor: Descriptor = {
      interactionId: string(d.interactionId, "interactionId"),
      revision: string(d.revision, "revision"),
      audience: string(d.audience, "audience"),
      requiredFeatures,
      missingFeatures: requiredFeatures.filter((f) => !SUPPORTED_FEATURES.has(f)),
      operation: {
        id: string(op.id, "operation.id"),
        version: string(op.version, "operation.version"),
        method,
        path: checkBindingPath(string(binding.path, "binding.path"), apiPrefix),
        pathParams: Array.isArray(binding.pathParams)
          ? binding.pathParams.map((p) => string(p, "pathParams[]"))
          : [],
        body,
        revisionHeader: string(binding.interactionRevisionHeader, "interactionRevisionHeader"),
        successStatus: integer(success.status, "success.status"),
        successBody: success.body === "none" ? "none" : "output",
        errors: Array.isArray(op.errors) ? op.errors.map((e) => integer(e, "errors[]")) : [],
      },
      input: { id: string(input.id, "input.id"), version: string(input.version, "input.version"), schema: inputSchema },
      output: {
        id: string(output.id, "output.id"),
        version: string(output.version, "output.version"),
        schema: decodeObjectSchema(output.schema, "output.schema"),
      },
      presentation: {
        title: string(presentation.title, "presentation.title"),
        fields,
        layout: decodedLayout,
        submitLabel: string(object(presentation.submit, "presentation.submit").label, "submit.label"),
      },
    };
    if (descriptor.operation.revisionHeader?.toLowerCase() !== REVISION_HEADER.toLowerCase()) {
      fail("unexpected interaction revision header");
    }
    const templated = [...descriptor.operation.path.matchAll(/\{([a-z_]+)\}/g)].map((m) => m[1]);
    if (templated.join() !== descriptor.operation.pathParams.join()) fail("path parameters do not match the path");
    for (const name of descriptor.operation.pathParams) {
      if (!inputSchema.fields.some((f) => f.name === name)) fail(`path parameter ${name} is not an input field`);
    }
    if (binding.operationVersionHeader !== undefined) {
      if (binding.operationVersionHeader !== "Trestle-Operation-Version" || !requiredFeatures.includes("http.operation-version@1")) fail("invalid version pinning binding");
      descriptor.operation.operationVersionHeader = true;
    }
    const idempotency = object(op.idempotency, "operation.idempotency");
    if (idempotency.mode !== "none" && idempotency.mode !== "key") fail("unsupported idempotency mode");
    if (idempotency.mode === "key" && idempotency.header !== "Idempotency-Key") fail("unsupported idempotency header");
    descriptor.operation.idempotencyKey = idempotency.mode === "key";
    if (binding.precondition !== undefined) {
      const p = object(binding.precondition, "binding.precondition");
      if (p.header !== "If-Match") fail("unsupported precondition header");
      const field = string(p.field, "precondition.field");
      if (!inputSchema.fields.some((f) => f.name === field && f.format === "digits")) {
        fail("precondition field must be a digits input field");
      }
      descriptor.operation.precondition = { field, etagOf: string(p.etagOf, "precondition.etagOf") };
      if (p.target !== undefined) {
        if (!requiredFeatures.includes("http.precondition-target@1")) fail("precondition target requires its feature");
        const target = object(p.target, "precondition target");
        const path = checkBindingPath(string(target.path, "target path"), apiPrefix);
        const parameter = string(target.parameter, "target parameter"), inputField = string(target.inputField, "target input field");
        if (path.match(/\{[a-z_]+\}/g)?.join() !== "{" + parameter + "}" || !inputSchema.fields.some((f) => f.name === inputField)) fail("invalid precondition target mapping");
        descriptor.operation.precondition.target = { operation: string(target.operation, "target operation"), path, parameter, inputField };
      }
    }
    if (d.availability !== undefined) {
      const a = object(d.availability, "availability");
      descriptor.availability = { available: a.available === true };
      if (typeof a.reason === "string") descriptor.availability.reason = a.reason;
    }
    return { ok: true, descriptor };
  } catch (error) {
    if (error instanceof DecodeError) return { ok: false, error: error.message };
    throw error;
  }
}
