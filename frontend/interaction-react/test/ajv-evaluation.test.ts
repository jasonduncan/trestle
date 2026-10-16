// ADR-007 evidence: would JSON Forms' validator (AJV, JSON Schema 2020-12)
// accept and reject the same fixtures as the server? This measures the
// candidate; it is not used at runtime.

import Ajv2020 from "ajv/dist/2020";
import { describe, expect, it } from "vitest";

import { publishedDescriptor, repoJson } from "./support";

const fixtures = repoJson("fixtures/contracts/lessons.create.input.json");

describe("AJV as an alternative validator", () => {
  const ajv = new Ajv2020({
    allErrors: true,
    strict: true,
    formats: { uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i },
  });
  const validate = ajv.compile(publishedDescriptor().input.schema);

  it("agrees with the server on accept/reject for every fixture", () => {
    const disagreements = fixtures.cases
      .filter((c: any) => validate(c.input) !== (c.expect.status === 201))
      .map((c: any) => c.name);
    expect(disagreements).toEqual([]);
  });

  it("cannot reproduce the server's 400/422 split or stable codes", () => {
    const shape = fixtures.cases.find((c: any) => c.name === "grade as string");
    validate(shape.input);
    // AJV reports keyword "type"; the 400-vs-422 decision and Trestle codes
    // such as not_nullable or unknown_field must still be mapped by hand.
    expect(validate.errors?.[0]?.keyword).toBe("type");
    const unknown = fixtures.cases.find((c: any) => c.name === "ownership fields are unknown");
    validate(unknown.input);
    expect(validate.errors?.map((e) => e.keyword)).toEqual(["additionalProperties", "additionalProperties"]);
  });
});
