# Runbook: failure and recovery

Rehearsed by `scripts/drills/restore.sh` (evidence:
`docs/evidence/m5-restore.json`) and `scripts/drills/db-tls.sh`.

## Backups

- Take `pg_dump -Fc --no-owner` of the pilot database at least daily, and
  before every migration. Use managed point-in-time recovery where the
  hosting offers it.
- Store dumps encrypted, off the database host.
- Record the dump's size and SHA-256 with the release.
- Test a restore monthly: step 1 below, then discard the copy.

## Data damage (a bad bulk change, a faulty release)

Default: **restore to a new database, then repair forward.** Never restore
over the live database; that would discard every write since the backup.

1. **Restore a copy.**
   ```sh
   createdb pilot_restored
   pg_restore --no-owner --exit-on-error -d pilot_restored backup.dump
   DATABASE_URL=…/pilot_restored pilot schema verify     # catalog matches history
   DATABASE_URL=…/pilot_restored pilot-migrate --plan    # nothing pending
   ```
2. **Work out exactly which rows the incident changed.** Use the audit log
   and `updated_at`. Never assume every changed row was damaged: rows
   edited legitimately afterwards must not be overwritten.
3. **Write a repair script** that runs in one transaction (`psql -1 -v
   ON_ERROR_STOP=1`). It should:
   - copy the correct values from the restored copy (for example
     `\copy … TO` from the copy, then `\copy … FROM` into a temporary
     table);
   - update only rows whose sole change since the backup was the damage;
     the drill uses `version = backup_version + 1`;
   - bump `version`, so stale ETags get 412 and nobody overwrites the
     repair with an old form;
   - insert an audit event per row, with actor
     `00000000-0000-0000-0000-000000000000` (operator) and a repair
     operation ID.
4. **List the rows the repair skipped** and review them with the school.
5. Keep the restored copy until the repair is confirmed, then drop it.

## Bad migration

- **Expand migration failed half-way.** Each migration runs in its own
  transaction and is recorded in the ledger only on commit. Fix it with a
  new migration; never edit an applied one (its digest is checked).
- **A contract step removed data it should not have.** Treat it as data
  damage: restore a copy and repair forward.
- `pilot schema verify` compares the live catalog with the replayed
  history. Run it after any manual intervention.

## Code rollback

Allowed only while the schema supports the previous binary (expand-only
changes). Redeploy the previous binary; its required migrations are a
subset of those applied. The previous frontend bundle keeps working while
its form revisions are retained.

A fence migration (`"fence": true`, ADR-017) ends that window for every
binary that lacks it. Such a binary refuses to start (`serve` exits 6,
naming the migration), and one still running answers writes with 503
`schema_fenced`. Roll forward, or restore the database from before the fence
migration together with the old binary.

## Dependency outages

| Failure | Behaviour | Action |
|---|---|---|
| Database unreachable | `/readyz` 503; requests 503 `database_unavailable` after the 5 s pool timeout | Restore the database; replicas recover without a restart |
| Slow queries | 503 `database_timeout` after the statement timeout; the transaction rolls back | Find the query (`pg_stat_activity`); raise the timeout only with a reason |
| Deadlocks or serialization failures | 503 `database_conflict` with `Retry-After: 1`; PostgreSQL rolled the transaction back, so nothing was written and a retry is safe | If frequent, find the transactions that lock the same rows in different orders (the server log has PostgreSQL's message) |
| Commit outcome unknown (connection lost during COMMIT) | 503 `outcome_unknown`; the client must re-read before retrying | Check the audit log for the operation and `request_id` |
| Old replicas after a fence migration | Their `/readyz` is 503 `schema_fenced`; their writes are 503 `schema_fenced` with `Retry-After: 1` and write nothing; reads still answer | Finish the rollout. The load balancer drops them on readiness |
| Identity provider down | New logins get 503 `identity_unavailable`. Existing sessions keep working: they are server-side. Bearer tokens keep verifying with cached keys; if a refetch fails, the stale keys are kept | Wait, or fail over at the provider |
| Overload | 503 `overloaded`, with `Retry-After`; health is exempt | Add replicas or find the slow path |

## Security incidents

- **A password account is compromised:** `pilot account disable --email …`
  ends its sessions and voids its links. Once the person is verified,
  `enable`, then send a `reset-link`.
- **One person's sessions:** `pilot identity revoke --actor <id>`.
- **All sessions:**
  `UPDATE identity_sessions SET revoked_at = now() WHERE revoked_at IS NULL;`.
  Everyone signs in again.
- **Session key suspected leaked:** rotate `TRESTLE_SESSION_KEY` (CSRF
  tokens change) and revoke all sessions.
- **Cursor key leaked:** rotate it. Cursors carry no authority, but a
  forged cursor could skip pages of an authorized list.
- **Development tokens found in a production environment:** boot refuses
  them, so a running replica cannot have them. Check the deployment
  pipeline.
