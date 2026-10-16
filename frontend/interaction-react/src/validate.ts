// `trestle.portable@1` validation in the browser, driven entirely by the
// descriptor's input schema (no per-form code). It mirrors
// `trestle_protocol::ContractDoc::validate`: same codes, messages and
// 400/422 split. It is feedback only; the server always validates again.
//
// Agreement with the server is proven by running the shared fixtures in
// `fixtures/contracts/` through both (see test/validate.test.ts).

import type { FieldSchema, ObjectSchema } from "./protocol";

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
  }
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
