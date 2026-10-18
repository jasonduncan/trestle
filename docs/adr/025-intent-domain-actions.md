# ADR-025: Domain actions in `trestle.toml`

**Status:** Proposed (2026-10-17) · **Design doc:** §9.4, §10 · **Amends:**
the intent format · **Depends on:** ADR-018, ADR-024

## Context

Rust resources expose transitions as explicit actions (`lessons.publish`),
not as updates of a status field. `trestle.toml` could only do the same
through a Rust handler. A state machine as small as "win a deal" needed
code, and the reason was the same each time: lock the row, check its
state, write a few fields.

## Decision

An endpoint with `set`, `when` or `accept`, and no `handler`, is an
**action**:

```toml
[[endpoints]]
id = "crm.win"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/win"
permission = "crm.write"
set = { stage = "won", closed_at = "now" }   # what it writes
when = { stage = ["open"] }                  # the states it may start from
accept = ["reason"]                          # fields the caller may supply
```

- **Shape.** It is a `POST` on one row (`{id}` is its only parameter). Its
  contract is `id`, the accepted fields (each optional) and
  `expected_version`, bound from `If-Match`. Its response is the row with
  an `ETag`. It declares `writes` of its model (ADR-018).
- **Run, in the write transaction:**
  1. The row is fetched `FOR UPDATE` in the caller's tenant. Another
     tenant's row is a 404.
  2. Its version must equal `If-Match` (412).
  3. Every `when` field must hold one of its listed values. Otherwise the
     action is a 409 `invalid_state` that names the requirement.
  4. The accepted inputs and `set` constants are written, and the version
     is bumped. `"now"` on an instant field is the transaction's time in
     UTC. Accepted refs are tenant-checked as on any write (ADR-024).
- **Checked when the file is read:**
  - a non-`POST` path, or one without exactly `{id}`;
  - `input` on an action; an action that changes nothing;
  - unknown fields, `set` or `when` values that don't fit their field, and
    empty `when` lists;
  - a field both set and accepted;
  - `handler` together with `set`, `when` or `accept`.
- **Not supported:** `set` can't hold `null` (TOML has none) or a ref.
  Clearing a field or picking a row is for `accept`, or for a handler.

## Evidence

`crates/trestle/tests/intent_actions.rs` (PostgreSQL):

- the spec problems above;
- `inspect` shows a `POST` write on the declared path, with
  `writes: ["crm.deal"]` and input `expected_version`, `id`, `reason`;
- **a full transition:**
  - without `If-Match` it is 428, and with a stale version 412;
  - a viewer gets 403, and another tenant's member 404;
  - the member's win gives `stage: "won"`, the accepted reason, a UTC
    `closed_at` and version 2;
  - a second win is 409 `invalid_state`;
  - called through `Registry::invoke` without a version it is 422 on
    `/expected_version`;
  - reopen sets only `stage`.

Mutation checks:

- **Caught:** ignoring `when`; not writing `"now"`; dropping accepted
  fields.
- **Not caught (equivalent at run time):** making the operation's own
  version check optional. The binding and the contract already require it.

## Consequences

- **The pilot's `lessons.publish`** now has a declarative equivalent. Its
  audit event would still need a hook.
- **The remaining parity item** is forms and admin views (U9c).
