//! Durable jobs and the transactional outbox (ADR-015, design doc §6.3,
//! §15.2).
//!
//! A job is work that must happen after a transaction commits: a deferred
//! operation, or an external effect such as sending mail. It is written with
//! [`enqueue`] on the operation's own connection, so it exists exactly when
//! the operation's writes do. A [`Worker`] runs it later:
//!
//! ```text
//! claim (FOR UPDATE SKIP LOCKED, new claim token, lease)
//!   -> operation target: Registry::invoke with the recorded Invocation
//!      handler target:   the registered JobHandler
//!   -> done / retry with backoff / failed, fenced by the claim token
//! ```
//!
//! Delivery is at least once: a worker that dies after the work but before
//! recording it lets the lease expire, and the job runs again. Operation
//! targets are re-authorized on every attempt (their context is resolved and
//! their policy admits and rechecks, as for any caller). A job stores the
//! actor and tenant, never credentials.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRef;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row as _};
use tokio::task::JoinSet;
use trestle_historical::schema::{Column, Model};
use uuid::Uuid;

use crate::Problem;
use crate::identity::ActorId;
use crate::invocation::Invocation;
use crate::operation::BoxFuture;
use crate::registry::{App, AppBuilder, Registry};

/// The job table.
pub const TABLE: &str = "trestle_jobs";
/// Largest serialized input a job may carry.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Default attempts before a job is marked failed.
pub const DEFAULT_ATTEMPTS: i32 = 5;

/// The job table's model (`jobs.job`). Install [`JobsApp`] to add it to a
/// project's desired schema; `makemigrations` then creates the migration.
pub fn model() -> Model {
    Model::new("jobs.job")
        .table(TABLE)
        .field(Column::uuid("id").primary_key())
        .field(Column::text("target").one_of(&["operation", "handler"]))
        .field(Column::text("name").chars(1, 200))
        .field(
            Column::integer("payload_version")
                .at_least(1)
                .default_int(1),
        )
        .field(Column::text("input"))
        .field(Column::uuid("actor_id").nullable())
        .field(Column::uuid("tenant_id").nullable())
        .field(Column::text("dedupe_key").chars(1, 200).nullable())
        .field(
            Column::text("status")
                .one_of(&["ready", "running", "done", "failed"])
                .default_text("ready"),
        )
        .field(Column::integer("attempts").at_least(0).default_int(0))
        .field(
            Column::integer("max_attempts")
                .at_least(1)
                .default_int(i64::from(DEFAULT_ATTEMPTS)),
        )
        .field(Column::timestamptz("run_after").default_now())
        .field(Column::uuid("claim").nullable())
        .field(Column::timestamptz("lease_until").nullable())
        .field(Column::text("last_error").chars(0, 200).nullable())
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("finished_at").nullable())
        .unique("trestle_jobs_dedupe_key", &["target", "name", "dedupe_key"])
        .index("trestle_jobs_due_idx", &["status", "run_after"])
}

/// Installs the job table. Registers no operations or routes.
pub struct JobsApp;

impl<S: 'static> App<S> for JobsApp {
    fn id(&self) -> &str {
        "jobs"
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        app.model(model());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(pub Uuid);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Operation,
    Handler,
}

impl Target {
    fn as_str(self) -> &'static str {
        match self {
            Target::Operation => "operation",
            Target::Handler => "handler",
        }
    }
}

/// Work to run after the current transaction commits.
#[derive(Debug, Clone)]
pub struct NewJob {
    target: Target,
    name: String,
    payload_version: i32,
    input: Value,
    invocation: Invocation,
    delay: Duration,
    dedupe_key: Option<String>,
    max_attempts: i32,
}

impl NewJob {
    /// Invokes the registered operation `id` with `input`, re-authorized
    /// when it runs.
    pub fn operation(id: impl Into<String>, input: Value) -> Self {
        Self::new(Target::Operation, id.into(), input)
    }

    /// Runs the [`JobHandler`] registered as `kind` with `input`.
    pub fn handler(kind: impl Into<String>, input: Value) -> Self {
        Self::new(Target::Handler, kind.into(), input)
    }

    fn new(target: Target, name: String, input: Value) -> Self {
        Self {
            target,
            name,
            payload_version: 1,
            input,
            invocation: Invocation::anonymous(),
            delay: Duration::ZERO,
            dedupe_key: None,
            max_attempts: DEFAULT_ATTEMPTS,
        }
    }

