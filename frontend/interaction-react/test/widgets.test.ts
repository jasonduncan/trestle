// ADR-022 widgets: decimals stay strings; a local date-time becomes an
// instant in UTC and comes back in local time. ADR-027/028: money and unions.

import { describe, expect, it } from "vitest";

import { decodeObjectSchema, type FieldSchema } from "../src/protocol";
import { defaultWidgets } from "../src/widgets";
import { repoJson } from "./support";

const field = (format: FieldSchema["format"], extra: Partial<FieldSchema> = {}): FieldSchema => ({
  name: "x",
  kind: "string",
  required: false,
  nullable: false,
  format,
  ...extra,
});

describe("scalar widgets", () => {
  it("pick the widget by format", () => {
    expect(defaultWidgets.decimal?.accepts(field("decimal", { precision: 10, scale: 2 }))).toBe(true);
    expect(defaultWidgets.text?.accepts(field("decimal"))).toBe(false);
    expect(defaultWidgets.date?.accepts(field("date"))).toBe(true);
    expect(defaultWidgets.datetime?.accepts(field("date-time"))).toBe(true);
    expect(defaultWidgets.duration?.accepts(field("duration"))).toBe(true);
  });

  it("send decimals as the text typed, never a number", () => {
    const decimal = defaultWidgets.decimal!;
    expect(decimal.toJson(" 12.50 ", field("decimal"))).toBe("12.50");
    expect(decimal.toJson("", field("decimal"))).toBeUndefined();
    expect(decimal.toJson("", field("decimal", { nullable: true }))).toBeNull();
  });

  it("round-trip a local date-time through UTC", () => {
    const datetime = defaultWidgets.datetime!;
    const sent = datetime.toJson("2026-10-17T09:30:00", field("date-time")) as string;
    expect(sent).toBe(new Date(2026, 9, 17, 9, 30, 0).toISOString());
    expect(sent.endsWith("Z")).toBe(true);
    expect(datetime.fromJson(sent, field("date-time"))).toBe("2026-10-17T09:30:00");
    expect(datetime.toJson("not a time", field("date-time"))).toBe("not a time");
  });
});

describe("money widget", () => {
  const money = defaultWidgets.money!;
  const field = (extra: Partial<FieldSchema> = {}): FieldSchema => ({
    name: "price",
    kind: "money",
    required: true,
    nullable: false,
    precision: 10,
    scale: 2,
    ...extra,
  });

  it("sends the amount as typed and the currency beside it", () => {
    expect(money.accepts(field())).toBe(true);
    expect(money.toJson("12.50 USD", field())).toEqual({ amount: "12.50", currency: "USD" });
    expect(money.fromJson({ amount: "3", currency: "EUR" }, field())).toBe("3 EUR");
    expect(money.toJson(money.empty(field()), field())).toBeUndefined();
  });

  it("prefills a single allowed currency and treats it alone as empty", () => {
    const one = field({ choices: ["EUR"], nullable: true });
    expect(money.empty(one)).toBe(" EUR");
    expect(money.toJson(money.empty(one), one)).toBeNull();
    expect(money.toJson("5 EUR", one)).toEqual({ amount: "5", currency: "EUR" });
  });
});


describe("union widget", () => {
  const union = defaultWidgets.union!;
  const unions = repoJson("fixtures/contracts/unions.json");
  const input = decodeObjectSchema(unions.schema, "unions");
  const payment = input.fields.find((f) => f.name === "payment")!;
  const refund = input.fields.find((f) => f.name === "refund")!;
  const raw = (variant: string, members: Record<string, Record<string, string | boolean>> = {}) =>
    JSON.stringify({ variant, members });

  it("sends the tag and only the chosen variant's members", () => {
    expect(union.accepts(payment)).toBe(true);
    const typed = { card: { last4: "4242", brand: "" }, invoice: { due: "2026-11-01" } };
    expect(union.toJson(raw("card", typed), payment)).toEqual({ method: "card", last4: "4242" });
    expect(union.toJson(raw("invoice", typed), payment)).toEqual({ method: "invoice", due: "2026-11-01" });
    expect(union.toJson(raw("cash", typed), payment)).toEqual({ method: "cash" });
    expect(union.toJson(union.empty(payment), payment)).toBeUndefined();
    expect(union.toJson("not ours", payment)).toBeUndefined();
  });

  it("prefills a lone variant, sends members through their own widgets, and clears to null", () => {
    expect(JSON.parse(String(union.empty(refund))).variant).toBe("credit");
    const credit = raw("credit", { credit: { amount: "5.00 USD" } });
    expect(union.toJson(credit, refund)).toEqual({ kind: "credit", amount: { amount: "5.00", currency: "USD" } });
    expect(union.toJson(raw(""), refund)).toBeNull();
  });

  it("edits a record's value and reads back what it sends", () => {
    const value = { method: "card", last4: "4242", brand: "visa" };
    const loaded = union.fromJson(value, payment);
    expect(union.toJson(loaded, payment)).toEqual(value);
    expect(union.fromJson(value, payment)).toBe(loaded);
    expect(union.fromJson({ method: "cheque" }, payment)).toBe(union.empty(payment));
  });
});
