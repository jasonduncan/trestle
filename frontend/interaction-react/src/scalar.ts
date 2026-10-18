// Portable scalar formats (ADR-022), mirroring `trestle_protocol::scalar`
// line for line: the browser must accept and reject exactly what the server
// does. `fixtures/contracts/scalars.json` proves it (test/validate.test.ts).

import { DURATION_PATTERN } from "./protocol";

export type DecimalError = "syntax" | "digits";

/** `-?(0|[1-9][0-9]*)(\.[0-9]+)?` within `precision` digits, `scale` after the point. */
export function checkDecimal(text: string, precision: number, scale: number): DecimalError | undefined {
  const unsigned = text.startsWith("-") ? text.slice(1) : text;
  const point = unsigned.indexOf(".");
  const whole = point === -1 ? unsigned : unsigned.slice(0, point);
  const fraction = point === -1 ? undefined : unsigned.slice(point + 1);
  const digits = (s: string) => /^[0-9]+$/.test(s);
  if (!digits(whole) || (whole.length > 1 && whole.startsWith("0"))) return "syntax";
  if (fraction !== undefined && !digits(fraction)) return "syntax";
  const wholeDigits = whole === "0" ? 0 : whole.length;
  const fractionDigits = fraction?.length ?? 0;
  if (wholeDigits > precision - scale || fractionDigits > scale) return "digits";
  return undefined;
}

const leap = (year: number) => (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;

function daysInMonth(year: number, month: number): number {
  if (month === 2) return leap(year) ? 29 : 28;
  return [4, 6, 9, 11].includes(month) ? 30 : 31;
}

/** An RFC 3339 `full-date`, years 0000–9999. */
export function parseDate(text: string): [number, number, number] | undefined {
  const m = /^([0-9]{4})-([0-9]{2})-([0-9]{2})$/.exec(text);
  if (!m) return undefined;
  const [year, month, day] = [Number(m[1]), Number(m[2]), Number(m[3])];
  if (month < 1 || month > 12 || day < 1 || day > daysInMonth(year, month)) return undefined;
  return [year, month, day];
}

const floorDiv = (a: number, b: number) => Math.floor(a / b);

function daysFromCivil(year: number, month: number, day: number): number {
  const y = month <= 2 ? year - 1 : year;
  const era = floorDiv(y, 400);
  const yoe = y - era * 400;
  const doy = floorDiv(153 * (month > 2 ? month - 3 : month + 9) + 2, 5) + day - 1;
  const doe = yoe * 365 + floorDiv(yoe, 4) - floorDiv(yoe, 100) + doy;
  return era * 146097 + doe - 719468;
}

function civilFromDays(days: number): [number, number, number] {
  const z = days + 719468;
  const era = floorDiv(z, 146097);
  const doe = z - era * 146097;
  const yoe = floorDiv(doe - floorDiv(doe, 1460) + floorDiv(doe, 36524) - floorDiv(doe, 146096), 365);
  const doy = doe - (365 * yoe + floorDiv(yoe, 4) - floorDiv(yoe, 100));
  const mp = floorDiv(5 * doy + 2, 153);
  const day = doy - floorDiv(153 * mp + 2, 5) + 1;
  const month = mp < 10 ? mp + 3 : mp - 9;
  return [yoe + era * 400 + (month <= 2 ? 1 : 0), month, day];
}

const pad = (n: number, width: number) => String(n).padStart(width, "0");

/** An RFC 3339 `date-time` with an offset, in UTC (`…Z`); `undefined` if invalid. */
export function normalizeInstant(text: string): string | undefined {
  const m = /^([0-9]{4}-[0-9]{2}-[0-9]{2})[Tt]([0-9]{2}):([0-9]{2}):([0-9]{2})(\.[0-9]+)?([Zz]|[+-][0-9]{2}:[0-9]{2})$/.exec(text);
  if (!m) return undefined;
  const [, day = "", h, mi, sec, fraction = "", zone = ""] = m;
  const date = parseDate(day);
  const [hour, minute, second] = [Number(h), Number(mi), Number(sec)];
  if (!date || hour > 23 || minute > 59 || second > 59) return undefined;
  let offset = 0;
  if (zone !== "Z" && zone !== "z") {
    const hours = Number(zone.slice(1, 3));
    const minutes = Number(zone.slice(4, 6));
    if (hours > 23 || minutes > 59) return undefined;
    offset = (zone.startsWith("-") ? -1 : 1) * (hours * 60 + minutes);
  }
  const utc = daysFromCivil(...date) * 86400 + hour * 3600 + minute * 60 + second - offset * 60;
  const [y, mo, d] = civilFromDays(floorDiv(utc, 86400));
  if (y < 0 || y > 9999) return undefined;
  const s = utc - floorDiv(utc, 86400) * 86400;
  return `${pad(y, 4)}-${pad(mo, 2)}-${pad(d, 2)}T${pad(floorDiv(s, 3600), 2)}:${pad(floorDiv(s % 3600, 60), 2)}:${pad(s % 60, 2)}${fraction}Z`;
}

const DURATION = new RegExp(DURATION_PATTERN);

/** ISO 8601 `P[nD][T[nH][nM][nS]]`, the server's subset. */
export function isDuration(text: string): boolean {
  return DURATION.test(text);
}
