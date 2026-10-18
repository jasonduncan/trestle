// The browser validator and the Rust server agree on every shared fixture,
// using the schema the server actually publishes (no hand-written rules).

import { describe, expect, it } from "vitest";

import { decimalPattern, decodeDescriptor, decodeObjectSchema, DURATION_PATTERN } from "../src/protocol";
import { codePoints, validateInstance } from "../src/validate";
import { publishedDescriptor, repoJson } from "./support";

const fixtures = repoJson("fixtures/contracts/lessons.create.input.json");

function schema() {
  const decoded = decodeDescriptor(publishedDescriptor());
  if (!decoded.ok) throw new Error(decoded.error);
  expect(decoded.descriptor.input.id).toBe(fixtures.contract);
  expect(decoded.descriptor.input.version).toBe(fixtures.version);
  return decoded.descriptor.input.schema;
}

describe("trestle.portable@1 in the browser", () => {
  const input = schema();

  for (const testCase of fixtures.cases) {
    it(`agrees with the server: ${testCase.name}`, () => {
      const result = validateInstance(input, testCase.input);
      if (testCase.expect.status === 201) {
        expect(result.ok).toBe(true);
        return;
      }
      expect(result.ok).toBe(false);
      if (result.ok) return;
      expect(result.status).toBe(testCase.expect.status);
      expect(result.code).toBe(testCase.expect.code);
      // Order is presentation's job; the set of (pointer, code) must match.
      const pairs = result.errors.map((e) => [e.pointer, e.code]).sort();
      expect(pairs).toEqual([...(testCase.expect.errors ?? [])].sort());
    });
  }

  it("counts code points like the server", () => {
    expect(codePoints("📐📐")).toBe(2);
    expect("📐📐".length).toBe(4);
    expect(codePoints("é")).toBe(2);
  });
});

// ADR-022: decimals, dates, instants and durations, from the same fixture
// the Rust test checks against the server and an independent validator.
const scalars = repoJson("fixtures/contracts/scalars.json");

describe("portable scalar types in the browser", () => {
  const input = decodeObjectSchema(scalars.schema, "scalars");

  it("decodes each type from the schema the server exports", () => {
    const byName = Object.fromEntries(input.fields.map((f) => [f.name, f]));
    expect(byName.price).toMatchObject({ format: "decimal", precision: 10, scale: 2, required: true });
    expect(byName.rate).toMatchObject({ format: "decimal", precision: 3, scale: 3 });
    expect(byName.due?.format).toBe("date");
    expect(byName.at).toMatchObject({ format: "date-time", nullable: true });
    expect(byName.length?.format).toBe("duration");
  });

  for (const testCase of scalars.cases) {
    it(`agrees with the server: ${testCase.name}`, () => {
      const result = validateInstance(input, testCase.input);
      if (testCase.expect.status === 201) {
        expect(result.ok).toBe(true);
        return;
      }
      expect(result.ok).toBe(false);
      if (result.ok) return;
      expect(result.status).toBe(testCase.expect.status);
      expect(result.code).toBe(testCase.expect.code);
      const pairs = result.errors.map((e) => [e.pointer, e.code]).sort();
      expect(pairs).toEqual([...(testCase.expect.errors ?? [])].sort());
    });
  }

  it("accepts only the patterns the server writes", () => {
    expect(decimalPattern(10, 2)).toBe(scalars.schema.properties.price.pattern);
    expect(DURATION_PATTERN).toBe(scalars.schema.properties.length.pattern);
    const decode = (pattern: string, format?: string) =>
      decodeObjectSchema(
        { type: "object", additionalProperties: false, required: [], properties: { x: { type: "string", pattern, ...(format ? { format } : {}) } } },
        "probe",
      );
    expect(() => decode("^-?(0|[1-9][0-9]{0,7})(\\.[0-9]{1,3})?$")).not.toThrow();
    expect(() => decode("^[a-z]+$")).toThrow(/unsupported pattern/);
    expect(() => decode("^-?(0|[1-9][0-9]{0,7})(\\.[0-9]{1,2})$")).toThrow(/unsupported pattern/);
    expect(() => decode("^P[0-9]+D$", "duration")).toThrow();
  });
});


// ADR-027: money and references, from the fixture the Rust test checks
// against the server and an independent validator.
const composites = repoJson("fixtures/contracts/composites.json");

describe("money and references in the browser", () => {
  const input = decodeObjectSchema(composites.schema, "composites");

  it("decodes money from its exact shape, and a reference as a uuid", () => {
    const byName = Object.fromEntries(input.fields.map((f) => [f.name, f]));
    expect(byName.price).toMatchObject({ kind: "money", precision: 10, scale: 2, required: true });
    expect(byName.price?.choices).toBeUndefined();
    expect(byName.fee).toMatchObject({ kind: "money", precision: 6, scale: 2, nullable: true, choices: ["EUR", "USD"] });
    expect(byName.account).toMatchObject({ kind: "string", format: "uuid" });
  });

  for (const testCase of composites.cases) {
    it(`agrees with the server: ${testCase.name}`, () => {
      const result = validateInstance(input, testCase.input);
      if (testCase.expect.status === 201) {
        expect(result.ok).toBe(true);
        return;
      }
      expect(result.ok).toBe(false);
      if (result.ok) return;
      expect(result.status).toBe(testCase.expect.status);
      expect(result.code).toBe(testCase.expect.code);
      const pairs = result.errors.map((e) => [e.pointer, e.code]).sort();
      expect(pairs).toEqual([...(testCase.expect.errors ?? [])].sort());
    });
  }

  it("refuses objects that are not money", () => {
    const decode = (schema: unknown) =>
      decodeObjectSchema({ type: "object", additionalProperties: false, required: [], properties: { x: schema } }, "probe");
    const money = structuredClone(composites.schema.properties.price);
    expect(() => decode(money)).not.toThrow();
    expect(() => decode({ ...money, additionalProperties: true })).toThrow(/unsupported object/);
    expect(() => decode({ ...money, required: ["amount"] })).toThrow(/unsupported object/);
    const extra = structuredClone(money);
    extra.properties.note = { type: "string" };
    expect(() => decode(extra)).toThrow(/unsupported object/);
    const loose = structuredClone(money);
    loose.properties.currency = { type: "string", pattern: "^[a-z]+$" };
    expect(() => decode(loose)).toThrow(/unsupported currency/);
  });
});
