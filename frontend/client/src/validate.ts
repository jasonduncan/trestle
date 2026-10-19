// `trestle.portable@1` validation in the browser, driven entirely by the
// descriptor's input schema (no per-form code). It mirrors
// `trestle_protocol::ContractDoc::validate`: same codes, messages and
// 400/422 split. It is feedback only; the server always validates again.
//
// Agreement with the server is proven by running the shared fixtures in
// `fixtures/contracts/` through both (see test/validate.test.ts).

import type { FieldSchema, ObjectSchema } from "./protocol";
import { checkDecimal, isDuration, normalizeInstant, parseDate } from "./scalar";

export interface FieldError {
  pointer: string;
  code: string;
  message: string;
  params?: Record<string, unknown>;
}

export type Validation =
  | { ok: true; value: Record<string, unknown> }
  | { ok: false; status: 400 | 422; code: string; errors: FieldError[] };

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const DIGITS = /^[0-9]+$/;

export function pointer(name: string): string {
  return `/${name.replace(/~/g, "~0").replace(/\//g, "~1")}`;
}

/** Length in Unicode code points, as the server counts. */
export function codePoints(text: string): number {
  let count = 0;
  for (const _ of text) count++;
  return count;
}

function typeError(at: string, expected: string): FieldError {
  const article = /^[aeiou]/.test(expected) ? "an" : "a";
  return { pointer: at, code: "type", message: `Expected ${article} ${expected}.`, params: { expected } };
}

type Check = { ok: unknown } | { shape: FieldError } | { invalid: FieldError };

function check(field: FieldSchema, at: string, value: unknown): Check {
  switch (field.kind) {
    case "string": {
      if (typeof value !== "string") return { shape: typeError(at, "string") };
      if (field.choices) {
        return field.choices.includes(value)
          ? { ok: value }
          : {
              invalid: {
                pointer: at,
                code: "choice",
                message: `Choose one of: ${field.choices.join(", ")}.`,
                params: { choices: field.choices },
              },
            };
      }
      if (field.format === "uuid" && !UUID.test(value)) {
        return { invalid: { pointer: at, code: "format", message: "Enter a valid ID.", params: { format: "uuid" } } };
      }
      if (field.format === "digits" && !DIGITS.test(value)) {
        return { invalid: { pointer: at, code: "format", message: "Enter digits only.", params: { format: "digits" } } };
      }
      if (field.format === "decimal") {
        const precision = field.precision ?? 0;
        const scale = field.scale ?? 0;
        const error = checkDecimal(value, precision, scale);
        if (error === "syntax") {
          return { invalid: { pointer: at, code: "format", message: "Enter a number, like 12.50.", params: { format: "decimal" } } };
        }
        if (error === "digits") {
          const message = `Use at most ${precision - scale} digits before the decimal point and ${scale} after.`;
          return { invalid: { pointer: at, code: "decimal_digits", message, params: { precision, scale } } };
        }
      }
      if (field.format === "date" && parseDate(value) === undefined) {
        return { invalid: { pointer: at, code: "format", message: "Enter a date as YYYY-MM-DD.", params: { format: "date" } } };
      }
      if (field.format === "date-time" && normalizeInstant(value) === undefined) {
        const message = "Enter a date and time with a time zone, like 2026-10-17T09:30:00Z.";
        return { invalid: { pointer: at, code: "format", message, params: { format: "date-time" } } };
      }
      if (field.format === "duration" && !isDuration(value)) {
        const message = "Enter a duration like PT30M or P1DT2H.";
        return { invalid: { pointer: at, code: "format", message, params: { format: "duration" } } };
      }
      const count = codePoints(value);
      if (field.minLength !== undefined && count < field.minLength) {
        const min = field.minLength;
        const message = min === 1 ? "This field cannot be empty." : `Use at least ${min} characters.`;
        return { invalid: { pointer: at, code: "min_length", message, params: { min } } };
      }
      if (field.maxLength !== undefined && count > field.maxLength) {
        const max = field.maxLength;
        return {
          invalid: { pointer: at, code: "max_length", message: `Use ${max} characters or fewer.`, params: { max } },
        };
      }
      return { ok: value };
    }
    case "integer": {
      // JSON Schema integer: any number without a fractional part.
      if (typeof value !== "number" || !Number.isFinite(value) || !Number.isInteger(value)) {
        return { shape: typeError(at, "integer") };
      }
      if (field.minimum !== undefined && value < field.minimum) {
        const min = field.minimum;
        return { invalid: { pointer: at, code: "minimum", message: `Must be at least ${min}.`, params: { min } } };
      }
      if (field.maximum !== undefined && value > field.maximum) {
        const max = field.maximum;
        return { invalid: { pointer: at, code: "maximum", message: `Must be at most ${max}.`, params: { max } } };
      }
      return { ok: value };
    }
    case "boolean":
      return typeof value === "boolean" ? { ok: value } : { shape: typeError(at, "boolean") };
    case "items":
      return { shape: typeError(at, "array") };
    case "money":
    case "union":
      // Both report per member; see checkMoney and checkUnion.
      return { shape: typeError(at, "object") };
  }
}

