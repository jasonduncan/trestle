# ADR-015: Durable jobs with transactional enqueue and fenced claims

**Status:** Proposed (2026-10-17) · **Design doc:** §6.3, §15.1–§15.3, §19.2 B ·
**Reopens:** ADR-009 · **Depends on:** ADR-013 (`Invocation`, `Registry::invoke`)

## Context

ADR-009 deferred jobs and an outbox until a feature needed an external
effect. It set the bar for building them:

- enqueue inside the operation's unit of work;
- claim tokens that fence completion;
- a bounded drain on shutdown;
- authority re-evaluated at execution time;
- crash, duplicate and lease tests.

Request-time email (ADR-011) and any after-commit work are blocked on it, and
§6.3 forbids putting mail, payments or webhooks inside a database
transaction.

## Decision

- **Table and app.** `trestle_core::jobs::JobsApp` installs model `jobs.job`
  (`trestle_jobs`) through the normal model and migration path. It records
  target and name, payload version, input (JSON text, at most 64 KiB), actor
  and tenant IDs, an optional dedupe key, status
  (`ready`/`running`/`done`/`failed`), attempts and limit, `run_after`, claim
  token, lease, the last error code, and timestamps. It never stores
  credentials.
- **Transactional enqueue.** `jobs::enqueue(conn, NewJob)` runs on the
  executor's connection inside an operation's `run`, so a job exists exactly
  when the operation's writes commit. `dedupe_key` makes enqueueing
  idempotent per target.
- **Two targets.**
  - `NewJob::operation(id, input)` is a deferred `Registry::invoke` under the
    recorded `Invocation`. Context resolution, contract validation,
    `admit`/`recheck`, the executor and the output check all run again on
    each attempt, so authority is re-evaluated by construction (§19.2 B).
  - `NewJob::handler(kind, input)` runs a `JobHandler` registered with
    `AppBuilder::job(kind, ..)`, namespaced like operations and checked at
    assembly. It is for external effects. A handler does its database work
    through `JobRun::invoke`, which re-authorizes; a handler that needs
    service authority instead must document it.
- **Claims and fencing.**
  - The worker claims due jobs, and running jobs whose lease expired with
    attempts left, in one `UPDATE … FOR UPDATE SKIP LOCKED`. Each claim gets
    a new claim token, increments `attempts` and sets a lease.
  - Every outcome (`done`, retry, `failed`) is written
    `WHERE id = $1 AND claim = $2 AND status = 'running'`, so a stale worker
    cannot overwrite a newer attempt.
  - An attempt is cut off at 90% of its lease, so normally only one worker
    runs a job at a time.
- **Delivery and retries.**
  - Delivery is at least once. `JobRun::idempotency_key()` (the job ID) is the
    same on every attempt, for providers that deduplicate.
  - Client errors (4xx: invalid input, forbidden, not found, unknown target)
    fail without retrying.
  - Other errors retry with exponential backoff (2 s doubling, at most an
    hour) up to `max_attempts` (default 5).
  - A lease that expires with no attempts left is swept to `failed`
    (`lease_expired`).
  - Only the problem code is kept as the last error.
- **Shutdown.** `Worker::run(shutdown)` stops claiming when `shutdown`
  resolves and lets running jobs finish for up to `drain` (20 s by default).
  It then abandons the rest to their leases, so another worker runs them
  again.
- **Running it.** `serve --jobs` (`TRESTLE_JOBS=1`) runs a worker in the
  server process, sharing its pool and state, and stops it on the same
  signal. It refuses to start if `JobsApp` is not installed. `Worker` and
  `Worker::run_once` are public, for separate worker processes and tests.
- **Retention.** `jobs::purge_finished(conn, older_than)` deletes done and
  failed jobs. Nothing purges on its own.
- **Metrics.**
  - `trestle_jobs_total{job, outcome}`: `job` is a registered operation ID or
    handler kind, so the label set is bounded.
  - `trestle_job_lag_seconds`: from due to claimed.
  - `trestle_jobs_expired_total`.

## Evidence

`crates/trestle-core/tests/jobs.rs` runs against real PostgreSQL:

- **Enqueue:** a job enqueued by an operation that then rolls back does not
  exist; on commit, it and the write exist together.
- **Authority:** an operation job writes as the enqueuing actor. After that
  actor loses membership, the next one fails with `tenant_not_permitted` and
  writes nothing.
- **Retries:** a handler failing twice retries, waits out its backoff, then
  succeeds; all three attempts carry the same idempotency key. Attempts are
  bounded, and a client error and an unknown handler fail after one attempt.
- **Fencing:** worker A is held mid-attempt while its lease expires; worker B
  reclaims and finishes; A's result is refused as stale. The job is done,
  with 2 attempts.
- **Crash:** a worker that performs the effect and dies is re-run after its
  lease expires (at least once, same key). A lease that expires with no
  attempts left is swept to `failed`.
- **Shutdown:** stopping claims nothing new and drains the running job. A
  drain that times out leaves the job to its lease, and it is recovered.
- **Other:** a dedupe key enqueues once; purging removes only finished jobs
  past retention; job kinds must be namespaced and unique.

Six consecutive runs pass. Mutation checks:

- dropping the claim-token fence fails the stale-claim test;
- retrying client errors fails both authority tests;
- removing the attempt bound from the claim query is **not** caught, because
  the sweep that runs first already fails exhausted jobs. The bound only
  guards a lease that expires between sweep and claim. It stays, with a
  comment.

`serve` refuses `--jobs` without `JobsApp` (unit test).

## Consequences

- Operations can now cause after-commit work without `tokio::spawn`.
  Detached tasks for application work are still a review finding.
- Job inputs are stored in plain text until purged. Never put credentials,
  tokens or sign-in link secrets in them; derive such values when the
  handler runs.
- The `serve --jobs` process is not covered by an end-to-end test; the
  worker it starts is the one the tests exercise.
- **Not in this change:**
  - `Idempotency-Key` receipts for HTTP requests (§6.4);
  - request-time mail through a job handler (ADR-011);
  - a `jobs` operator command (list, retry, purge);
  - letting `trestle.toml` handlers enqueue.
