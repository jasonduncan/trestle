# ADR-020: Batched data steps that resume from a checkpoint

**Status:** Proposed (2026-10-17) · **Design doc:** §14.2, §14.4 · **Amends:**
ADR-006 · **Depends on:** ADR-019

## Context

A data step ran once, inside its migration's transaction. That is right
for small fixes, but a backfill over a large table then holds one long
transaction:

- its row locks block writers for the whole run;
- a failure near the end throws away all the work;
- nothing records how far it got.

§14.4 asks for expand, then a **resumable** backfill, then validation.
Non-atomic migrations already resume at a step, but a step there is a SQL
statement, not code that reads and writes rows through `HistoricalTx`.

## Decision

- **A second kind of data step.**
  - `DataStep::batched(name, source, run)` takes a `BatchFn`: it gets the
    checkpoint the previous batch returned (`None` the first time), does one
    bounded batch through `HistoricalTx`, and returns the next checkpoint,
    or `None` when nothing is left.
  - The checkpoint is any JSON, typically the last primary key, as with
    `HistoricalTx::batch(.., after, limit)`.
  - `DataStep::once` is the existing kind. `DataStep::run` is now
    `DataRun::{Once, Batched}`.
- **Where it runs.** A batched step runs in a data-only migration with
  `"atomic": false`, which `makemigrations --empty --batched` creates.
  `check` and the runner refuse:
  - a batched step in an atomic migration;
  - SQL statements alongside one;
  - a one-shot step in a non-atomic migration.
- **One transaction per batch.** Each batch writes its rows, then records
  its count (`step`) and checkpoint in the ledger's new `checkpoint`
  column, and commits. A batch and its progress commit together or not at
  all, so:
  - **On a failure:** that batch rolls back whole, the migration is
    `partial`, and the earlier batches stay committed.
  - **On a rerun:** it resumes after the last committed batch, from its
    checkpoint. Every row is processed exactly once.
  - **On a crash during `COMMIT`:** the ledger tells the truth either way.
  - **When it finishes:** the last batch, which returns `None`, marks the
    migration `applied` in the same transaction.
- **Guards.**
  - A batch that returns the checkpoint it was given would loop forever, so
    it is stopped and recorded as `partial`.
  - As with non-atomic steps, a partly applied batched migration cannot be
    edited: its digest, which covers the step's source, is checked before
    it resumes.
  - Preconditions run before batch 1, and a fence goes up before it
    (ADR-017, ADR-019).
- **Upgrading.** Every `migrate` run adds the `checkpoint` column to an
  older ledger, the same way `step` was added.

## Evidence

`crates/trestle-historical/tests/batched.rs` (PostgreSQL):

- **A failure mid-batch.** Ten orders are slugged three per batch, and
  batch 3 fails partway through:
  - orders 1–6 are touched once and 7–10 not at all, so batch 3 rolled back
    whole;
  - the ledger is `partial` at batch 2 with order 6 as its checkpoint, and
    `plan` shows the retry;
- **The rerun.** After the data is fixed, the rerun processes 7–10:
  - every order is touched exactly once;
  - the ledger reads `applied` after 5 batches, with no checkpoint;
  - a third run does nothing.
- a batch that does not advance is stopped as `partial`;
- the placement rules (atomic, SQL alongside, a one-shot step in a
  non-atomic migration) are problems;
- a ledger without the column is upgraded, and the step runs.

Mutation checks, each caught:

- not recording the checkpoint;
- resuming from scratch;
- committing a failed batch;
- not adding the column.

## Consequences

- **Breaking (API):** a `DataStep` literal now needs
  `run: DataRun::Once(f)`, or `DataStep::once(name, source, f)`. Nothing
  downstream builds one.
- **No snapshot across batches.** Batches commit as they go, so a backfill
  sees writes made between its batches. Make the step idempotent per row,
  or select rows by state, such as `WHERE slug IS NULL`.
- **No cross-batch atomicity.** If the change must be all-or-nothing, use a
  one-shot step.
