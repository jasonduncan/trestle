// ADR-022 widgets: decimals stay strings; a local date-time becomes an
// instant in UTC and comes back in local time.

import { describe, expect, it } from "vitest";

import type { FieldSchema } from "../src/protocol";
import { defaultWidgets } from "../src/widgets";

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
