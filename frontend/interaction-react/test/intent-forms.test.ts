// ADR-026: a form generated from `trestle.toml` (decimal, ref, enum,
// instant and long text fields) is one this renderer can render and
// submit: it decodes, needs no feature the renderer lacks, and every field
// has a local widget that accepts it. The descriptor is the fixture the
// Rust test regenerates from the intent.

import { describe, expect, it } from "vitest";

import { decodeDescriptor } from "../src/protocol";
import { defaultWidgets } from "../src/widgets";
import { repoJson } from "./support";

describe("generated intent forms", () => {
  const raw = repoJson("fixtures/interactions/intent-crm-deal-create.json");

  it("decode with every feature and widget supported", () => {
    const decoded = decodeDescriptor(raw);
    if (!decoded.ok) throw new Error(decoded.error);
    const { descriptor } = decoded;
    expect(descriptor.missingFeatures).toEqual([]);
    const byName = Object.fromEntries(descriptor.input.schema.fields.map((f) => [f.name, f]));
    for (const presented of descriptor.presentation.fields) {
      const widget = defaultWidgets[presented.widget];
      expect(widget, presented.widget).toBeDefined();
      const field = byName[presented.name];
      expect(field, presented.name).toBeDefined();
      expect(widget!.accepts(field!), `${presented.widget} for ${presented.name}`).toBe(true);
    }
    expect(descriptor.presentation.fields.map((f) => f.widget)).toEqual(["text", "decimal", "relation", "select", "datetime", "textarea"]);
    expect(descriptor.presentation.fields[2]?.options?.path).toBe("/api/v1/accounts");
  });
});