    /// Whose authority the job runs under: its actor and tenant are stored
    /// and the job is resolved from them on every attempt.
    pub fn on_behalf_of(mut self, invocation: &Invocation) -> Self {
        self.invocation = invocation.clone();
        self
    }

    /// Not before `delay` from now.
    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// At most one job per target and key: enqueueing the same key again
    /// returns the existing job.
    pub fn dedupe_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    /// The input's format version, so a handler can keep reading old
    /// payloads until they drain (§15.2). Defaults to 1.
    pub fn payload_version(mut self, version: i32) -> Self {
        self.payload_version = version;
        self
    }

    pub fn max_attempts(mut self, attempts: i32) -> Self {
        self.max_attempts = attempts;
        self
    }
}

/// Writes `job` on `conn`, normally the executor's transaction inside an
/// operation's `run`: the job exists only if that transaction commits.
pub async fn enqueue(conn: &mut PgConnection, job: NewJob) -> Result<JobId, Problem> {
    crate::effects::check_emit(&job.name)?;
    let input = job.input.to_string();
    if input.len() > MAX_INPUT_BYTES {
        tracing::error!(job = %job.name, bytes = input.len(), "job input is too large");
        return Err(Problem::internal());
    }
    if job.max_attempts < 1 || job.payload_version < 1 {
        tracing::error!(job = %job.name, "job attempts and payload version must be at least 1");
        return Err(Problem::internal());
    }
    let id: Option<Uuid> = sqlx::query_scalar(&format!(
        "INSERT INTO {TABLE} (id, target, name, payload_version, input, actor_id, tenant_id,
                              dedupe_key, max_attempts, run_after)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now() + make_interval(secs => $10))
         ON CONFLICT (target, name, dedupe_key) DO NOTHING
         RETURNING id"
    ))
    .bind(Uuid::now_v7())
    .bind(job.target.as_str())
    .bind(&job.name)
    .bind(job.payload_version)
    .bind(&input)
    .bind(job.invocation.actor().ok().map(|a| a.0))
    .bind(job.invocation.tenant())
    .bind(&job.dedupe_key)
    .bind(job.max_attempts)
    .bind(job.delay.as_secs_f64())
    .fetch_optional(&mut *conn)
    .await?;
    let id = match id {
        Some(id) => id,
        None => {
            sqlx::query_scalar(&format!(
                "SELECT id FROM {TABLE} WHERE target = $1 AND name = $2 AND dedupe_key = $3"
            ))
            .bind(job.target.as_str())
            .bind(&job.name)
            .bind(&job.dedupe_key)
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(JobId(id))
}

/// Where a job stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobStatus {
    pub status: String,
    pub attempts: i32,
    /// The last failure's problem code; never a value or message.
    pub last_error: Option<String>,
}

pub async fn status(conn: &mut PgConnection, id: JobId) -> Result<Option<JobStatus>, Problem> {
    let row = sqlx::query(&format!(
        "SELECT status, attempts, last_error FROM {TABLE} WHERE id = $1"
    ))
    .bind(id.0)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|r| JobStatus {
        status: r.get(0),
        attempts: r.get(1),
        last_error: r.get(2),
    }))
}

/// Deletes done and failed jobs that finished more than `older_than` ago,
/// returning how many. Ready and running jobs are never touched. Run it from
/// an operator command or a scheduled job; nothing purges on its own.
pub async fn purge_finished(conn: &mut PgConnection, older_than: Duration) -> Result<u64, Problem> {
    let purged = sqlx::query(&format!(
        "DELETE FROM {TABLE}
          WHERE status IN ('done', 'failed')
            AND finished_at < now() - make_interval(secs => $1)"
    ))
    .bind(older_than.as_secs_f64())
    .execute(conn)
    .await?
    .rows_affected();
    Ok(purged)
}

/// One attempt of a handler job.
pub struct JobRun<'a, S> {
    pub id: JobId,
    /// 1 for the first attempt.
    pub attempt: i32,
    pub payload_version: i32,
    pub input: Value,
    invocation: Invocation,
    registry: &'a Registry<S>,
}

impl<S> JobRun<'_, S> {
    /// The authority the job was enqueued under.
    pub fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    /// The same for every attempt: pass it to providers that deduplicate
    /// (an email API's idempotency key), since an attempt can repeat.
    pub fn idempotency_key(&self) -> String {
        self.id.to_string()
    }

    /// Runs operation `id` under the job's authority (re-authorized now).
    pub async fn invoke(&self, state: &S, id: &str, input: &Value) -> Result<Value, Problem> {
        self.registry
            .invoke(state, id, &self.invocation, input)
            .await
    }
}

