# ADR-022: Decimal, date, instant and duration in contracts

**Status:** Proposed (2026-10-17) · **Design doc:** §5.2, §5.4, §11.3 ·
**Amends:** ADR-002, ADR-004

## Context

`trestle.portable@1` had text, integer, boolean, uuid, digits, enum and
items. Ordinary business data needs more:

- **Money amounts and rates are decimals.** As JSON numbers they pass
  through IEEE doubles in every JavaScript client, which can round them.
- **Dates and instants need one unambiguous syntax and time zone.**
- **Durations need a form whose length doesn't vary.**

§5.4 requires that a value means the same thing in Rust, JSON Schema and
the browser, proven by shared fixtures. AGENTS.md requires a new type to
ship with:

- the server evaluator;
- the schema export;
- the TypeScript decoder and validator;
- fixtures.

## Decision

Four new `FieldType`s, each a JSON **string** with one fixed syntax:

| Type | Example | Syntax | JSON Schema |
|---|---|---|---|
| `Decimal { precision, scale }` | `"-12.50"` | `-?(0\|[1-9][0-9]*)(\.[0-9]+)?`; at most `precision − scale` digits before the point and `scale` after. No exponent, `+` or leading zeros | `pattern` generated from precision and scale |
| `Date` | `"2026-10-17"` | RFC 3339 `full-date`, a real calendar day | `format: date` |
| `Instant` | `"2026-10-17T07:30:00Z"` | RFC 3339 `date-time`; offset required; `T`/`Z` in either case | `format: date-time` |
| `Duration` | `"P1DT2H30M"` | ISO 8601 `P[nD][T[nH][nM][nS]]`, at least one part; no years, months or weeks, whose length varies | `format: duration` plus the subset `pattern` |

- **Decimals are never numbers.** A JSON number for a decimal field is a
  400 (`type`), so no client sends a rounded value by accident.
- **Instants are normalized to UTC (`…Z`)** as they validate. Fractional
  digits are kept, and PostgreSQL rounds beyond microseconds when storing.
  Other types are returned as given.
- **Codes:**
  - `format` for bad syntax (`params.format` names the type);
  - `decimal_digits` for too many digits (`params: {precision, scale}`).

  Messages are fixed strings, as for other types.
- **Builders:** `Field::decimal(name, precision, scale)`, `Field::date`,
  `Field::instant` and `Field::duration`.
  - Definition problems: precision must be 1..=38 and scale at most
    precision.
  - The IR records `precision` and `scale`.
- **Compatibility.** A decimal that keeps fewer digits on either side
  "accepts fewer decimal digits", so it rejects older instances (§11.3).
- **Portable patterns only.** The browser still refuses arbitrary regexes.
  It recognizes the digits pattern, a decimal pattern only if regenerating
  it from the precision and scale it parsed gives the same string, and the
  duration pattern only beside `format: duration`.
- **Forms.**
  - New widgets: `decimal` (text, `inputmode=decimal`), `date`, `datetime`
    (`datetime-local`, sent as UTC) and `duration`.
  - A form using them requires `forms.decimal@1` or `forms.temporal@1`.
  - A renderer without the feature refuses to submit (I11).
  - `FormPresentation::features` is derived from widgets, sorted and
    unique.
- **SDK.** The four types are `string` in the generated TypeScript.
- **Not in this change:** database columns, `HistoricalTx` and
  `trestle.toml` types (U8b), and money, references and tagged unions
  (U8c). A typed operation binds these fields to `String` until then.

## Evidence

`fixtures/contracts/scalars.json` has 23 cases: valid values, offsets
normalized across midnight and a leap day, nulls, numbers sent for
decimals, exponents, leading zeros, digit limits including
`precision == scale`, impossible dates, missing offsets, hour 24, `P1Y`,
`P2W` and a bare `T`.

- **Server and independent validator.**
  `crates/trestle-protocol/tests/scalars.rs` checks:
  - the contract exports exactly the fixture's schema;
  - the server returns each expected status, code, error order and
    normalized value;
  - `jsonschema` (with formats) accepts exactly the valid cases;
  - the IR round-trips;
  - digit changes are judged for compatibility.
- **Browser.** `frontend/interaction-react/test/validate.test.ts` decodes
  the same schema and agrees on every case. It also rejects patterns the
  server would not write.
- **Units and forms.**
  - Unit tests cover each parser.
  - `form.rs` tests check that widgets fit their types and name their
    features.
  - `widgets.test.ts` checks that decimals stay strings and that a local
    date-time round-trips through UTC.
- **Mutation checks, each caught:** a whole part of `0` counted as a
  digit; offsets applied backwards; no leap years; temporal widgets
  needing no feature; the browser skipping digit limits; the browser
  accepting any date.

## Consequences

- **Agreement covers the fixture cases.** Off the fixtures, standard
  `date-time` validators also accept leap seconds (`23:59:60`), which the
  server refuses. Some, such as ajv-formats, also accept a space instead of
  `T`. Neither is in the fixtures.
- **Clients send strings,** including for amounts, which is the point.
