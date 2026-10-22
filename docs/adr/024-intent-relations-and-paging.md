# ADR-024: Relations and cursor paging in `trestle.toml`

**Status:** Accepted (2026-10-22; proposed 2026-10-17) · **Design doc:** §3.4, §12.3 · **Amends:**
the intent format · **Depends on:** ADR-022, ADR-023

## Context

A `trestle.toml` model could hold a `uuid`, but couldn't say what it
pointed to. Two things followed:

- **No relations.** There was no foreign key, and nothing stopped a write
  from naming another tenant's row. §12.3 requires foreign-key writes to
  check visibility and tenant compatibility, not just existence.
- **No real paging.** Intent lists took a `limit` and nothing else, while
  Rust lists page with signed keyset cursors (`pagination`).

The parity guide listed both as reasons apps stayed in Rust.

## Decision

- **`type = "ref", to = "<model id>"`.** A ref field references a model
  declared in the same file:
  - it is a `uuid` column with a foreign key to that model, plus an index
    `<table>_<field>_idx`;
  - its contract field is a `uuid`.

  `check` refuses a ref without `to`, `to` on any other type, and a target
  that isn't in the file.
- **Tenant-checked writes.** Before every create and update, including
  `Ctx::update` from handlers, each non-null ref value must name a row of
  its model in the caller's tenant.
  - That row is locked `FOR KEY SHARE` until the write commits, so it can't
    be deleted in between.
  - A missing row, or another tenant's, is the same 422: `/<field>`
    `not_found`, "No account has this ID." Existence in another tenant is
    not disclosed.
- **Deleting a referenced row** is 409 `still_referenced`, not a 500 from
  the foreign key.
- **Cursor paging, as in Rust:**
  - lists take an optional `cursor`, and pages carry `next_cursor`, which
    is `null` on the last page;
  - pages are ordered by the endpoint's `order` field, then `id`, and
    continue strictly after the last row by row comparison;
  - the cursor is signed and bound to its query (filters, limit and order),
    so another query's cursor is 400 `cursor_mismatch` and a tampered one
    is 400 `invalid_cursor`.

  `order` must name a required field or `created_at`, because a null key
  has no place in a keyset order.

## Evidence

`crates/trestle/tests/intent_relations.rs` (PostgreSQL):

- a ref is an indexed foreign key;
- the spec problems: no `to`, `to` on text, an unknown target, an optional
  order field;
- **tenant checks.** A deal may name an account in its own tenant:
  - another tenant's account and a random ID are both 422 `/account`
    `not_found`;
  - an update to another tenant's partner is refused, its own is accepted,
    and null clears it;
  - deleting the referenced account is 409 `still_referenced`;
- **paging.** Five accounts (and one in another tenant) paged two at a time
  by `name`:
  - every row arrives once, in order, only this tenant's, in three pages;
  - a cursor used with another limit is `cursor_mismatch`, and a tampered
    one is refused;
  - the default `-created_at` order pages too.

Mutation checks, each caught:

- writes skipping the check;
- the check ignoring the tenant;
- the delete conflict becoming a 500;
- pages ignoring the cursor;
- optional order fields allowed.

The units parity test still passes with the new page shape.

## Consequences

- **Breaking (intent format):** an `order` on an optional field is now a
  problem. Intent list outputs gain `next_cursor`, and inputs gain
  `cursor`.
- **Still missing for full parity:** domain actions and forms (U9b, U9c).
