# ADR-021: Online index and check changes from the planner

**Status:** Accepted (2026-10-22; proposed 2026-10-17) · **Design doc:** §14.3–§14.4 · **Amends:**
ADR-006 · **Depends on:** ADR-019

## Context

`makemigrations` rendered an index on an existing table as `CREATE INDEX`,
which blocks writes for the whole build. It rendered a new check as
`ADD CONSTRAINT … CHECK`, which holds an exclusive lock while it scans every
row. Both were classified `existing_table_lock` and needed approval.

PostgreSQL's online forms were already accepted in hand-written non-atomic
migrations: `CREATE INDEX CONCURRENTLY`, and `ADD CONSTRAINT … NOT VALID`
followed by `VALIDATE CONSTRAINT`. Nothing generated them.

## Decision

- **`makemigrations --online`** (`PlanOptions::online`) splits each app's
  change in two:
  1. **An atomic migration** with everything else. A check added to an
     existing table is rendered `NOT VALID`: it is added without reading
     any rows, and new writes are checked from then on. This is
     `AlterCheck { not_valid: true }`; the field is left out when false, so
     existing manifests are unchanged.
  2. **A non-atomic `<id>_online` migration**, which depends on the first.
     It runs one step per operation: `CREATE INDEX CONCURRENTLY IF NOT EXISTS`
     for each index on an existing table, then `VALIDATE CONSTRAINT` for
     each `NOT VALID` check (the new `StateOp::ValidateConstraint`, which
     changes no state).
- **Indexes alone.** When a change only adds indexes, only the online
  migration is written.
- **Tables created in the same change** keep their inline `CREATE INDEX`
  and checks. They are empty, so nothing is gained by going online.
- **Classification.**
  - A `NOT VALID` check reads no rows, so it carries no
    `existing_table_lock`. An atomic half with nothing else risky is
    `auto`.
  - In a non-atomic migration, an index built `CONCURRENTLY` carries no
    `existing_table_lock` either.
  - Generated online SQL is recognized as generated, so it is not
    `hand_sql`. The online half still needs approval as `non_atomic`.
- **Failure and resume.** A row that breaks the new check fails its
  `VALIDATE CONSTRAINT` step and leaves the migration `partial`.
  - The index step before it has already committed.
  - The constraint stays `NOT VALID` and keeps checking new rows.
  - Fix the data and run `migrate` again: it resumes at the validation
    step.
  - ADR-019's impact report shows `index` and `validate` steps with table
    estimates.
- **Not covered:** unique constraints and foreign keys on existing tables
  still render in the blocking form. Their online forms need named
  constraints that the schema doesn't model yet: a unique index built
  `CONCURRENTLY`, then attached with `USING INDEX`; and `NOT VALID` foreign
  keys.

## Evidence

`crates/trestle-historical/tests/online.rs` (PostgreSQL):

- **The split.** A change adding a column, a check and an index to an
  existing table becomes two migrations:
  - the atomic half is `ADD COLUMN` and `CHECK … NOT VALID`, with no index;
  - the online half is `CREATE INDEX CONCURRENTLY IF NOT EXISTS`, then
    `VALIDATE CONSTRAINT`, in that order;
  - neither has a problem;
  - the atomic half classifies `auto` and the online half `[non_atomic]`;
- without `--online`, the change stays one atomic migration, and its
  manifest has no `not_valid`;
- an index alone becomes one online migration;
- **A populated table.** On a table with rows, the pair applies, the index
  and the constraint are valid, and `verify` matches;
- **A breaking row.** With a row that breaks the check:
  - validation stops at step 2, with the index built and the constraint
    still `NOT VALID` but already refusing new bad rows;
  - after the data is fixed, the rerun validates it.

Mutation checks, each caught:

- the online flag ignored;
- `NOT VALID` not rendered;
- indexes built without `CONCURRENTLY`;
- either classification exemption removed.

## Consequences

- **Two migrations to approve and apply**, instead of one lock. Between
  them, the check holds for new rows but is not yet proven for old ones,
  which is what `NOT VALID` means.
- **The online half runs outside a transaction.** It resumes by step, like
  any non-atomic migration.
