import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { currentAvailability, decodeView, decodeViewManifest, formatValue } from "../src/views";
import { decodeDescriptor } from "../src/protocol";

const fixture = (name: string) => JSON.parse(readFileSync(fileURLToPath(new URL("../../../fixtures/views/" + name + ".json", import.meta.url)), "utf8"));
describe("views from Rust registry assembly", () => {
  it("decodes complete collection and record documents for both applications", () => {
    for (const name of ["support", "maintenance"]) {
      const f = fixture(name);
      for (const audience of ["admin", "product"]) for (const doc of f[audience].views) {
        const view = decodeView(doc);
        expect(view.missingFeatures).toEqual([]);
        expect(view.viewId).toBe(doc.viewId);
      }
      for (const forms of [f.adminForms, f.productForms]) for (const form of forms.interactions) expect(decodeDescriptor(form).ok).toBe(true);
    }
  });
  it("shows enum values by their labels (ADR-035)", () => {
    const state = { name: "state", kind: "string" as const, required: true, nullable: false, choices: ["open", "closed"], labels: { closed: "Done" } };
    expect([formatValue("closed", state), formatValue("open", state), formatValue("closed")]).toEqual(["Done", "open", "closed"]);
  });
  it("rejects invalid pointers, actions, paths, references and oversized metadata before invocation", () => {
    const original = fixture("support").admin.views[0];
    for (const mutate of [
      (d: any) => { d.columns[0].pointer = "/hidden"; },
      (d: any) => { d.source.invocation.path = "//evil.example/api/"; },
      (d: any) => { d.source.invocation.method = "POST"; },
      (d: any) => { d.source.item.schema.$ref = "https://evil.example/schema"; },
      (d: any) => { d.filters = ["sql"]; },
      (d: any) => { d.title = "x".repeat(256 * 1024); },
      (d: any) => { d.actions[0].interactionRevision = null; },
      (d: any) => { d.actions[0].invocation.body = "raw"; },
    ]) { const changed = structuredClone(original); mutate(changed); expect(() => decodeView(changed)).toThrow(); }
  });
  it("decodes a completeness flag only as a required boolean of a collection (ADR-045)", () => {
    const recent = fixture("support").product.views.find((v: any) => v.viewId === "support_work.recent");
    expect(decodeView(recent).source.completePointer).toBe("/complete");
    expect(decodeView(recent).missingFeatures).toEqual([]);
    for (const mutate of [
      (d: any) => { d.source.completePointer = "/items"; },
      (d: any) => { d.source.completePointer = "/hidden"; },
      (d: any) => { d.source.invocation.output.schema.required = ["items"]; },
    ]) { const changed = structuredClone(recent); mutate(changed); expect(() => decodeView(changed)).toThrow(); }
    const record = structuredClone(fixture("support").product.views.find((v: any) => v.kind === "record"));
    record.source.completePointer = "/id";
    expect(() => decodeView(record)).toThrow();
  });
  it("blocks new required features while retaining an inspectable view", () => {
    const doc = fixture("support").admin.views[0]; doc.requiredFeatures.push("views.future@9");
    expect(decodeView(doc).missingFeatures).toEqual(["views.future@9"]);
  });
  it("returns a decode refusal for malformed relation bindings", () => {
    const forms = fixture("support").adminForms.interactions;
    const form = forms.find((f: any) => JSON.stringify(f).includes("searchBinding"));
    expect(form).toBeDefined();
    const corrupt = (node: any) => {
      if (node && typeof node === "object") {
        if (node.searchBinding) node.searchBinding.method = "INVALID";
        Object.values(node).forEach(corrupt);
      }
    };
    corrupt(form);
    expect(decodeDescriptor(form).ok).toBe(false);
  });
  it("validates manifest destinations and rejects prototype keys", () => {
    const entry = { viewId: "support.case.queue", revision: "r-test", audience: "product", kind: "collection", title: "Cases", url: "/_meta/v1/ui/views/support.case.queue" };
    const doc = { protocol: "trestle.views", protocolVersion: "1.0", views: [entry] };
    expect(decodeViewManifest(doc)).toHaveLength(1);
    entry.url = "https://evil.example/meta";
    expect(() => decodeViewManifest(doc)).toThrow();
    expect(() => decodeView(JSON.parse('{"__proto__":{}}'))).toThrow();
  });
  it("discards availability for another resource, record or observed version", () => {
    const view = decodeView(fixture("support").product.views.find((v: any) => v.viewId === "support_work.case"));
    const hints = { resource: "support.case", record_key: "case-1", etag: '"support.case:3"', actions: [{ id: "support.assign.form", available: true, reason: null }] };
    expect(currentAvailability(hints, view, { id: "case-1" }, hints.etag)?.["support.assign.form"]?.available).toBe(true);
    for (const mutation of [{ resource: "other.case" }, { record_key: "case-2" }, { etag: '"support.case:4"' }]) {
      expect(currentAvailability({ ...hints, ...mutation }, view, { id: "case-1" }, hints.etag)).toBeUndefined();
    }
  });
});