/// Runs handler jobs of one kind. At least once: an attempt can repeat
/// after a crash, so external calls carry [`JobRun::idempotency_key`].
/// Database work goes through [`JobRun::invoke`], which re-authorizes; a
/// handler that needs service authority instead says so in its docs.
pub trait JobHandler<S>: Send + Sync + 'static {
    fn run<'a>(
        &'a self,
        state: &'a S,
        job: &'a JobRun<'a, S>,
    ) -> BoxFuture<'a, Result<(), Problem>>;
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Jobs running at once in [`Worker::run`].
    pub concurrency: usize,
    /// How long a claim lasts. An attempt is cut off before its lease ends.
    pub lease: Duration,
    /// Wait between polls when there was nothing to claim.
    pub poll: Duration,
    /// On shutdown, how long running jobs may finish before they are
    /// abandoned to lease expiry.
    pub drain: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: 8,
            lease: Duration::from_secs(60),
            poll: Duration::from_secs(1),
            drain: Duration::from_secs(20),
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pass {
    pub claimed: usize,
    pub done: usize,
    pub retried: usize,
    pub failed: usize,
    /// Results refused because a newer claim owns the job.
    pub stale: usize,
    /// Expired leases with no attempts left, marked failed.
    pub expired: usize,
}

struct Claimed {
    id: Uuid,
    claim: Uuid,
    target: String,
    name: String,
    payload_version: i32,
    input: String,
    actor: Option<Uuid>,
    tenant: Option<Uuid>,
    attempts: i32,
    max_attempts: i32,
}

enum Outcome {
    Done,
    Retry(String),
    Failed(String),
    Stale,
}

struct Inner<S> {
    registry: Arc<Registry<S>>,
    state: S,
    pool: PgPool,
    config: WorkerConfig,
}

/// Claims and runs jobs. Any number of workers may share one table.
pub struct Worker<S> {
    inner: Arc<Inner<S>>,
}

impl<S> Clone for Worker<S> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<S> Worker<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
{
    pub fn new(registry: Arc<Registry<S>>, state: S, config: WorkerConfig) -> Self {
        let pool = PgPool::from_ref(&state);
        Self {
            inner: Arc::new(Inner {
                registry,
                state,
                pool,
                config,
            }),
        }
    }

    /// Claims up to `limit` due jobs and runs them to an outcome. For tests,
    /// operator commands and custom schedulers; [`Worker::run`] loops this.
    pub async fn run_once(&self, limit: usize) -> Result<Pass, Problem> {
        let mut pass = Pass {
            expired: self.sweep().await?,
            ..Pass::default()
        };
        let claimed = self.claim(limit).await?;
        pass.claimed = claimed.len();
        let mut running = JoinSet::new();
        for job in claimed {
            let worker = self.clone();
            running.spawn(async move { worker.attempt(job).await });
        }
        while let Some(outcome) = running.join_next().await {
            count(&mut pass, outcome);
        }
        Ok(pass)
    }

