// ADR-029: list cells and action buttons for any described resource.

import { describe, expect, it } from "vitest";

import { actionRequest, cell } from "../src/rows";

describe("admin rows", () => {
  it("shows money as an amount and its currency, and nothing as a dash", () => {
    expect(cell({ amount: "12.50", currency: "USD" })).toBe("12.50 USD");
    expect(cell(null)).toBe("—");
    expect(cell(3)).toBe("3");
    expect(cell({ method: "cash" })).toBe('{"method":"cash"}');
  });

  it("sends the version in the body, or as the ETag the server issued", () => {
    const row = { id: "d1", version: 4 };
    expect(actionRequest(row, { path: "/x/{id}", bind: { expected_version: "/version" } })).toEqual({
      body: { expected_version: 4 },
    });
    expect(actionRequest(row, { path: "/x/{id}", bind: {}, ifMatch: { pointer: "/version", etagOf: "crm.deal" } })).toEqual({
      body: {},
      headers: { "If-Match": '"crm.deal:4"' },
    });
  });
});
