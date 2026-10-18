# ADR-019: Plans assert their source state and report their impact

**Status:** Proposed (2026-10-17) · **Design doc:** §14.3–§14.4 · **Amends:**
ADR-006 · **Depends on:** ADR-017

## Context

`migrate --plan` (`trestle.plan/1`) said which migrations would run, how
each was classified and its digest. It did not say:

- **What the database is now.** Neither `plan` nor `migrate` checked that
  the catalog matched the schema its applied history describes. `serve`
  refused drift, but only after a migration had already run against it.
- **What a migration assumes about the data.** A migration could not state
  "no duplicate slugs" or "every order has an amount". It failed partway
  if that wasn't true, or, worse, silently did the wrong thing.
- **What a step would do to existing rows.** It did not say how many null
  rows a `SET NOT NULL` would meet, how many duplicate groups a new unique
  constraint would meet, or how many values a drop deletes. A reviewer
  approving a digest could not see the data impact.

## Decision

- **Source state.** Before applying anything, the runner compares the
  catalog with the schema its applied history describes (`verify`'s
  comparison).
  - A difference stops the run with `HistoryError::Drift`, exit 6, before
    any migration is attempted.
  - `migrate --plan` lists it under `database.drift` and in `blocked`.
  - `--allow-drift` (or `RunConfig::allow_drift`) overrides it for a known,
    reviewed difference.
  - History without manifests cannot be replayed into a schema, so it is
    not compared, as `verify` already reports.
- **Preconditions.** A manifest may list
  `"preconditions": [{"name": …, "violations": "SELECT …"}]`.
  - **Form:** each check is one `SELECT` (or `WITH`) that returns how many
    rows break it. `check` validates the form.
  - **Atomic migrations:** the runner evaluates the checks inside the
    migration's own transaction, after its fence lock and before its SQL.
  - **Non-atomic migrations:** the checks run once, before step 1 and
    before the fence goes up.
  - **On a violation,** the migration rolls back with nothing changed. It is
    recorded `failed`, and the run exits 4 with the check's name and count.
  - **In the plan,** each check is evaluated and reported, and any
    violation is `blocked`.
  - The field is omitted when empty, so existing digests are unchanged.
- **Impact.** For every pending step, `trestle.plan/1` reports
  `impact: [{model, table, change, rows, affected, meaning, blocks}]` for
  each operation on an existing table.
  - `rows` is the planner's estimate (`reltuples`).
  - `affected` is an exact count where it decides the outcome:

    | Change | Counts | Blocks the plan |
    |---|---|---|
    | `not_null` | null rows | yes, unless a backfill in the same migration fills them, or a data step might |
    | `unique` | duplicate groups, with NULLs excluded | yes, unless a data step might fix them |
    | `backfill` | rows to fill | no |
    | `drop_column` | values deleted | no |
    | `drop_table` | rows deleted | no |
- **Read-only measurement.**
  - Every count runs in its own `BEGIN READ ONLY` transaction under a 5 s
    statement timeout, and is rolled back.
  - A count that times out, or that names a table an earlier pending step
    creates, is reported with a note instead of a number.
  - Impact and precondition results are not part of the plan digest:
    approval binds what will run, and the runner re-checks preconditions
    when it applies.

## Evidence

`crates/trestle-historical/tests/impact.rs` (PostgreSQL):

- a planner-generated migration that drops a column, requires a field and
  adds a unique constraint reports 2 values deleted, 2 null rows (blocks)
  and 1 duplicate group (blocks; two NULL SKUs are not duplicates), and
  counting changes nothing;
- a backfill reports 2 rows to fill and unblocks `SET NOT NULL`, which then
  applies;
- a violated precondition stops an atomic migration with nothing applied.
  It is recorded `failed`, and it applies once the data is fixed;
- a non-atomic migration is checked before step 1 and before its fence;
- malformed preconditions are problems: an empty or repeated name, a
  non-`SELECT`, two statements, or a value that isn't a list;
- drift stops `migrate` before anything is attempted, and `allow_drift`
  overrides it;
- a check on a table that an earlier step creates is reported as not yet
  measurable.

`crates/trestle/tests/reconcile.rs`:

- the plan lists evaluated preconditions, blocks on a violation (exit 1),
  and `migrate` exits 4;
- after drift, the plan shows `database.drift` and blocks, `migrate` exits
  6, and `--allow-drift` applies.

Mutation checks, each caught:

- the runner skipping the drift check;
- atomic or non-atomic migrations skipping their preconditions;
- the plan ignoring violations or drift;
- a backfill not unblocking `SET NOT NULL`;
- NULLs counted as duplicates.

## Consequences

- **Breaking (behavior):** `migrate` refuses to apply over drift that
  `serve` would have refused to start on anyway. Fix it, or pass
  `--allow-drift` after review.
- **Cost:** a plan for a large table runs exact counts, each bounded by
  5 s, read-only. Apply runs only the preconditions.
- **Limits:** preconditions are reviewed SQL like the migration itself, not
  a sandbox. A `WITH` query that writes is caught by the plan's read-only
  transaction, but not inside the migration's own transaction.
