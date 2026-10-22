# ADR-043: An instant field can default to the time of its create

**Status:** Proposed (2026-10-22) · **Design doc:** §3.4, §13.2 ·
**Amends:** the intent format · **Depends on:** ADR-023, ADR-025, ADR-037

## Context

"When was this opened" is a field almost every business record has.
`created_at` is Trestle's own and can't be renamed or set, and before this
change an intent `instant` field could take no default at all: `fits`
accepted no instant constant. `"now"` existed only in an action's `set`
(ADR-025), so a record could be stamped when it changed state, but not when
it was created, short of a `before_create` or `before_write` hook in Rust.
Combined with `writable = "actions"` (ADR-037), which requires a default or
`optional`, there was no way to say "set on create, then only by actions".

## Decision

- **`trestle.toml`:** `default = "now"` on an `instant` field
  (`FieldDoc.default`, `FieldSpec::defaults_to_now`):

  ```toml
  fields = [
    { id = "opened_at", type = "instant", default = "now" },
    { id = "stage_changed_at", type = "instant", default = "now", writable = "actions" },
  ]
  ```

- **It is a database default, `DEFAULT now()`,** like every other intent
  default: `model_of` already renders `default` as the column's default
  (`default_bool`, `default_int`, `default_text`), and Trestle's own
  `created_at` is `DEFAULT now()`. So a create that leaves the field out
  stores the transaction's time, in UTC on the wire like any instant, and
  equal to the row's `created_at` and to any `set = { .. = "now" }` in the
  same transaction, since all three are PostgreSQL's `now()`. It is
  schema: turning it on for a new field plans an `ADD COLUMN ... DEFAULT
  now()`, so a required field can be added to a table with rows, which
  get the migration's time.
- **Every create path gets it,** because none of them names an omitted
  field: CRUD create, `Ctx::create` from handlers, hooks and Rust
  operations. A writable field still takes an explicit value. A managed
  field (`writable = "actions"`) is left out of the CRUD create contract
  and refused to `Ctx`, so it always starts at the default, and
  `default = "now"` satisfies ADR-037's "default or optional".
- **Rejected: filling it in the runtime,** the way an action's `set =
  "now"` reads `now()` and writes the value. It would make `"now"` the
  only intent default that isn't in the schema, so rows written by data
  steps or hand SQL wouldn't get it, and adding a required field to a
  table with rows would need a backfill.
- **Parse rules:**
  - `default = "now"` on a date, int, bool, decimal, duration or uuid
    field is a problem: `default = "now" is the time of the create, for
    instant fields; "x" is a date field`. On a text or enum field, "now"
    is just a constant of that type, as before.
  - Any other instant default is a problem: `an instant's default is
    "now", the time of the create`. Literal instant defaults are left out
    until a project needs them.
  - `default = "now"` on a handler endpoint's input is a problem: an
    input has no row for the database to fill.
- **`"now"` is one constant,** `intent::spec::NOW`, shared by actions and
  defaults.

## Evidence

`crates/trestle/tests/intent_types.rs` (PostgreSQL; the pool's session
zone is Asia/Kolkata, so UTC rendering is checked too), on an order with
`opened_at` (`default = "now"`) and `stamped_at` (`default = "now"`,
`writable = "actions"`, set by an action):

- `the_intent_declares_columns_and_contracts_of_each_type`: both columns'
  desired default is `DbDefault::Now`.
- `decimal_settings_and_defaults_are_checked`: `default = "now"` on a
  date, an int and a duration are each the problem above; an instant
  literal default is refused with the "now" hint; a text field with
  `default = "now"` is fine.
- `crud_keeps_every_value_in_its_contract_form`:
  - a CRUD create without either field stores both as exactly the row's
    `created_at`, within the last minute of the database's clock;
  - an explicit `opened_at` is stored as given, in UTC;
  - sending `stamped_at` to create is 422 `unknown_field`;
  - a handler's `Ctx::create` gets both defaults, equal to its row's
    `created_at`.

`intent_doc.rs`: `every_key.toml` has an instant with `default = "now"`,
and the document round-trips and assembles like its file.

## Consequences

- **"Stamped on create" is one key,** with no hook, and combines with
  `writable = "actions"` for "stamped on create, then only by actions".
- **A model's `before_write` hook doesn't see the value on create,**
  like any default (ADR-039: the row to insert is "without `id` or
  defaults"); `after_write` and the response do.
- **Changing an existing field's default to or from `"now"`** is a default
  change, which `makemigrations` doesn't generate yet ("changes its
  database default; not supported by the generator yet"); write that
  migration by hand, as for any default.
- **No breaking change:** files that parsed before mean the same, and an
  instant default was always refused.
