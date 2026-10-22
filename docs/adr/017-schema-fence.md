# ADR-017: A schema fence for writes from older builds

**Status:** Accepted (2026-10-22; proposed 2026-10-17) · **Design doc:** §14.4, §16 · **Amends:**
ADR-006 · **Depends on:** ADR-015

## Context

§14.4 asks for expand-and-contract releases. Startup checks that a build's
required migrations are applied, and it allows additive changes to coexist
with old binaries. Nothing stops an old binary once a newer release has
changed the schema under it:

- **Natural errors.** A dropped column makes an old write fail, which is
  safe.
- **Silent damage.** Some changes produce no error: a column's unit or
  meaning changes (dollars become cents), or an invariant only the new code
  enforces. An old replica that is still serving during a rolling deploy,
  or a rollback to an old image, keeps writing data that is wrong under the
  new meaning.

The design doc's release step needs a way to say "from this migration on,
older builds must not write".

## Decision

- **Fence migrations.** A manifest may say `"fence": true`. The field is
  omitted when false, so existing manifests and digests are unchanged.
  Applying such a migration records it in `trestle_schema_fence`, one row
  per app (`app`, newest fence `migration`, `generation` count,
  `raised_at`).
- **Which builds are fenced.** A build may write only if it contains its
  app's newest fence migration:
  - The build's fence (`BuildFence::of(set)`) is the apps in its history
    plus every fence migration it knows.
  - Foreign apps are not its apps, so their fences don't apply to it.
  - Additive and destructive migrations don't fence by default, because an
    expand-and-contract release keeps the previous build compatible. The
    author marks a fence when it doesn't.
- **Write path.** `trestle serve` installs the build's fence
  (`trestle_core::fence::install`), which is process-wide. Every write
  transaction the executor begins then sends
  `SELECT trestle_fence_v1(apps, known); BEGIN` as one simple-query message:
  - **No extra round trip:** the check travels with `BEGIN`.
  - **The lock lasts:** the check takes `ACCESS SHARE` on the fence table.
    The check runs in an implicit transaction, which `BEGIN` turns into the
    write's own, so the lock is held until the write ends.
  - **A refusal leaves nothing behind:** the check raises SQLSTATE `TRF01`
    before `BEGIN`. PostgreSQL rolls back the implicit transaction and the
    connection returns to the pool idle. If `BEGIN` came first, sqlx would
    hand back a connection stuck in an aborted transaction.
  - **The refusal** maps to 503 `schema_fenced` with `Retry-After: 1`.
    Nothing ran, so a retry is safe, and a load balancer can send it to a
    current server.
- **Migration side.** A fence migration's first statement is
  `LOCK TABLE trestle_schema_fence IN ACCESS EXCLUSIVE MODE`, under the
  migration's `lock_timeout`. After its SQL and data step, it records itself
  in the same transaction.
  - Writes already past the check finish first, against the old schema.
  - Writes that arrive meanwhile wait. When they resume, the check function
    takes a new statement snapshot after its own `LOCK` (READ COMMITTED), so
    it sees the new fence: old builds are refused and current ones proceed.
  - Writes pause only while a fence migration runs.
  - Locking before any DDL avoids a deadlock with a write that holds the
    fence and waits for a table the migration locks.
  - A non-atomic fence migration raises its fence in a short transaction
    before its first step. Resuming it doesn't raise the fence again.
- **The fence also covers:**
  - **Job claims**, which run in a fenced transaction: a fenced worker
    claims nothing and spends no attempts.
  - **`/readyz`**, which answers 503 `schema_fenced` so the build drops out
    of rotation.
  - **`serve`**, which refuses to start when the fence shuts this build out
    (exit 6) or when the database has no fence yet (exit 1, "run
    `migrate`").
  - **`verify`**, which lists `fenced_by`.
  - **`trestle.plan/1`**, which marks `fence: true` with reason `fence`, so
    a fence migration always needs approval.
- **Objects.** Every `migrate` run creates the table and the function
  `trestle_fence_v1(text[], text[])`, like the ledger. The version is in the
  function's name, so a later change adds a new function and older builds
  keep calling theirs. The application role needs `SELECT` on the table, as
  it already does on the ledger for readiness.

## Evidence

`crates/trestle-historical/tests/fence.rs` (PostgreSQL):

- fence migrations record their app and count generations, and ordinary
  ones leave the fence alone;
- manifests without the field serialize without it;
- older builds are refused before `BEGIN` (`TRF01`), newer and foreign-app
  builds pass, and the refused connection runs the next transaction;
- **a write already past the check holds back the fence migration** until
  it commits. The migration then converts that write's value: the old write
  landed entirely before the change;
- **a write that arrives while a fence migration holds the lock waits.** It
  is then refused if old and admitted if current. Only the current build's
  row exists, unconverted;
- a fence migration that cannot get its lock within `lock_timeout` fails
  and records no fence. The retry succeeds;
- a non-atomic fence migration fences before step 1, and resuming after a
  failed step records it once.

`crates/trestle-core/tests/fence.rs` runs on a one-connection pool. After
an old build's fence migration is applied, all of these refuse with
`schema_fenced` and write nothing:

- HTTP writes;
- `Registry::invoke` writes;
- job claims;
- `/readyz`.

Reads still answer. Installing the current build's fence on the same pool
lets writes and the queued job through.

`crates/trestle/tests/reconcile.rs`: the plan shows the fence, `serve`
refuses the old build (exit 6) and a fence-less database (exit 1), and
`verify` names the fencing migration.

Mutation checks, each confirmed to fail a test:

- no lock in the migration;
- the check inside an explicit `BEGIN`;
- releasing the check's lock before `BEGIN`;
- skipping the non-atomic fence;
- the executor, job claims or readiness ignoring the fence;
- not excluding foreign apps.

Load (`scripts/load/compare.sh 15 3`, U4b and then this branch on the same
machine, no errors in either run):

| Create | U4b | U5 |
|---|---|---|
| p95 overhead vs. the matched Axum baseline, concurrency 8 | 3.9% | 8.1% |
| p95 overhead, concurrency 32 | 7.0% | 9.8% |
| Trestle p50, concurrency 8 | 1.07 ms | 1.13 ms |

The check's cost is about 0.06 ms of server time per write. Reads don't
change beyond noise: get at concurrency 8 moved 12 points between the two
runs, with no read code changed.

Removing the explicit `LOCK` from the check function is **not** caught. On
PostgreSQL 16 the lookup's own lock is taken before its snapshot, so it sees
the new fence anyway. The explicit lock keeps that from depending on when
the planner takes locks.

## Consequences

- **Scope.** Only writes through the executor and job claims are fenced.
  SQL a handler sends on its own pool, and identity's session writes, are
  not. Reads go on, so a fenced replica can still serve stale meaning until
  readiness takes it out.
- **Isolation.** The guarantee assumes READ COMMITTED, PostgreSQL's
  default, which the executor uses. Under REPEATABLE READ the snapshot
  predates the wait.
- **Write pause.** A fence migration pauses writes for as long as it runs,
  plus the wait for in-flight writes, bounded by its `lock_timeout`. Keep
  fence migrations short, and put long backfills in their own migration.
- **Upgrading.** After upgrading Trestle, run `migrate` once before `serve`
  to create the fence. The documented order already runs `migrate` first.
- **Tests and other callers.** `TestApp` and callers that never install a
  fence are unchecked, as before.
