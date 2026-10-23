# ADR-041: A model can keep an append-only change log of every write

**Status:** Proposed (2026-10-22) · **Design doc:** §3.4, §9.4 ·
**Amends:** the intent format, the intent API · **Depends on:** ADR-018,
ADR-024, ADR-030, ADR-039, ADR-040

## Context

Business records need an answer to "who changed this, when, through what,
and from what to what". Django projects reach for django-simple-history or
django-reversion, which hook every `save()`. An intent project had no way
to get it short of an `after_write` hook (ADR-039) per model writing rows
of a model it declared itself, which is the same code in every project and
easy to get subtly wrong (old values, nested writes, rollbacks).
Proseware, which generates `trestle.toml` files, needs it as data.

ADR-039 already gives the one place that sees every write: `Ctx::write`,
which CRUD, actions, handlers, hooks and Rust operations all go through.

## Decision

- **`trestle.toml`:** a model opts in with `history = true`
  (`ModelDoc.history`, `ModelSpec.history`):

  ```toml
  [[models]]
  id = "crm.deal"
  history = true
  ```

- **Storage is schema.** The log is a generated model,
  `<app>.<name>_history` (`ModelSpec::history_model`), in table
  `<table>_history` (`ModelSpec::history_table`), installed with the
  model's app. `makemigrations` plans it like any model, so turning
  `history` on is a reviewed migration and `makemigrations --check` stays
  meaningful. Columns:

  | Column | |
  |---|---|
  | `id` | uuid, the entry |
  | `<tenant>_id` | with `[tenancy]`, references the tenant |
  | `row_id` | the row written; no foreign key, so a deleted row keeps its log |
  | `version` | the row's version after the write (on delete, the version deleted) |
  | `kind` | `create`, `update` or `delete` |
  | `actor_id` | the acting `ActorId` |
  | `operation` | `Write::operation()`: the operation whose write it is, or `"ctx"` for a `Ctx` write |
  | `outer_operation` | the intent operation whose transaction made the write; null under a Rust operation's `Ctx::run` |
  | `changed_at` | `DEFAULT now()`: the transaction's time |
  | `changes` | JSON text: field → `{ "old": .., "new": .. }` for the fields whose value differs |

  An index on (`<tenant>_id`, `row_id`) serves the reads. A parse problem
  refuses a model whose log names are taken by another model or table,
  and a model without fields.
- **Written inside `Ctx::write`,** after the row write and before
  `after_write`, on the operation's connection. Every path is covered
  because every path is already there (ADR-039): CRUD create, update and
  delete, actions, and `Ctx::create`, `update`, `update_versioned` and
  `delete` from handlers, endpoint hooks, write hooks and Rust operations.
  A nested write logs its own entry, with `operation = "ctx"` and the
  outer intent operation. Logging does not depend on the hook chain: a
  row whose hooks are not re-run in a chain still logs the write. A
  write refused before the row write logs nothing, and one refused after
  it rolls back with its entry, since both are in one transaction. SQL
  that doesn't go through `Ctx::write` logs nothing: a migration's data
  steps (ADR-020), a hand-written operation's own queries, and anything
  run in `psql`. The log records the application's writes, not every
  change to the table.
- **Old values.** An update or delete of a history model without a row
  already locked by its caller is fetched `FOR UPDATE` first, as for
  hooked models. Old and new are both the row's representation
  (`representation_sql`), so equal values compare equal and the values in
  the log are in their contract form (decimals and instants as strings,
  money as `{amount, currency}`). Fields whose value didn't change are
  left out; an update that changes nothing is still logged, with no
  changes, because it bumped the version.
- **Reads.**
  - `Ctx::history(model, id)` returns `Vec<HistoryEntry>`, newest first
    and at most `MAX_LIMIT` (100), like `Ctx::list`. It is tenant-scoped
    and trusted like the rest of `Ctx` (ADR-040), so it also reads the log
    of a deleted row. Asking for a model without history is a 500.
  - `history = true` on a CRUD endpoint (`EndpointDoc.history`) publishes
    `GET <path>/{id}/history` as operation `<model>.history`, which needs
    the model's `history = true` and `"get"` in `crud`. It uses the
    endpoint's `read` permission and finds the row exactly as `get` does,
    tenant first and through the caller's row scopes, so another tenant's
    row, a row outside the caller's scopes and a deleted row are all 404.
    It pages newest first with `limit` and a signed `cursor` bound to the
    row, the caller and the page size. Publishing stays explicit (I01):
    the model's flag records, the endpoint's flag publishes.
- **The wire shape is typed.** A contract has no free-form object, so the
  page's contract is built from the model: each entry has `id`, `row`,
  `version`, `kind`, `actor`, `operation`, `outer_operation`, `at` and
  `changes`, a list of `{"change": {"field": "stage", "old": "open",
  "new": "won"}}`, where `change` is a union tagged by `field` with one
  variant per model field, and `old` and `new` have that field's type
  (nullable). The executor checks every page against it (ADR-012).
- **Entries written under an earlier model** (fixed in 0.8.1). The log
  keeps what was written, but the page is typed by the model as it is
  now. A change to a field the model no longer has is left out, and a
  value its field no longer admits (an enum value removed, a range
  tightened) is shown as `null`. Either way the entry has
  `incomplete: true`. Before 0.8.1 such an entry failed the page's
  output check, and that row's history was a 500 from then on.
  `Ctx::history` returns entries as stored.
