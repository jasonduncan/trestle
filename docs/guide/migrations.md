# Migrations

Migrations live in `migrations/<app>/<id>.json` (the manifest) and
`<id>.sql`. `makemigrations` writes them from model changes. `migrate`
applies them in dependency order and records each one in the
`trestle_migrations` ledger. Applied migrations are immutable: if one
changes, its digest no longer matches the ledger and `migrate` refuses to
run.

## Atomic (default)

The SQL and the ledger's `applied` mark commit in one transaction, so a
failure rolls back cleanly and is safe to retry.

The default timeouts are `lock_timeout` 10s and `statement_timeout` 15min.
A manifest can override them for its own migration:

```json
{ "format": 2, "app": "notes", "id": "0007_backfill", "depends_on": ["notes.0006_x"],
  "operations": [], "lock_timeout": "2s", "statement_timeout": "30min" }
```

## Non-atomic (`"atomic": false`)

Some statements can't run in a transaction: `CREATE INDEX CONCURRENTLY`,
`DROP INDEX CONCURRENTLY`, and the like. They also don't block writes while
they run. Write them as a non-atomic migration, with one statement per step:

```sql
CREATE INDEX CONCURRENTLY IF NOT EXISTS notes_title_idx ON notes (title);
-- trestle:step
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS notes_slug_key ON notes (slug);
```

Rules. `check` and the runner both enforce them:

- One statement per step. Steps are separated by `-- trestle:step` lines.
- Each step must be idempotent and must run outside a transaction:
  - `CREATE INDEX CONCURRENTLY IF NOT EXISTS`
  - `DROP INDEX CONCURRENTLY IF EXISTS`
  - other `CONCURRENTLY` statements
  - `ALTER TABLE ... VALIDATE CONSTRAINT`
- Any other DDL goes in its own atomic migration.
- A non-atomic migration can't have a data step.
- Timeouts per step default to `lock_timeout` 5s and no `statement_timeout`
  (index builds take a while). The manifest can override both.

How a run proceeds:

- The ledger records the next step before and after each step. Each step
  commits on its own.
- If a step fails, the migration is marked `partial`, and `migrate` exits
  with the step number. `schema verify` reports the migration as partially
  applied.
- Fix the cause and run `migrate` again. It resumes at the failed step.
  Steps that already finished are not repeated.
- If the failed step was `CREATE INDEX CONCURRENTLY`, it may have left an
  INVALID index behind. `IF NOT EXISTS` would skip that index, so the runner
  drops it before retrying the step.
- A partly applied migration can't be edited. Its digest is checked before
  the run resumes.

Non-atomic migrations always need approval in automated deploys. See the
reconciler contract.

## Fencing older builds (`"fence": true`)

Expand-and-contract releases keep the previous build working while a new
one rolls out. Some changes can't: a column's unit or meaning changes, or
new code starts enforcing an invariant old code ignores. Old code would keep
writing without any error. Mark such a migration as a fence:

```json
{ "format": 2, "app": "orders", "id": "0007_amount_in_cents",
  "depends_on": ["orders.0006_x"], "operations": [], "fence": true }
```

Once it is applied, a build may write only if it contains it:

- **Refused writes.** A build whose history lacks the app's newest fence
  migration gets 503 `schema_fenced`, with `Retry-After: 1` and nothing
  written. This covers HTTP writes, `Registry::invoke` and job claims. Reads
  still work.
- **Readiness.** `/readyz` answers 503 `schema_fenced`, so the load balancer
  takes the old build out of rotation.
- **Startup.** `serve` refuses to start an old build, which also catches a
  rollback to an image that predates the fence.
- **Write pause.** While the migration runs, writes wait for it. It first
  waits for writes already under way, for up to its `lock_timeout`, 10s by
  default. Keep fence migrations short, and put a long backfill in a
  migration of its own.
- **Approval.** `migrate --plan` marks the step `"fence": true` with reason
  `fence`, so it always needs approval in automated deploys.
- **Inspecting.** `verify` lists the fences that shut the running build out.

The fence covers writes made through the executor. SQL a handler sends on
its own pool is not checked. Every `migrate` run creates the fence table and
function, so after upgrading Trestle, run `migrate` before `serve`. See
ADR-017.

