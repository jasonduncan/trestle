# ADR-036: Guarded actions

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Design doc:** §9.4, §10 · **Amends:** the intent format, ADR-025 ·
**Depends on:** ADR-018, ADR-024, ADR-027, ADR-032, ADR-037

## Context

A domain action (ADR-025) can only require that row fields hold one of
some constants (`when`). Business transitions need more, and today each
one needs a Rust handler:

- "entering Proposal needs a known, non-negative value and an expected
  close date";
- "entering Contract also needs a principal contact who is currently
  affiliated with the deal's organization";
- "asking for the stage the deal is already in changes nothing; it is not
  an error".

django-fsm calls the first two transition `conditions`. Proseware, which
generates `trestle.toml` files, needs them as data.

## Decision

Four data-only keys on an action endpoint:

```toml
[[endpoints]]
id = "crm.contract"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/contract"
permission = "crm.write"
set = { stage = "contract" }
when = { stage = ["proposal"] }
accept = ["expected_close"]
require = ["value", "expected_close", "principal_contact"]
check = [
  { field = "value", ge = "0" },
  { field = "value", gt = { amount = "50000.00", currency = "USD" } },
]
exists = [{ model = "crm.affiliation",
            match = { contact = "principal_contact", organization = "organization" },
            null = ["ended_at"] }]
repeat = "unchanged"
```

- **The row as the action writes it.** `require`, `check` and `exists`
  look at the current row merged with the accepted inputs and the `set`
  values (`"now"` included), so an action that accepts the missing field
  can satisfy its own requirement. A model's `before_write` hook (ADR-039)
  runs after the guards and may change the row further, and the guards
  aren't evaluated again: hooks are trusted code, so the stored row is the
  guarded row plus whatever the hook set.
- **`require`:** these fields are non-null. Money is null when its amount
  is.
- **`check`:** one comparison per entry, exactly one of `eq`, `ne`, `lt`,
  `le`, `gt`, `ge`:
  - int, decimal (string), date, instant and duration take all six; bool,
    text and enum only `eq` and `ne`; IDs (`uuid`, `ref`) none (use
    `require` or `exists`);
  - money compares with `{ amount, currency }`: a value in another currency
    is unmet and reported as `currency_mismatch`. A decimal string compares
    the amount alone, whatever the currency (`ge = "0"`);
  - PostgreSQL compares, in the column's type, so `"10.5"` equals
    `"10.50"` and instant offsets don't matter;
  - a null field never meets a check, `ne` included.
- **`exists`:** some row of `model` in the caller's tenant has, for each
  `match` entry, its field (the key) equal to this row's field (the
  value), and each `null` field null. Either side of `match` may be `id`.
  A null local value is unmet without a query.
  - **Tenant first,** in SQL, like every query; another tenant's row never
    counts.
  - **Locked `FOR SHARE`** (one row, `LIMIT 1`) until the action commits.
    `FOR KEY SHARE`, which ref checks use (ADR-024), blocks only deletes
    and key changes, so a concurrent `UPDATE ... SET ended_at = now()` would
    commit alongside the action and leave a contract signed with an ended
    affiliation. `FOR SHARE` blocks that update; an action that meets the
    row while such an update is in flight waits, then rechecks the new
    version (here: unmet). The cost is that the related row can't be
    updated until the action commits, which is brief.
  - **Apps:** the action's app requires the related model's app, exactly
    as a ref does (ADR-032), and those edges join the one-way rule: a
    circle of refs and `exists` between apps is the same parse problem.
  - **Effects:** the action declares `reads` of the related model
    (ADR-018).
- **`repeat`:** `"refuse"` (the default) is today's behaviour. With
  `"unchanged"`, when every `set` constant already holds (`"now"` fields
  are skipped), the action is a no-op: 200 with the current row and its
  `ETag`, nothing written, version unchanged, accepted inputs ignored. An
  action whose `set` is only `"now"` fields can't use it.
- **Order, in the write transaction:** lock the row (`FOR UPDATE`) →
  `If-Match` (412) → `repeat` no-op → `when` (409 `invalid_state`) → the
  row as the action writes it → `require` → `check` → `exists` → the
  model's `before_write` (ADR-039) → write.
