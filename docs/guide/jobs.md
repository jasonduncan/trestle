# Jobs: work after commit

A job is work that must happen after an operation commits: send an email,
call another service, or run an operation later. It is written in the same
transaction as the operation's other writes, and a worker runs it afterwards.
Design and guarantees: [ADR-015](../adr/015-durable-jobs.md).

## Set up

Install the job table and generate its migration:

```rust
trestle::project()
    .install_app(trestle::core::jobs::JobsApp)
    // ... your apps
```

```sh
myapp makemigrations   # adds jobs/0001_*.json and .sql; review and commit
myapp migrate
```

Run a worker in the server process with `myapp serve --jobs` (or
`TRESTLE_JOBS=1`; `TRESTLE_JOB_CONCURRENCY` defaults to 8). To run workers
in their own processes, use `trestle::core::jobs::Worker`.

## Enqueue from an operation

Use the connection the operation was given. The job then exists only if
the operation commits:

```rust
async fn run(&self, conn: &mut PgConnection, ctx: &TenantContext, input: Invite) -> Result<View, Problem> {
    let invite = repository::create_invite(conn, ctx, &input).await?;
    let me = Invocation::for_actor(ctx.actor).with_tenant(ctx.school.0);
    jobs::enqueue(
        conn,
        NewJob::handler("schools.send_invite", json!({ "invite": invite.id }))
            .on_behalf_of(&me)
            .dedupe_key(format!("invite:{}", invite.id)),
    )
    .await?;
    Ok(invite.into())
}
```

- `NewJob::operation(id, input)` runs a registered operation later, as the
  recorded actor and tenant. It is re-authorized on every attempt: if the
  actor has lost access, the job fails and writes nothing.
- `NewJob::handler(kind, input)` runs a handler you register with
  `app.job("schools.send_invite", SendInvite)`.
- Builders:
  - `.after(duration)` delays the job;
  - `.dedupe_key(key)` makes enqueueing idempotent;
  - `.max_attempts(n)` sets the retry limit (default 5);
  - `.payload_version(n)` versions the input format.

## Write a handler

```rust
struct SendInvite;

impl JobHandler<AppState> for SendInvite {
    fn run<'a>(&'a self, state: &'a AppState, job: &'a JobRun<'a, AppState>) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async move {
            // Database reads/writes go through operations, re-authorized:
            let invite = job.invoke(state, "schools.invite_details", &job.input).await?;
            // External calls carry the job's idempotency key (`provider` is
            // your client for the service; most email and payment APIs take one).
            state.provider.send(&message(&invite), &job.idempotency_key()).await
                .map_err(|_| Problem::unavailable("provider_unavailable"))
        })
    }
}
```

- **At least once.** An attempt can repeat after a crash. Make handlers
  idempotent, and pass `job.idempotency_key()`, which is the same on every
  attempt, to providers that deduplicate.
- **Errors decide retries.**
  - A client error (4xx) fails the job at once.
  - Anything else retries with backoff (2 s, 4 s, 8 s, … at most an hour)
    until `max_attempts`.
  - Only the problem code is recorded.
- **No secrets in input.** The input is stored in plain text until purged.
  Store an ID, and look up or generate the secret when the handler runs.
- **Payload versions.** When the input format changes, bump
  `payload_version` and keep reading the old version until those jobs have
  drained.

## Operate

- **Status.** `jobs::status(conn, id)` gives the status (`ready`, `running`,
  `done` or `failed`), the attempts so far, and the last error code.
- **Purging.** Delete old rows with `jobs::purge_finished(conn,
  Duration::from_secs(30 * 86400))`, from an operator command or a scheduled
  job.
- **Metrics:**
  - `trestle_jobs_total{job, outcome}`, where outcome is
    `done`/`retry`/`failed`/`stale`;
  - `trestle_job_lag_seconds`;
  - `trestle_jobs_expired_total`.
- **Shutdown.** The worker stops claiming and lets running jobs finish for
  up to 20 s. Anything still running then is retried once its lease (60 s)
  expires.
