# Runbook: release and rolling deploy

For the pilot on Trestle (design doc §16). Commands assume the repository
root. "Replica" means one `pilot serve` process behind the edge
(`deploy/Caddyfile`).

## 1. Release gate (before anything ships)

| Check | Command | Must be |
|---|---|---|
| Lint, all Rust and frontend tests | `make check` | green |
| Registry assembles offline | `cargo run -p pilot -- check` | no issues |
| Models match committed migrations | `cargo run -p pilot -- makemigrations --check` | "No changes" |
| Published contracts, descriptors, OpenAPI, SDK unchanged or intended | `cargo test -p pilot --test published` | green, and any diff reviewed |
| Every retained form revision is still accepted | same test | green |
| Dependencies | `cargo audit`, `cd frontend && npm audit --omit=dev` | no unreviewed advisories |
| Compatibility demonstration (for schema or form changes) | `node frontend/e2e/run-m5.mjs` | passes |

Record in the release notes:

- the commit;
- the digests of `contracts.json`, `interactions.json` and `openapi.json`
  (from `pilot contracts export`, `pilot interactions export` and
  `pilot openapi export`);
- the bundle digests;
- the migration heads (`pilot-migrate --plan`);
- the toolchain (`rust-toolchain.toml`, lockfiles).

## 2. Configuration

Production settings are listed in `deploy/pilot.env.example`. `pilot serve`
refuses to start in production, listing every problem at once, when any of
these holds:

- development tokens are set;
- there is no way to sign in: neither `TRESTLE_PASSWORD_LOGIN=true` nor an
  OIDC issuer with a client ID or an API audience;
- `TRESTLE_CURSOR_KEY` or `TRESTLE_SESSION_KEY` is missing or shorter than
  32 bytes;
- the issuer or the public origin is not `https`;
- the API audience equals the client ID;
- a non-local `DATABASE_URL` lacks `sslmode=require`, `verify-ca` or
  `verify-full`. Use `verify-full` with `sslrootcert`.

Use two database roles: the migration role owns the schema and runs only
`pilot-migrate`; the runtime role in `DATABASE_URL` needs DML on the
application tables only.

## 3. Rolling deploy of an additive change (expand, deploy, contract)

The M5 demonstration (`docs/evidence/m5-demo.json`) rehearses exactly
this sequence.

1. **Expand.** With the migration role:
   ```sh
   DATABASE_URL=… pilot-migrate --plan   # review: only additive steps
   DATABASE_URL=… pilot-migrate
   ```
   - Old replicas keep serving: they select and insert explicit columns
     and check only their own required migrations.
   - Nullable columns without defaults are metadata-only in PostgreSQL.
   - A `CHECK` on a new column scans the table under an exclusive lock.
     That is fine at pilot size; for large tables, add the check
     `NOT VALID`, then `VALIDATE CONSTRAINT` in a later migration.
2. **Deploy replicas one at a time.**
   - A new replica only starts if every migration it requires is applied.
   - The edge adds it once `/readyz` passes.
   - Stop an old replica with SIGTERM: it stops accepting and drains
     in-flight requests. (The M4 binary did not handle SIGTERM and was
     killed outright; M5 drains.)
3. **Publish the new frontend bundles.**
   - Hashed assets are immutable.
   - The entry document is `no-cache`, so new page loads pick up the new
     bundle. Open tabs keep the old bundle, which keeps working (see the
     compatibility matrix).
4. **Watch during and after the rollout** (`/metrics` on each replica):
   - `trestle_interaction_stale_total`: new form revisions landing on old
     replicas. Expected during the overlap and should fall to zero.
     Clients recover by reloading the form, with nothing written.
   - `trestle_http_requests_total{status="5xx"}` and
     `trestle_operation_total{outcome="internal"}` should stay flat.
   - `412`/`409` rates (`precondition_failed`, `stale_version`) should
     stay near baseline.
5. **Contract** (drop or tighten) only after every client that could send
   the old shape is gone:
   - after the compatibility window, and after retained form revisions
     have been removed from `retain(..)`;
   - as its own release, with the planner's `--allow-destructive`
     reviewed.

Skew is mostly avoided if the edge uses sticky routing during the overlap;
it is survived without it (demonstrated).

## 4. Rollback conditions

- **Roll back the code** (redeploy the previous binary) only while the
  active schema still supports it, which is true after an expand-only
  migration. The M4 binary served correctly against the M5 schema in the
  demonstration.
- **Do not roll back the database.** Irreversible or damaging changes are
  fixed forward (`docs/runbooks/recover.md`).
- **Roll back the frontend** by pointing the entry document at the previous
  bundle. Its form revisions are still accepted, provided they are retained.

Trigger a rollback of the code when any of these holds:

- 5xx or `internal` outcomes rise above baseline after the new replicas
  take traffic;
- `/readyz` fails on new replicas;
- p95 on a core route regresses by more than 50% for 10 minutes;
- a security regression is found.