/** Mirrors the server's `check_money`: each member at its own pointer. */
function checkMoney(field: FieldSchema, at: string, value: unknown, shape: FieldError[], invalid: FieldError[]): unknown {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    shape.push(typeError(at, "object"));
    return undefined;
  }
  const members = value as Record<string, unknown>;
  let ok = true;
  const member = (name: string, schema: FieldSchema) => {
    const here = `${at}/${name}`;
    if (!Object.hasOwn(members, name)) {
      invalid.push({ pointer: here, code: "required", message: "This field is required." });
      return false;
    }
    if (members[name] === null) {
      invalid.push({ pointer: here, code: "not_nullable", message: "This field cannot be null." });
      return false;
    }
    const result = check(schema, here, members[name]);
    if ("ok" in result) return true;
    if ("shape" in result) shape.push(result.shape);
    else invalid.push(result.invalid);
    return false;
  };
  const base = { name: "", required: true, nullable: false } as const;
  ok = member("amount", { ...base, kind: "string", format: "decimal", precision: field.precision, scale: field.scale }) && ok;
  if (field.choices) {
    ok = member("currency", { ...base, kind: "string", choices: field.choices }) && ok;
  } else if (typeof members.currency === "string" && !/^[A-Z]{3}$/.test(members.currency)) {
    ok = false;
    invalid.push({
      pointer: `${at}/currency`,
      code: "format",
      message: "Enter a three-letter currency code, like USD.",
      params: { format: "currency" },
    });
  } else {
    ok = member("currency", { ...base, kind: "string" }) && ok;
  }
  for (const key of Object.keys(members).filter((k) => k !== "amount" && k !== "currency").sort()) {
    ok = false;
    invalid.push({ pointer: `${at}${pointer(key)}`, code: "unknown_field", message: "This field is not accepted." });
  }
  return ok ? { amount: members.amount, currency: members.currency } : undefined;
}

/** Mirrors the server's `check_union`: the tag picks the variant's schema. */
function checkUnion(field: FieldSchema, at: string, value: unknown, shape: FieldError[], invalid: FieldError[]): unknown {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    shape.push(typeError(at, "object"));
    return undefined;
  }
  const members = value as Record<string, unknown>;
  const tag = field.tag ?? "";
  const here = `${at}${pointer(tag)}`;
  if (!Object.hasOwn(members, tag)) {
    invalid.push({ pointer: here, code: "required", message: "This field is required." });
    return undefined;
  }
  const name = members[tag];
  if (typeof name !== "string") {
    shape.push(typeError(here, "string"));
    return undefined;
  }
  const variants = field.variants ?? [];
  const variant = variants.find((v) => v.name === name);
  if (!variant) {
    const choices = variants.map((v) => v.name);
    invalid.push({ pointer: here, code: "choice", message: `Choose one of: ${choices.join(", ")}.`, params: { choices } });
    return undefined;
  }
  const rest = Object.fromEntries(Object.entries(members).filter(([k]) => k !== tag));
  const result = validateInstance({ fields: variant.fields }, rest);
  if (result.ok) return { ...result.value, [tag]: name };
  (result.status === 400 ? shape : invalid).push(...result.errors.map((e) => ({ ...e, pointer: `${at}${e.pointer}` })));
  return undefined;
}

export function validateInstance(schema: ObjectSchema, instance: unknown): Validation {
  if (typeof instance !== "object" || instance === null || Array.isArray(instance)) {
    return { ok: false, status: 400, code: "body_not_object", errors: [] };
  }
  const object = instance as Record<string, unknown>;
  const shape: FieldError[] = [];
  const invalid: FieldError[] = [];
  const value: Record<string, unknown> = {};

  for (const field of schema.fields) {
    const at = pointer(field.name);
    if (!Object.hasOwn(object, field.name)) {
      if (field.required) invalid.push({ pointer: at, code: "required", message: "This field is required." });
      continue;
    }
    const present = object[field.name];
    if (present === null) {
      if (field.nullable) value[field.name] = null;
      else invalid.push({ pointer: at, code: "not_nullable", message: "This field cannot be null." });
      continue;
    }
    if (field.kind === "items") {
      if (!Array.isArray(present) || !field.items || present.length > (field.maxItems ?? 0)) {
        shape.push(typeError(at, "bounded array")); continue;
      }
      const items: unknown[] = [];
      present.forEach((item, index) => {
        const checked = validateInstance(field.items!, item);
        if (checked.ok) items.push(checked.value);
        else (checked.status === 400 ? shape : invalid).push(...checked.errors.map((error) => ({ ...error, pointer: at + "/" + index + error.pointer })));
      });
      value[field.name] = items;
      continue;
    }
    if (field.kind === "money" || field.kind === "union") {
      const checked = (field.kind === "money" ? checkMoney : checkUnion)(field, at, present, shape, invalid);
      if (checked !== undefined) value[field.name] = checked;
      continue;
    }
    const result = check(field, at, present);
    if ("ok" in result) value[field.name] = result.ok;
    else if ("shape" in result) shape.push(result.shape);
    else invalid.push(result.invalid);
  }

  if (shape.length > 0) return { ok: false, status: 400, code: "invalid_body_shape", errors: shape };
  const unknown = Object.keys(object)
    .filter((key) => !schema.fields.some((f) => f.name === key))
    .sort();
  for (const key of unknown) {
    invalid.push({ pointer: pointer(key), code: "unknown_field", message: "This field is not accepted." });
  }
  if (invalid.length > 0) return { ok: false, status: 422, code: "validation_failed", errors: invalid };
  return { ok: true, value };
}