- **A field can be left out** with `history = false` on it, for a value
  the log must not keep, such as a token:

  ```toml
  fields = [{ id = "api_token", type = "text", history = false }]
  ```

  Its changes are never written to `changes`, and the page's contract has
  no variant for it. An update that changes only such fields is still
  logged, with no changes, because it bumped the version. Entries written
  before the field was left out still hold its values in the table: the
  page shows them without it and with `incomplete: true`, and erasing them
  is a data step. `history` on a field of a model without `history =
  true`, or on a handler input, is a parse problem (`FieldDoc.history`,
  `FieldSpec.history`).
- **A row's whole past is shown to whoever can read the row now.** The
  endpoint finds the row through the caller's scopes as they are now and
  then returns all of its entries. A row moved into a caller's team, or
  newly matching a scope's `where` (ADR-044), shows its earlier edits and
  the actor IDs of who made them. That is intended: it is how audit
  trails such as Salesforce's field history and django-simple-history
  work, where access to the record governs its history. Limiting entries
  to the time the caller could see the row would need the scopes as they
  were at each entry, and the `through` rows that decide them keep no log.
  A field whose past values must not reach later viewers takes
  `history = false`.
- **Declared effects (ADR-018).** A write that declares effects declares
  `writes` of the log model too, and the history read declares `reads` of
  the model, its log and its scopes' `through` models.

## Evidence

`crates/trestle/tests/intent_actions.rs` (PostgreSQL), on the ADR-039
fixture with `history = true` on the deal and the organization:

- `history_is_checked_when_the_file_is_read_and_planned_as_schema`: the
  endpoint flag without the model's, and without `get`, are parse
  problems, as are `history` on an action and a model whose log table is
  taken (with its line). `crm.win` and `crm.deal.create` declare `writes`
  of `crm.deal` and `crm.deal_history`. After an initial
  `makemigrations`, turning `history` on writes `crm/0002_history.sql`
  with `CREATE TABLE deals_history` and nothing else of the deal's, and
  `makemigrations --check` is then clean.
- `history_logs_every_committed_write_once_on_every_path`:
  - a CRUD create of an organization logs one `create`, whose changes are
    its non-null fields, and its `after_write`'s own `Ctx::update` logs one
    `update` (`operation` `"ctx"`, outer `crm.organization.create`);
  - the refused action, CRUD update and handler (409
    `approval_required`) log nothing;
  - the winning action logs one deal entry (`crm.win`, version 2, the
    member as actor, `stage` open → won), and the nested `Ctx::update` of
    the deal's `after_write` logs one organization entry, `customer`
    false → true, `operation` `"ctx"`, outer `crm.win`;
  - a CRUD update refused after the row write, by the endpoint's
    `after_update`, leaves both logs as they were; the same update
    committed logs one entry with `stage`, `title` and the hooks' `note`;
  - an unversioned `Ctx::update` of the approval, a model without hooks,
    logs `consumed` false → true;
  - the published log pages newest first with `limit` and `cursor`,
    reads for a viewer, and is 404 for another tenant's member; a handler
    reads the same entries with `Ctx::history`.

`crates/trestle/tests/intent_scopes.rs`:
`a_rows_history_is_as_hidden_as_the_row`: a salesperson reads their own
row's log; another salesperson's row, and another team's for the manager,
are 404 without entries; a role without the grant is 403; the
salesperson's own rename is logged with them as the actor. A deleted
row's log is 404 through the endpoint and kept in the table, ending with a
`delete` entry whose changes end in `null`.

`a_field_with_history_false_is_never_logged`: with the opportunity's
`title` left out, a salesperson's update of `title` and `stage` logs only
`stage`, the create logs `owner`, `stage` and `team`, and no entry in the
table holds `title`. `history_false_needs_a_model_with_history`: the key
on a field of a model without history is a parse problem.

`intent_doc.rs`: `every_key.toml` sets `history` on a model, on one of
its fields and on its CRUD endpoint, and the document round-trips and assembles like its file.

Mutation check, caught: **not fetching the old row for history** (only for
hooks) makes the organization's own nested write, whose hooks don't run
again, log `old: null`, and the first log assertion fails.

## Consequences

- **An audit trail is one key per model,** on every write path, in the
  write's transaction, with no hook code.
- **Each write to a history model costs one more insert,** and an
  unversioned `Ctx::update` or `Ctx::delete` one row lock, as with hooks.
  The log grows without bound; pruning or archiving it is the project's
  job (a data step or a job), not this ADR's.
- **Turning `history` off** removes the log model, which `makemigrations`
  refuses like any removed model until it is approved with
  `--allow-destructive <app>.<name>_history`.
- **`changes` is text holding JSON,** not `jsonb`: the schema IR has no
  JSON column type, and adding one is a separate change. Reading it in SQL
  needs `changes::jsonb`.
- **Breaking for struct literals:** `ModelSpec` has `history`,
  `EndpointSpec::Crud` has `history`, and `ModelDoc` and `EndpointDoc`
  have `history` fields (`..Default::default()` covers the docs). Since
  field exclusion, `FieldSpec` has `history` and `FieldDoc` a `history`
  field too. New
  public types `HistoryEntry` and `FieldChange`.

### Open questions

- **The outer operation of a Rust operation's writes.** `Ctx::run` doesn't
  know the operation that calls it, so its writes log
  `outer_operation = null`. A `Ctx::run` variant that names it can come
  when a project needs it.
- **Reverting** to an entry is left out until a project needs it.