- **One 409 for every unmet condition,** `requirement_unmet`, with nothing
  written:

  ```json
  { "status": 409, "code": "requirement_unmet",
    "detail": "This needs value. This needs expected_close. value must be at least 0.",
    "errors": [
      { "pointer": "/value", "code": "required", "message": "This needs value." },
      { "pointer": "/expected_close", "code": "required", "message": "This needs expected_close." },
      { "pointer": "/value", "code": "check", "message": "value must be at least 0.",
        "params": { "op": "ge", "value": "0" } } ] }
  ```

  Codes are `required`, `check`, `currency_mismatch` and `exists`
  (`params.model`, pointer at the first local `match` field). Every
  condition is evaluated and every unmet one listed. The body carries
  field names and the file's constants, never SQL; the only row value in
  it is the currency named by `currency_mismatch`.
- **Everywhere the operation runs.** The guards are in the operation, so
  `Registry::invoke`, jobs and admin buttons get them too.
- **Checked when the file is read:** unknown fields; an unknown or second
  operator, or none; a constant that doesn't fit (bad decimal scale, a
  currency the field doesn't take, a malformed date or instant); ordering
  operators on bool, text or enum; checks on IDs; an `exists` model not in
  the file, an empty `match`, sides of different types (refs must point to
  the same model; money can't be matched), `null` on a required field;
  an unknown `repeat`, or `"unchanged"` with no constant to compare. Any of
  the four keys makes an endpoint an action, so they can't go on a handler.
- **`IntentDoc`:** `EndpointDoc` has `require`, `check: Vec<CheckDoc>`,
  `exists: Vec<ExistsDoc>` and `repeat: Option<String>`.
  `EndpointSpec::Action` has `require`, `check: Vec<CheckSpec>`,
  `exists: Vec<ExistsSpec>` and `repeat: Repeat`.
- **Unchanged:** action forms and admin buttons. Requirements are not form
  hints, and the descriptor doesn't carry them.

## Evidence

`crates/trestle/tests/intent_actions.rs` (PostgreSQL), on a CRM pipeline
(deal with a managed `stage`, optional money `value`, `expected_close`
and `principal_contact`, and an affiliation with optional `ended_at`):

- `guards_are_checked_when_the_file_is_read`: each problem above, the
  app circle through `exists`, and a guard on a handler endpoint;
- `require_and_check_see_the_row_as_written_and_repeats_are_no_ops`:
  - no value and no date: 409 naming `/value` required, `/expected_close`
    required and `/value` check, with the stage and version unchanged;
  - a negative value: 409 on the check, and the accepted date not written;
  - zero and an accepted date: 200, stage `proposal`;
  - again, with a different accepted date: 200 with the same body and
    `ETag`; a stale `If-Match` is still 412;
  - through `Registry::invoke`: the same 409 with three errors;
- `money_checks_compare_the_amount_in_the_same_currency`: `gt` 50000.00
  USD refuses 50000.00 USD (`check`) and 90000.00 EUR
  (`currency_mismatch`), and accepts 50000.01 USD;
- `exists_needs_a_current_related_row_in_the_callers_tenant`:
  - the action reads `crm.affiliation` and writes `crm.deal`;
  - no principal contact: `required` and `exists`;
  - no affiliation, another tenant's with the same IDs, and an ended one
    are 409 `exists`; a current one is 200;
  - **concurrency:** a transaction sets `ended_at` on the current
    affiliation and stays open. The action waits on the lock (seen in
    `pg_stat_activity`); after the commit it is 409 `exists`.
- `intent_doc.rs`: `every_key.toml` uses every new key and operator, and
  round-trips and assembles like its file.

Mutation checks, each caught: `FOR KEY SHARE` instead of `FOR SHARE` (the
action doesn't wait, and signs with an ended affiliation); `exists`
without the tenant condition; guards on the current row instead of the
row as written; ignoring `repeat`.

## Consequences

- **The transitions above need no Rust.** Anything beyond these
  comparisons still needs a hook or handler.
- **Breaking for exhaustive patterns:** `EndpointSpec::Action` has four
  new members, so code matching it without `..` needs them. `EndpointDoc`
  struct literals need `..Default::default()`.
- **A held lock on related rows.** Updates of a matched row wait for the
  action's commit.
- **Future work:** showing requirements in action descriptors and forms,
  comparing two fields of the row, and `exists` counts ("at least two").
