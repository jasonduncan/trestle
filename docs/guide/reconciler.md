# Deploying with a reconciler

These are the commands, JSON output and exit codes an automated deployer
(for example Commons) relies on. Both the JSON schemas and the exit codes
are versioned and kept compatible.

The flow is **plan, approve, snapshot, apply, verify**. Migrations only go
forward: to recover, restore a backup or ship a forward fix.

## 1. Plan

`myproj migrate --plan --json` changes nothing and prints `trestle.plan/1`:

```json
{"schema": "trestle.plan/1", "project": "myproj", "trestle_version": "0.1.0",
 "database": {"ledger_present": true, "applied": 12},
 "plan_digest": "sha256:…",
 "steps": [{"app": "notes", "id": "0002_title_idx", "digest": "sha256:…",
            "atomic": false, "risk": "additive", "has_data_step": false,
            "statements": 1, "status": "pending", "class": "approval",
            "requires_approval": true, "reasons": ["non_atomic", "hand_sql"]}],
 "requires_approval": true, "class": "approval", "blocked": []}
```

- `status` is one of:
  - `pending`
  - `retry`: an earlier attempt rolled back
  - `partial`: a non-atomic migration stopped part-way; `resume_step` says
    which step runs next
- Anything in `blocked` stops the plan, and the command exits 1. Examples:
  an applied migration was changed, the database has applied migrations
  this build doesn't know, a data step is missing, or a non-atomic
  migration breaks the rules.

## 2. Classify

A step is `auto` only if all of these hold:

- it is atomic
- it has no data step
- its SQL is exactly what `makemigrations` renders from its operations
- every operation is on the allowlist:
  - create a table
  - add a nullable column, or a NOT NULL column with a constant default
  - add an index, unique constraint, foreign key or check on a table
    created in the same migration

Anything else needs approval. These are the reason codes:

| Reason | Meaning |
|---|---|
| `destructive` | drops or renames something |
| `non_atomic` | `"atomic": false` |
| `data_step` | runs a data step or backfill |
| `hand_sql` | the SQL differs from what the planner renders, or there are no operations to check it against |
| `rewrite_risk` | volatile default, NOT NULL without a default, or a nullability change |
| `existing_table_lock` | a new index, unique constraint, foreign key or check on an existing table |
| `fence` | `"fence": true`: pauses writes while it runs, then refuses writes from builds without it (ADR-017) |

## 3. Apply

The behaviour depends on the command and the environment:

- `migrate --approve sha256:…` applies exactly the approved plan. If the
  database has moved since, so the plan differs, it exits 3 and runs nothing.
- `migrate --auto` applies only if every step is `auto`. Otherwise it exits
  3 and prints the digest to approve.
- Plain `migrate` with `TRESTLE_ENV=production` behaves like `--auto`.
- In development and test, plain `migrate` applies everything.

Other options:

- `--backup-ref ID` records the snapshot taken before the run in the ledger,
  for audit. Trestle does not take snapshots itself.
- `--lock-timeout 30s` bounds how long to wait for another `migrate`'s lock.
- `--json` prints one line per event (`started`, `applied`, `failed`), then
  a `trestle.migrate/1` summary.

## 4. Verify

`myproj verify --json` prints:

```json
{"schema": "trestle.verify/1", "ok": true, "drift": [], "partial": [],
 "unknown_applied": [], "pending": [], "unverified": []}
```

- It compares the ledger with committed history, and the database catalog
  with the schema the applied migrations should have produced.
- It exits 6 on drift, on a partly applied migration, or when the database
  has applied migrations this build doesn't know.
- If the database is ahead of this build, the catalog is not compared.

`serve` refuses to start (exit 6) on drift or a partly applied migration.
A database ahead of the build is allowed, because rolling deploys need it.

### Foreign apps

When an app moves out of a project into another one that keeps using the
same database, its ledger rows would otherwise look like migrations "from a
newer release" and `migrate` would refuse to run. Declare them:

```rust
project.foreign_apps(["smartfolder"])
```

Rows of foreign apps are skipped by `migrate`, `migrate --plan` and
`verify`; `verify --json` lists them under `foreign` (omitted when empty).
An app the project still has migrations for cannot be foreign. The receiving
project keeps the moved migration files byte-for-byte, so their digests (and
the ledger rows) still match there.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | success / nothing to do |
| 1 | check failed, changes pending, or plan blocked |
| 2 | usage or configuration error |
| 3 | plan needs approval, or differs from the approved digest |
| 4 | migration failed and rolled back cleanly; safe to retry |
| 5 | migration failed in a partial or unknown state; needs a human |
| 6 | verify found drift |
| 7 | could not get the lock within `--lock-timeout` |
| 10 | cannot reach the database |
