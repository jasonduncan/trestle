// Pure helpers for list rows: what a cell shows and what an action button
// sends. The server decides everything else.

export type Row = Record<string, unknown> & { id: string };

export interface RowAction {
  path: string;
  bind: Record<string, string>;
  /** Send `If-Match: "<etagOf>:<row value at pointer>"`, the ETag the server issues. */
  ifMatch?: { pointer: string; etagOf: string };
}

/** Value at a JSON Pointer such as `/version`. */
export function at(row: Row, pointer: string): unknown {
  return pointer
    .split("/")
    .slice(1)
    .reduce<unknown>((value, key) => (value && typeof value === "object" ? (value as Record<string, unknown>)[key] : undefined), row);
}

/** A list cell: money as "12.50 USD" (ADR-027), other objects as JSON. */
export function cell(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value !== "object") return String(value);
  const money = value as { amount?: unknown; currency?: unknown };
  if (typeof money.amount === "string" && typeof money.currency === "string") return `${money.amount} ${money.currency}`;
  return JSON.stringify(value);
}

/** The body and headers an action button sends for `row`. */
export function actionRequest(row: Row, action: RowAction): { body: Record<string, unknown>; headers?: Record<string, string> } {
  const body = Object.fromEntries(Object.entries(action.bind).map(([field, pointer]) => [field, at(row, pointer)]));
  if (!action.ifMatch) return { body };
  return { body, headers: { "If-Match": `"${action.ifMatch.etagOf}:${String(at(row, action.ifMatch.pointer))}"` } };
}