    /// Runs until `shutdown` resolves, then stops claiming and lets running
    /// jobs finish for up to `drain`. Jobs still running after that are
    /// abandoned; their leases expire and another worker runs them again.
    pub async fn run(self, shutdown: impl std::future::Future<Output = ()>) -> Pass {
        let mut total = Pass::default();
        let mut running: JoinSet<Outcome> = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            while let Some(done) = running.try_join_next() {
                count(&mut total, done);
            }
            let free = self.inner.config.concurrency.saturating_sub(running.len());
            let mut claimed = 0;
            if free > 0 {
                match self.sweep().await {
                    Ok(n) => total.expired += n,
                    Err(_) => tracing::warn!("job sweep failed"),
                }
                match self.claim(free).await {
                    Ok(jobs) => {
                        claimed = jobs.len();
                        total.claimed += claimed;
                        for job in jobs {
                            let worker = self.clone();
                            running.spawn(async move { worker.attempt(job).await });
                        }
                    }
                    Err(problem) => tracing::warn!(
                        code = problem.code(),
                        "job claim failed; retrying after the poll interval"
                    ),
                }
            }
            let wait = if claimed == free && free > 0 {
                Duration::ZERO
            } else {
                self.inner.config.poll
            };
            tokio::select! {
                _ = &mut shutdown => break,
                Some(done) = running.join_next(), if !running.is_empty() => count(&mut total, done),
                _ = tokio::time::sleep(wait) => {}
            }
        }
        let drained = tokio::time::timeout(self.inner.config.drain, async {
            while let Some(done) = running.join_next().await {
                count(&mut total, done);
            }
        })
        .await;
        if drained.is_err() {
            tracing::warn!(
                abandoned = running.len(),
                "drain timed out; abandoned jobs run again after their leases expire"
            );
            running.abort_all();
        }
        total
    }

    /// Marks running jobs whose lease expired with no attempts left as failed.
    async fn sweep(&self) -> Result<usize, Problem> {
        let swept = sqlx::query(&format!(
            "UPDATE {TABLE}
                SET status = 'failed', last_error = 'lease_expired', finished_at = now(),
                    claim = NULL, lease_until = NULL
              WHERE status = 'running' AND lease_until < now() AND attempts >= max_attempts"
        ))
        .execute(&self.inner.pool)
        .await?
        .rows_affected();
        if swept > 0 {
            crate::metrics::increment(
                "trestle_jobs_expired_total",
                "Jobs whose lease expired with no attempts left.",
                &[],
            );
        }
        Ok(swept as usize)
    }

    /// Atomically claims due jobs and expired leases with attempts left.
    /// The attempt bound repeats the sweep's: it covers a lease that expires
    /// between the sweep and this claim, which no test can schedule.
    async fn claim(&self, limit: usize) -> Result<Vec<Claimed>, Problem> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "UPDATE {TABLE} AS j
                SET status = 'running', claim = gen_random_uuid(), attempts = j.attempts + 1,
                    lease_until = now() + make_interval(secs => $1)
               FROM (SELECT id, extract(epoch FROM now() - run_after)::float8 AS lag
                       FROM {TABLE}
                      WHERE attempts < max_attempts
                        AND ((status = 'ready' AND run_after <= now())
                          OR (status = 'running' AND lease_until < now()))
                      ORDER BY run_after, id
                      LIMIT $2
                      FOR UPDATE SKIP LOCKED) AS due
              WHERE j.id = due.id
          RETURNING j.id, j.claim, j.target, j.name, j.payload_version, j.input, j.actor_id,
                    j.tenant_id, j.attempts, j.max_attempts, due.lag"
        );
        // A build the schema fence shuts out claims nothing, so it cannot
        // spend attempts that a current worker would finish.
        let mut tx = crate::fence::begin(&self.inner.pool).await?;
        let rows = sqlx::query(&sql)
            .bind(self.inner.config.lease.as_secs_f64())
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await?;
        crate::db::commit(tx).await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let lag: f64 = r.get(10);
                crate::metrics::observe(
                    "trestle_job_lag_seconds",
                    "Time from when a job was due to when a worker claimed it.",
                    &[],
                    lag.max(0.0),
                );
                Claimed {
                    id: r.get(0),
                    claim: r.get(1),
                    target: r.get(2),
                    name: r.get(3),
                    payload_version: r.get(4),
                    input: r.get(5),
                    actor: r.get(6),
                    tenant: r.get(7),
                    attempts: r.get(8),
                    max_attempts: r.get(9),
                }
            })
            .collect())
    }

    /// Runs one claimed job and records its outcome under its claim.
    async fn attempt(&self, job: Claimed) -> Outcome {
        let inner = &self.inner;
        let mut invocation = match job.actor {
            Some(actor) => Invocation::for_actor(ActorId(actor)),
            None => Invocation::anonymous(),
        };
        if let Some(tenant) = job.tenant {
            invocation = invocation.with_tenant(tenant);
        }
        // Cut the attempt off before its lease ends, so a second worker
        // never runs it while this one still could be.
        let budget = inner.config.lease.mul_f64(0.9);
        let result = match serde_json::from_str::<Value>(&job.input) {
            Err(_) => Err(Problem::malformed(
                "invalid_job_input",
                "The job input is not JSON.",
            )),
            Ok(input) => {
                let run = async {
                    match job.target.as_str() {
                        "operation" if inner.registry.operation(&job.name).is_some() => {
                            // An operation that accepts a key gets the job's ID
                            // as its key: an attempt repeated after its write
                            // committed replays instead of writing again.
                            let keyed = inner
                                .registry
                                .operation(&job.name)
                                .is_some_and(|op| op.accepts_idempotency_key());
                            let invocation = if keyed {
                                invocation.clone().with_idempotency_key(job.id.to_string())
                            } else {
                                invocation.clone()
                            };
                            inner
                                .registry
                                .invoke(&inner.state, &job.name, &invocation, &input)
                                .await
                                .map(drop)
                        }
                        "operation" => Err(Problem::malformed(
                            "unknown_operation",
                            "The job names an operation that is not registered.",
                        )),
                        _ => match inner.registry.job_handler(&job.name) {
                            None => Err(Problem::malformed(
                                "unknown_handler",
                                "The job names a handler that is not registered.",
                            )),
                            Some(handler) => {
                                let run = JobRun {
                                    id: JobId(job.id),
                                    attempt: job.attempts,
                                    payload_version: job.payload_version,
                                    input,
                                    invocation,
                                    registry: &inner.registry,
                                };
                                handler.run(&inner.state, &run).await
                            }
                        },
                    }
                };
                match tokio::time::timeout(budget, run).await {
                    Ok(result) => result,
                    Err(_) => Err(Problem::unavailable("job_timeout")),
                }
            }
        };
        let outcome = match result {
            Ok(()) => self.finish_done(&job).await,
            // Client errors (invalid input, forbidden, not found) will not
            // change by retrying; everything else might.
            Err(problem) if problem.status().is_client_error() => {
                self.finish_failed(&job, problem.code()).await
            }
            Err(problem) if job.attempts >= job.max_attempts => {
                self.finish_failed(&job, problem.code()).await
            }
            Err(problem) => self.finish_retry(&job, problem.code()).await,
        };
        let label = match &outcome {
            Outcome::Done => "done",
            Outcome::Retry(_) => "retry",
            Outcome::Failed(_) => "failed",
            Outcome::Stale => "stale",
        };
        if let Outcome::Retry(code) | Outcome::Failed(code) = &outcome {
            tracing::info!(job = %job.id, kind = %job.name, attempt = job.attempts, code = %code, outcome = label, "job attempt");
        }
        crate::metrics::increment(
            "trestle_jobs_total",
            "Job attempts by target name and outcome.",
            &[("job", &job.name), ("outcome", label)],
        );
        outcome
    }

    async fn finish_done(&self, job: &Claimed) -> Outcome {
        self.fenced(
            job,
            &format!(
                "UPDATE {TABLE} SET status = 'done', finished_at = now(), claim = NULL,
                        lease_until = NULL
                  WHERE id = $1 AND claim = $2 AND status = 'running'"
            ),
            None,
            Outcome::Done,
        )
        .await
    }

    async fn finish_failed(&self, job: &Claimed, code: &str) -> Outcome {
        self.fenced(
            job,
            &format!(
                "UPDATE {TABLE} SET status = 'failed', finished_at = now(), claim = NULL,
                        lease_until = NULL, last_error = $3
                  WHERE id = $1 AND claim = $2 AND status = 'running'"
            ),
            Some(code),
            Outcome::Failed(code.to_owned()),
        )
        .await
    }

    async fn finish_retry(&self, job: &Claimed, code: &str) -> Outcome {
        // 2, 4, 8, … seconds, at most an hour.
        let backoff = 2f64.powi(job.attempts.clamp(1, 12)).min(3600.0);
        self.fenced(
            job,
            &format!(
                "UPDATE {TABLE} SET status = 'ready', claim = NULL, lease_until = NULL,
                        last_error = $3, run_after = now() + make_interval(secs => {backoff})
                  WHERE id = $1 AND claim = $2 AND status = 'running'"
            ),
            Some(code),
            Outcome::Retry(code.to_owned()),
        )
        .await
    }

    /// Applies an outcome only if this worker's claim still owns the job.
    async fn fenced(
        &self,
        job: &Claimed,
        sql: &str,
        code: Option<&str>,
        outcome: Outcome,
    ) -> Outcome {
        let mut query = sqlx::query(sql).bind(job.id).bind(job.claim);
        if let Some(code) = code {
            query = query.bind(code.chars().take(200).collect::<String>());
        }
        match query.execute(&self.inner.pool).await {
            Ok(done) if done.rows_affected() == 1 => outcome,
            Ok(_) => {
                tracing::warn!(job = %job.id, "a newer claim owns this job; result not recorded");
                Outcome::Stale
            }
            Err(err) => {
                // Unrecorded: the lease expires and the job runs again.
                tracing::error!(job = %job.id, error = %err, "could not record a job outcome");
                Outcome::Stale
            }
        }
    }
}

fn count(pass: &mut Pass, outcome: Result<Outcome, tokio::task::JoinError>) {
    match outcome {
        Ok(Outcome::Done) => pass.done += 1,
        Ok(Outcome::Retry(_)) => pass.retried += 1,
        Ok(Outcome::Failed(_)) => pass.failed += 1,
        Ok(Outcome::Stale) | Err(_) => pass.stale += 1,
    }
}
