# ADR-023: Columns and `trestle.toml` fields for the scalar types

**Status:** Accepted (2026-10-22; proposed 2026-10-17) · **Design doc:** §13–§14 · **Amends:**
ADR-006 · **Depends on:** ADR-022

## Context

ADR-022 put decimal, date, instant and duration into contracts. To store
them, the historical layer needed matching column types:

- SQL rendering and `verify`;
- `HistoricalTx` for data steps, which refused `timestamptz` outright;
- `trestle.toml` fields, whose CRUD servers must answer in the contract
  forms. A `numeric` rendered by `jsonb_build_object` is a JSON number, which
  is exactly what ADR-022 forbids.

## Decision

- **Column types.** `ColumnType::Numeric { precision, scale }` (rendered
  `numeric(p,s)`), `Date`, `Interval`, and the existing `TimestampTz`.
  Builders: `Column::decimal(key, p, s)`, `Column::date` and
  `Column::duration`. `ColumnType::sql()` now returns `Cow<str>`.
- **`verify`** compares `numeric` precision and scale, so `numeric(12,2)`
  where history says `numeric(10,2)` is drift.
- **No new crates.** These values travel as text. PostgreSQL parses
  canonical strings on the way in, and fixed SQL expressions produce them on
  the way out:

  | Type | Bound as | Read back as |
  |---|---|---|
  | numeric | `$n::numeric` | `col::text` (`"1234.50"`) |
  | date | `$n::date` | `to_char(col, 'YYYY-MM-DD')` |
  | timestamptz | `$n::timestamptz`, the instant normalized to UTC first | `sql::iso_instant`: UTC, `…Z`, with only the fractional digits it needs, whatever the session `TimeZone` |
  | interval | `$n::interval` from ISO 8601 | `sql::iso_interval`: `P1DT2H30M`, or `PT0S` when zero, built from `extract`, whatever the session `IntervalStyle` |

  Values are checked against the column with the ADR-022 parsers, so a
  JSON number is refused as a decimal before any SQL runs.
  `sql::literal` (defaults, backfills) accepts the same strings.
- **`trestle.toml`.**
  - **New types:** `decimal` (with `precision`, and `scale`, default 0),
    `date`, `instant` and `duration`. Each maps to the column and the
    contract field of the same meaning.
  - **Defaults:** string defaults are checked: a decimal within its digits,
    a real date, a duration in the subset.
  - **Rows render** through the same read expressions, so CRUD answers in
    contract form.
  - **Writes** already go through `jsonb_populate_record`, which uses
    PostgreSQL's input functions.
  - **Filters** cast to the base type.

## Evidence

- **`crates/trestle-historical/tests/types.rs`:**
  - the planner renders `numeric(10,2)`, `date`, `timestamptz` and
    `interval`;
  - a data step inserts, reads back, updates and re-reads every type in
    contract form (`"2026-10-17T09:30:00.25+02:00"` reads back as
    `"…07:30:00.25Z"`), and refuses a JSON number for a decimal;
  - `verify` passes, then flags `numeric(12,2)` as drift;
  - `iso_instant` gives UTC from a session in America/New_York;
  - literals are checked against the column.
- **`crates/trestle/tests/intent_types.rs`:**
  - the columns and contract schemas declared;
  - precision, scale and default problems;
  - **a CRUD server whose sessions run at +05:30.** Creating returns
    `"1234.50"` (a string), the date, the instant in UTC, and the default
    `"PT30M"`. An update returns `"P1DT2H"` and a null instant. A filter by
    date works. A number for a decimal is 400, and too many digits is 422
    `decimal_digits`.
- **Mutation checks, each caught:** rows rendering raw columns; durations
  losing their days; instants formatted in the session zone; `verify`
  ignoring precision; values bound without casts.

## Consequences

- **`interval` columns written outside Trestle** with months or years are
  read back without them. Contract durations never carry them.
- **Sub-microsecond digits** are rounded by PostgreSQL on store.
