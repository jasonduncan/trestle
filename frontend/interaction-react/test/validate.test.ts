// The browser validator and the Rust server agree on every shared fixture,
// using the schema the server actually publishes (no hand-written rules).

import { describe, expect, it } from "vitest";

import { decodeDescriptor } from "../src/protocol";
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
