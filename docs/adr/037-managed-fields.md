# ADR-037: Fields that only actions write

**Status:** Proposed (2026-10-21; design agreed by the project owner) ·
**Design doc:** §9.4, §10 · **Amends:** the intent format, ADR-025,
ADR-026, ADR-029 · **Depends on:** ADR-012, ADR-025

## Context

A domain action (ADR-025) moves a deal from `open` to `won` only when its
`when` rule holds. The same model's generated CRUD `update` accepted
`stage` too, so a plain `PATCH` skipped the rule. Handlers and hooks could
also write it, through `Ctx::create`/`Ctx::update` or the row a hook
returns. Django's answer is `editable=False` plus transition methods
(django-fsm's `protected=True`). Proseware, which generates `trestle.toml`
files, needs the same guarantee without Rust.

## Decision

- **`trestle.toml`:** a model field may say `writable = "actions"`. The
  other value is `"always"`, the default, which is today's behaviour; a
  file without the key reads as before. A managed field is written only by
  domain actions, through `set` and `accept`.
- **`IntentDoc`:** `FieldDoc.writable: Option<String>`, left out of the
  rendering when `None` (absent means `"always"`). `FieldSpec.writable` is
  a `Writable` enum (`Always`, `Actions`) with `FieldSpec::managed()`.
- **Problems when the file is read,** on the field's line:
  - `writable = "<x>"; use "always" (the default) or "actions"`;
  - a managed field that is neither `optional` nor has a `default`:
    create cannot set it;
  - a managed field that no action on its model sets or accepts: nothing
    could ever write it;
  - `writable` on a handler endpoint's `input` field.
- **Every other write path refuses it:**

  | Path | Result |
  |---|---|
  | CRUD create, CRUD update (HTTP, `Registry::invoke`) | the field is not in the input contract; sending it is the usual 422 `validation_failed` with `unknown_field` at `/stage`. Create stores the default. |
  | `Ctx::create`, `Ctx::update` (handlers, hooks, Rust operations) | 500 `internal_error`; the log names the field and model. The transaction rolls back, earlier writes included. |
  | `Row::set` on a managed field (hooks) | the same 500. CRUD writes also store only non-managed fields, so a row can't carry one through. |
  | Actions (`set`, `accept`) | written as before, after `when` and `If-Match`. |

  The 422 comes from the contract, because the contract is the only
  definition of the wire input (ADR-002): browsers, the SDK and OpenAPI
  already agree that the field isn't accepted, and a special code would
  need a read-only notion in the contract IR. Trusted code that writes the
  field is a programming error, not a bad request, so it gets the same 500
  as other misuse of `Ctx` and `Row`, never a silent drop.
- **Reads are unchanged.** The representation, lists, filters, order,
  search, admin columns and filters, views and record pages keep the field.
- **Forms.** Generated create and update forms (ADR-026) and admin create
  and edit forms (ADR-029) leave the field out, matching their contracts.
  An action form keeps a managed field it accepts.
- **Contract change only.** `writable` is not schema state: no column,
  constraint or migration changes, in either direction. CRUD input
  contracts, OpenAPI and form descriptors change, so their revisions move.
- `If-Match` on update, delete and actions is unchanged.

## Evidence

`crates/trestle/tests/intent_actions.rs` (PostgreSQL):

- `actions_are_checked_when_the_file_is_read`: an unknown `writable`
  value; a required managed field, which also has no action writing it.
- `a_managed_field_is_written_only_by_actions`, on the CRM deal with
  `stage` managed:
  - the create and update inputs leave `stage` out and the outputs keep
    it; the create, update, admin create and admin edit forms show
    `/title`, `/closed_at`, `/reason`; admin columns and filters keep
    `stage`; the desired schema equals the unmanaged file's;
  - create with `stage` is 422 `unknown_field` at `/stage`; without it the
    deal is `open`; update with `stage` is the same 422;
  - a `before_update` hook calling `row.set("stage", ..)` is a 500;
  - a handler that updates the title and then `stage` through
    `Ctx::update` is a 500, and the title and version are unchanged;
  - a plain update keeps `stage`; `win` sets it to `won`; `?stage=won`
    finds the deal.
- `intent_doc.rs`: `every_key.toml` uses `writable`, and the document
  round-trips and assembles like its file.

Mutation checks, both caught: `Row::set` not refusing (the update then
returns 200 with `stage` silently kept); `Ctx::update` not refusing (a 422
instead of the 500).

## Consequences

- **Transitions can be enforced in the file.** A status field governed by
  actions can no longer be edited around them.
- **Handlers can't move a managed field.** A transition that needs Rust
  either drops `writable = "actions"` or becomes a declared action plus a
  hook.
- **`FieldDoc` and `FieldSpec` have a new member.** Code that builds them
  without `..Default::default()` needs `writable`.
- **No per-field refusal code.** Clients see `unknown_field` for a managed
  field, the same as for a field that doesn't exist.
