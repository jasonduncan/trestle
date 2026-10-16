// Descriptor decoding treats metadata as untrusted (design doc §8.5, B.2).

import { describe, expect, it } from "vitest";

import { decodeDescriptor, LIMITS } from "../src/protocol";
import { publishedDescriptor } from "./support";

function decodes(mutate: (d: any) => void = () => {}) {
  const d = publishedDescriptor();
  mutate(d);
  return decodeDescriptor(d);
}

function rejects(mutate: (d: any) => void, message: RegExp) {
  const result = decodes(mutate);
  expect(result.ok).toBe(false);
  if (!result.ok) expect(result.error).toMatch(message);
}

describe("decodeDescriptor", () => {
  it("decodes the published descriptor", () => {
    const result = decodes();
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    const d = result.descriptor;
    expect(d.operation.path).toBe("/api/v1/lessons");
    expect(d.presentation.fields.map((f) => f.name).slice(0, 2)).toEqual(["title", "grade_level"]);
    expect(d.input.schema.fields.find((f) => f.name === "title")?.required).toBe(true);
    expect(d.missingFeatures).toEqual([]);
  });

  it("rejects other protocols and unknown major versions", () => {
    rejects((d) => (d.protocol = "other"), /not a trestle.interactions/);
    rejects((d) => (d.protocolVersion = "2.0"), /unsupported protocol version/);
  });

  it("accepts newer minor versions but reports unknown required features", () => {
    const result = decodes((d) => {
      d.protocolVersion = "1.4";
      d.requiredFeatures.push("forms.wizard@1");
    });
    expect(result.ok && result.descriptor.missingFeatures).toEqual(["forms.wizard@1"]);
  });

  it("rejects bindings that escape the API origin", () => {
    for (const path of ["https://evil.example/api/v1/x", "//evil.example/api", "/_meta/v1/x", "/api/../admin", "/api/v1/x?y=1"]) {
      rejects((d) => (d.operation.binding.path = path), /escapes the API origin/);
    }
  });

  it("rejects references and unsupported schema keywords", () => {
    rejects((d) => (d.input.schema.properties.title = { $ref: "https://evil.example/s.json" }), /references are not allowed/);
    rejects((d) => (d.input.schema.properties.title.pattern = "^(a+)+$"), /unsupported pattern/);
    rejects((d) => (d.input.schema.properties.title.oneOf = []), /unsupported keyword "oneOf"/);
    rejects((d) => (d.input.schema.additionalProperties = true), /additionalProperties must be false/);
  });

  it("rejects prototype-polluting field names arriving as JSON", () => {
    // JSON.parse creates an own "__proto__" key, exactly as a hostile
    // response would; assigning obj["__proto__"] in JS would not.
    const text = JSON.stringify(publishedDescriptor()).replace(
      '"properties":{',
      '"properties":{"__proto__":{"type":"string"},',
    );
    const hostile = JSON.parse(text);
    expect(Object.keys(hostile.input.schema.properties)).toContain("__proto__");
    const result = decodeDescriptor(hostile);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/unsupported field name "__proto__"/);
  });

  it("enforces size and field-count limits", () => {
    expect(decodeDescriptor(publishedDescriptor(), { bytes: LIMITS.descriptorBytes + 1 }).ok).toBe(false);
    rejects((d) => {
      d.presentation.layout.children = Array.from({ length: LIMITS.formFields + 1 }, () => d.presentation.layout.children[0]);
    }, /more than 64 form fields/);
  });

  it("rejects presentation pointing at fields the input schema does not define", () => {
    rejects((d) => d.presentation.layout.children.push({ kind: "field", pointer: "/school_id", widget: "text", label: "School" }), /does not define/);
  });
});
