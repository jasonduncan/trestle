# ADR-027: Money and references in contracts

**Status:** Proposed (2026-10-17; design agreed by the project owner) ·
**Design doc:** §5.2, §5.4 · **Amends:** ADR-022 · **Depends on:** ADR-023,
ADR-024

## Context

Business models need amounts paired with their currency, and IDs that say
which model they point to:

- **Amounts.** ADR-022 made decimals strings, but a price is an amount and
  a currency together. Two loose fields can disagree or be half-filled.
- **References.** A `uuid` field said nothing about what it pointed to, so
  tools (inspection, Proseware's impact analysis) could not follow it.

## Decision

- **`Field::reference(name, model)`.**
  - **Wire and JSON Schema:** a `uuid`, unchanged, so renderers and clients
    see nothing new.
  - **IR:** records `"model"`, so contract exports and tools know the
    target.
  - **Validation:** a uuid's.
  - **Compatibility:** a uuid and a reference accept the same instances, so
    changing one to the other doesn't reject older clients.
  - **Widget:** the relation widget accepts both.
  - **`trestle.toml`:** `ref` fields use it.
- **`Field::money(name, precision, scale)`, optionally `.currencies(&[..])`.**
  - **Wire:** `{"amount": "12.50", "currency": "USD"}`, both members
    required and nothing else.
    - The amount is an ADR-022 decimal.
    - The currency is three capital letters, or one of the listed codes.
  - **JSON Schema:** an object with exactly those two properties, using the
    decimal `pattern` and the currency `pattern` or `enum`.
  - **Errors** point inside the value: `/price/amount`, `/price/currency`,
    `/price/<other member>` (`unknown_field`). A non-object is a 400 `type`
    at `/price`. The codes are the decimal ones, `choice`, or `format` with
    `params.format = "currency"`.
  - **Definition problems:** precision and scale as for decimals; unique,
    well-formed currency codes.
  - **Compatibility:** fewer digits, or fewer currencies than before,
    rejects older instances.
- **The browser** accepts exactly one kind of object field, this one. The
  decoder recognizes the shape the server writes and still refuses any
  other object. The validator mirrors the server member by member.
- **Forms.** The `money` widget is an amount box plus a currency select
  (when currencies are listed) or a three-letter box. A form using it
  requires `forms.money@1`, and the renderer declares it.
- **SDK:** `{ amount: string; currency: string }`.
- **Storage: two columns.** `<field>_amount numeric(p,s)` and
  `<field>_currency text`, checked to three characters or to the listed
  codes. They are nullable together when the field is optional.
  - **`trestle.toml`:** `type = "money", precision, scale, currencies = [..]`
    flattens on write and renders back as one object (`null` when the amount
    is null).
  - **Refused by `check`:** money as a default, filter, order or unique
    member, and another field named like one of its columns.
  - **Rust apps** declare the two columns themselves.

## Evidence

- **`fixtures/contracts/composites.json`, 16 cases:**
  - valid money and references, including listed currencies and a null
    optional;
  - an amount without its object, a number amount, missing members, `{}`,
    a null amount;
  - too many decimals, a lowercase or two-letter currency, an unlisted
    currency, an extra member;
  - a null required field, a reference that isn't a uuid;
  - every error at once, in order.

  They agree across the server (status, codes, order, normalized value),
  `jsonschema` with formats, and the browser.
- **The browser** refuses objects that aren't exactly money.
- **Widget tests:** the amount stays text, and a single listed currency is
  prefilled and treated as empty on its own.
- **`crates/trestle/tests/intent_types.rs`:** money becomes two columns and
  one nullable object schema; the spec problems above; a CRUD round trip
  (`{"amount":"10.50","currency":"EUR"}`, cleared with `null`, `GBP` refused
  at `/total/currency`).
- **`intent_relations.rs`:** a ref's contract field is
  `Reference { model: "crm.account" }`.
- **Mutation checks, each caught:**
  - the server accepting any currency string, or ignoring extra members;
  - the browser skipping the currency format, or taking any object as
    money;
  - writes not flattening money;
  - money rendered from raw columns.

## Consequences

- **The portable profile now has one object field type.** Tagged unions,
  the other planned composite, come next and will need their own form
  behavior (U8c part 2).
- **Currencies are format-checked, not looked up.** `XXX` passes unless the
  field lists its currencies.
