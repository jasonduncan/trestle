//! ADR-015: durable jobs. Enqueue is transactional; operation jobs are
//! re-authorized when they run; claims are atomic and results are fenced by
//! the claim token; delivery is at least once with a stable idempotency key;
//! failures retry with a bound; expired leases are recovered; shutdown stops
//! claiming and drains. Needs PostgreSQL (`TRESTLE_TEST_DATABASE_URL`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::extract::FromRef;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Executor, PgConnection, PgPool};
use tokio::sync::Notify;
use trestle_core::Problem;
use trestle_core::identity::ActorId;
use trestle_core::invocation::{FromInvocation, Invocation};
use trestle_core::jobs::{self, JobHandler, JobId, JobRun, JobsApp, NewJob, Worker, WorkerConfig};
use trestle_core::operation::*;
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use trestle_core::registry::{App, AppBuilder, Project, Registry};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::SchemaState;
use trestle_historical::testing::TestDatabase;
use uuid::Uuid;

const ALICE: Uuid = Uuid::from_u128(0xaaaa);
const TEAM: Uuid = Uuid::from_u128(0x7ea3);

// State: a pool plus what the test handlers observe.

#[derive(Clone)]
struct State {
    pool: PgPool,
    probe: Arc<Probe>,
}

impl FromRef<State> for PgPool {
    fn from_ref(state: &State) -> Self {
        state.pool.clone()
    }
}

#[derive(Default)]
struct Probe {
    calls: AtomicUsize,
    keys: Mutex<Vec<String>>,
    /// Holds the first call of `notes.hold` until notified.
    release: Notify,
}

// Context: a member of the selected team, looked up on every resolve.

struct Member {
    actor: ActorId,
    team: Uuid,
}

impl RequestContext for Member {
    fn describe() -> ContextDescriptor {
        ContextDescriptor::authenticated().header("Trestle-Tenant", "Team.", Some("uuid"))
    }
}

impl FromInvocation<State> for Member {
    async fn resolve(invocation: &Invocation, state: &State) -> Result<Self, Problem> {
        let actor = invocation.actor()?;
        let team = invocation
            .tenant()
            .ok_or_else(|| Problem::malformed("tenant_required", "Select a team."))?;
        let member: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM members WHERE team = $1 AND actor = $2")
                .bind(team)
                .bind(actor.0)
                .fetch_optional(&state.pool)
                .await?;
        member.ok_or_else(|| Problem::forbidden("tenant_not_permitted"))?;
        Ok(Member { actor, team })
    }
}

struct Members;

impl Policy<Member> for Members {
    fn id(&self) -> &'static str {
        "notes.members"
    }

    fn admit(&self, _: &Member) -> Result<(), Problem> {
        Ok(())
    }

    async fn recheck(&self, conn: &mut PgConnection, ctx: &Member) -> Result<(), Problem> {
        let member: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM members WHERE team = $1 AND actor = $2 FOR SHARE")
                .bind(ctx.team)
                .bind(ctx.actor.0)
                .fetch_optional(conn)
                .await?;
        member
            .map(drop)
            .ok_or_else(|| Problem::forbidden("permission_denied"))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    body: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    fail: bool,
}

impl Contract for Body {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::input("notes.body", "1")
                .field(Field::text("body").chars(1, 200))
                .field(Field::boolean("fail").optional())
        });
        &C
    }
}

#[derive(Serialize)]
struct Done {
    id: Uuid,
}

impl Contract for Done {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> =
            LazyLock::new(|| ContractDoc::output("notes.done", "1").field(Field::uuid("id")));
        &C
    }
}

/// Writes a note. Run later by `notes.post`'s job.
struct AddNote;

impl Operation for AddNote {
    type Context = Member;
    type Input = Body;
    type Output = Done;
    type Policy = Members;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.add",
        version: "1",
        mode: Mode::Write,
        audiences: &[Audience::Product],
        summary: "Add a note.",
    };

    fn policy(&self) -> &Members {
        &Members
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &Member,
        input: Body,
    ) -> Result<Done, Problem> {
        let id = sqlx::query_scalar(
            "INSERT INTO notes (team, body, created_by) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(ctx.team)
        .bind(&input.body)
        .bind(ctx.actor.0)
        .fetch_one(&mut *conn)
        .await?;
        Ok(Done { id })
    }
}

/// Records a post and, in the same transaction, a job that adds its note
/// later under the caller's authority. `fail` makes it roll back after
/// enqueueing.
struct Post;

impl Operation for Post {
    type Context = Member;
    type Input = Body;
    type Output = Done;
    type Policy = Members;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.post",
        version: "1",
        mode: Mode::Write,
        audiences: &[Audience::Product],
        summary: "Post, then add a note later.",
    };

    fn policy(&self) -> &Members {
        &Members
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &Member,
        input: Body,
    ) -> Result<Done, Problem> {
        let id = sqlx::query_scalar("INSERT INTO posts (body) VALUES ($1) RETURNING id")
            .bind(&input.body)
            .fetch_one(&mut *conn)
            .await?;
        let me = Invocation::for_actor(ctx.actor).with_tenant(ctx.team);
        jobs::enqueue(
            conn,
            NewJob::operation("notes.add", json!({ "body": input.body })).on_behalf_of(&me),
        )
        .await?;
        if input.fail {
            return Err(Problem::conflict("rolled_back"));
        }
        Ok(Done { id })
    }
}

/// An external effect. Input: `{"fail_until": n}` fails attempts before
/// `n` as unavailable; `{"forbidden": true}` is a client error.
struct Deliver;

impl JobHandler<State> for Deliver {
    fn run<'a>(
        &'a self,
        state: &'a State,
        job: &'a JobRun<'a, State>,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async move {
            state.probe.calls.fetch_add(1, Ordering::SeqCst);
            state.probe.keys.lock().unwrap().push(job.idempotency_key());
            if job.input["forbidden"] == json!(true) {
                return Err(Problem::forbidden("recipient_blocked"));
            }
            let until = job.input["fail_until"].as_i64().unwrap_or(0);
            if i64::from(job.attempt) < until {
                return Err(Problem::unavailable("provider_down"));
            }
            Ok(())
        })
    }
}

/// The first call records its effect and then waits for `release`; later
/// calls return at once. Simulates a worker that is slow or dies.
struct Hold;

impl JobHandler<State> for Hold {
    fn run<'a>(
        &'a self,
        state: &'a State,
        job: &'a JobRun<'a, State>,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async move {
            let first = state.probe.calls.fetch_add(1, Ordering::SeqCst) == 0;
            state.probe.keys.lock().unwrap().push(job.idempotency_key());
            if first {
                state.probe.release.notified().await;
            }
            Ok(())
        })
    }
}

struct NotesApp;

impl App<State> for NotesApp {
    fn id(&self) -> &str {
        "notes"
    }

    fn register(&self, app: &mut AppBuilder<State>) {
        app.operation(AddNote);
        app.operation(Post);
        app.job("notes.deliver", Deliver);
        app.job("notes.hold", Hold);
    }
}

struct Env {
    db: TestDatabase,
    state: State,
    registry: Arc<Registry<State>>,
}

impl Env {
    async fn new() -> Self {
        let db = TestDatabase::create().await;
        let pool = db.pool().await;
        let mut desired = SchemaState::default();
        let state = jobs::model().state();
        desired.models.insert(state.id.clone(), state);
        let plan = diff(&SchemaState::default(), &desired, &PlanOptions::default());
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
        let ops: Vec<_> = plan.changes.into_iter().flat_map(|c| c.ops).collect();
        let sql = trestle_historical::sql::render(&SchemaState::default(), &ops).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        pool.execute(
            "CREATE TABLE members (team uuid, actor uuid, PRIMARY KEY (team, actor));
             CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), team uuid,
                                 body text NOT NULL, created_by uuid NOT NULL);
             CREATE TABLE posts (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text);",
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO members VALUES ($1, $2)")
            .bind(TEAM)
            .bind(ALICE)
            .execute(&pool)
            .await
            .unwrap();
        let registry = Project::new("jobs-test", "0")
            .install(JobsApp)
            .install(NotesApp)
            .assemble()
            .expect("assembles");
        Self {
            db,
            state: State {
                pool,
                probe: Arc::default(),
            },
            registry: Arc::new(registry),
        }
    }

    fn worker(&self, config: WorkerConfig) -> Worker<State> {
        Worker::new(self.registry.clone(), self.state.clone(), config)
    }

    fn alice(&self) -> Invocation {
        Invocation::for_actor(ActorId(ALICE)).with_tenant(TEAM)
    }

    async fn enqueue(&self, job: NewJob) -> JobId {
        let mut conn = self.state.pool.acquire().await.unwrap();
        jobs::enqueue(&mut conn, job).await.unwrap()
    }

    async fn status(&self, id: JobId) -> jobs::JobStatus {
        let mut conn = self.state.pool.acquire().await.unwrap();
        jobs::status(&mut conn, id)
            .await
            .unwrap()
            .expect("job exists")
    }

    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&self.state.pool)
            .await
            .unwrap()
    }

    /// Makes every pending retry and every lease due now, as if time passed.
    async fn advance(&self) {
        self.state
            .pool
            .execute("UPDATE trestle_jobs SET run_after = now() - interval '1 second' WHERE status = 'ready'")
            .await
            .unwrap();
        self.state
            .pool
            .execute("UPDATE trestle_jobs SET lease_until = now() - interval '1 second' WHERE status = 'running'")
            .await
            .unwrap();
    }

    async fn finish(self) {
        self.state.pool.close().await;
        self.db.drop().await;
    }
}

fn quick() -> WorkerConfig {
    WorkerConfig {
        poll: Duration::from_millis(20),
        ..WorkerConfig::default()
    }
}

#[tokio::test]
async fn enqueue_belongs_to_the_operations_transaction() {
    let env = Env::new().await;
    let failed = env
        .registry
        .invoke(
            &env.state,
            "notes.post",
            &env.alice(),
            &json!({ "body": "hi", "fail": true }),
        )
        .await
        .expect_err("rolls back");
    assert_eq!(failed.code(), "rolled_back");
    assert_eq!(
        env.count("SELECT count(*) FROM trestle_jobs").await,
        0,
        "no job without its post"
    );
    assert_eq!(env.count("SELECT count(*) FROM posts").await, 0);

    env.registry
        .invoke(
            &env.state,
            "notes.post",
            &env.alice(),
            &json!({ "body": "hi" }),
        )
        .await
        .expect("commits");
    assert_eq!(env.count("SELECT count(*) FROM posts").await, 1);
    assert_eq!(
        env.count(
            "SELECT count(*) FROM trestle_jobs WHERE status = 'ready' AND actor_id IS NOT NULL"
        )
        .await,
        1,
        "the job and its post commit together"
    );
    env.finish().await;
}

#[tokio::test]
async fn operation_jobs_are_reauthorized_when_they_run() {
    let env = Env::new().await;
    let worker = env.worker(quick());
    env.registry
        .invoke(
            &env.state,
            "notes.post",
            &env.alice(),
            &json!({ "body": "first" }),
        )
        .await
        .unwrap();
    let pass = worker.run_once(10).await.unwrap();
    assert_eq!((pass.claimed, pass.done), (1, 1));
    assert_eq!(
        env.count(&format!(
            "SELECT count(*) FROM notes WHERE created_by = '{ALICE}' AND body = 'first'"
        ))
        .await,
        1,
        "the note is written as the enqueuing actor"
    );

    // Alice leaves the team after posting: the deferred write must not happen.
    env.registry
        .invoke(
            &env.state,
            "notes.post",
            &env.alice(),
            &json!({ "body": "second" }),
        )
        .await
        .unwrap();
    sqlx::query("DELETE FROM members")
        .execute(&env.state.pool)
        .await
        .unwrap();
    let pass = worker.run_once(10).await.unwrap();
    assert_eq!(
        (pass.claimed, pass.failed),
        (1, 1),
        "a client error does not retry"
    );
    let last: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM trestle_jobs WHERE status = 'failed'")
            .fetch_one(&env.state.pool)
            .await
            .unwrap();
    assert_eq!(last.as_deref(), Some("tenant_not_permitted"));
    assert_eq!(
        env.count("SELECT count(*) FROM notes WHERE body = 'second'")
            .await,
        0
    );
    env.finish().await;
}

#[tokio::test]
async fn failures_retry_with_one_idempotency_key_until_they_succeed() {
    let env = Env::new().await;
    let worker = env.worker(quick());
    let id = env
        .enqueue(
            NewJob::handler("notes.deliver", json!({ "fail_until": 3 })).on_behalf_of(&env.alice()),
        )
        .await;
    for attempt in 1..=2 {
        let pass = worker.run_once(10).await.unwrap();
        assert_eq!(pass.retried, 1, "attempt {attempt}");
        let status = env.status(id).await;
        assert_eq!(
            (status.status.as_str(), status.attempts),
            ("ready", attempt)
        );
        assert_eq!(status.last_error.as_deref(), Some("provider_down"));
        assert_eq!(
            worker.run_once(10).await.unwrap().claimed,
            0,
            "backoff: not due yet"
        );
        env.advance().await;
    }
    assert_eq!(worker.run_once(10).await.unwrap().done, 1);
    assert_eq!(env.status(id).await.status, "done");
    let keys = env.state.probe.keys.lock().unwrap().clone();
    assert_eq!(
        keys,
        vec![id.to_string(); 3],
        "every attempt carries the job's key"
    );
    env.finish().await;
}

#[tokio::test]
async fn attempts_are_bounded_and_client_errors_do_not_retry() {
    let env = Env::new().await;
    let worker = env.worker(quick());
    let flaky = env
        .enqueue(NewJob::handler("notes.deliver", json!({ "fail_until": 99 })).max_attempts(2))
        .await;
    let blocked = env
        .enqueue(NewJob::handler(
            "notes.deliver",
            json!({ "forbidden": true }),
        ))
        .await;
    let unknown = env
        .enqueue(NewJob::handler("notes.missing", json!({})))
        .await;
    worker.run_once(10).await.unwrap();
    env.advance().await;
    worker.run_once(10).await.unwrap();
    let flaky = env.status(flaky).await;
    assert_eq!((flaky.status.as_str(), flaky.attempts), ("failed", 2));
    assert_eq!(flaky.last_error.as_deref(), Some("provider_down"));
    let blocked = env.status(blocked).await;
    assert_eq!((blocked.status.as_str(), blocked.attempts), ("failed", 1));
    assert_eq!(blocked.last_error.as_deref(), Some("recipient_blocked"));
    assert_eq!(
        env.status(unknown).await.last_error.as_deref(),
        Some("unknown_handler")
    );
    env.finish().await;
}

#[tokio::test]
async fn a_stale_claim_cannot_record_its_result() {
    let env = Env::new().await;
    let id = env.enqueue(NewJob::handler("notes.hold", json!({}))).await;
    // Worker A claims and is held mid-attempt.
    let a = env.worker(quick());
    let held = tokio::spawn(async move { a.run_once(1).await.unwrap() });
    while env.state.probe.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // A's lease runs out (a pause, a partition); B reclaims and finishes.
    env.advance().await;
    let b = env.worker(quick()).run_once(1).await.unwrap();
    assert_eq!((b.claimed, b.done), (1, 1));
    // A resumes and tries to record its result under the old claim.
    env.state.probe.release.notify_one();
    let a = held.await.unwrap();
    assert_eq!((a.claimed, a.stale, a.done), (1, 1, 0));
    let status = env.status(id).await;
    assert_eq!((status.status.as_str(), status.attempts), ("done", 2));
    env.finish().await;
}

#[tokio::test]
async fn work_lost_with_its_worker_runs_again() {
    let env = Env::new().await;
    let id = env.enqueue(NewJob::handler("notes.hold", json!({}))).await;
    // The worker performs the effect, then dies before recording it.
    let a = env.worker(quick());
    let dying = tokio::spawn(async move { a.run_once(1).await });
    while env.state.probe.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    dying.abort();
    let _ = dying.await;
    assert_eq!(env.status(id).await.status, "running", "nothing recorded");
    // Once the lease expires another worker runs it again: at least once.
    env.advance().await;
    let pass = env.worker(quick()).run_once(1).await.unwrap();
    assert_eq!(pass.done, 1);
    let keys = env.state.probe.keys.lock().unwrap().clone();
    assert_eq!(
        keys,
        vec![id.to_string(); 2],
        "both attempts carry the same key"
    );
    env.finish().await;
}

#[tokio::test]
async fn expired_leases_without_attempts_left_are_failed() {
    let env = Env::new().await;
    let id = env
        .enqueue(NewJob::handler("notes.hold", json!({})).max_attempts(1))
        .await;
    let a = env.worker(quick());
    let dying = tokio::spawn(async move { a.run_once(1).await });
    while env.state.probe.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    dying.abort();
    let _ = dying.await;
    env.advance().await;
    let pass = env.worker(quick()).run_once(1).await.unwrap();
    assert_eq!((pass.expired, pass.claimed), (1, 0));
    let status = env.status(id).await;
    assert_eq!(status.status, "failed");
    assert_eq!(status.last_error.as_deref(), Some("lease_expired"));
    env.finish().await;
}

#[tokio::test]
async fn a_dedupe_key_enqueues_once() {
    let env = Env::new().await;
    let job = || NewJob::handler("notes.deliver", json!({})).dedupe_key("welcome:alice");
    let first = env.enqueue(job()).await;
    let again = env.enqueue(job()).await;
    assert_eq!(first, again);
    assert_eq!(env.count("SELECT count(*) FROM trestle_jobs").await, 1);
    env.finish().await;
}

#[tokio::test]
async fn shutdown_stops_claiming_and_drains_running_jobs() {
    let env = Env::new().await;
    let slow = env.enqueue(NewJob::handler("notes.hold", json!({}))).await;
    let worker = env.worker(WorkerConfig {
        poll: Duration::from_secs(30),
        ..WorkerConfig::default()
    });
    let stop = Arc::new(Notify::new());
    let signal = stop.clone();
    let running =
        tokio::spawn(async move { worker.run(async move { signal.notified().await }).await });
    while env.state.probe.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let later = env
        .enqueue(NewJob::handler("notes.deliver", json!({})))
        .await;
    stop.notify_one();
    tokio::time::sleep(Duration::from_millis(50)).await;
    env.state.probe.release.notify_one();
    let pass = running.await.unwrap();
    assert_eq!(
        (pass.claimed, pass.done),
        (1, 1),
        "the running job finished during the drain"
    );
    assert_eq!(env.status(slow).await.status, "done");
    assert_eq!(
        env.status(later).await.status,
        "ready",
        "nothing is claimed after shutdown"
    );
    env.finish().await;
}

#[tokio::test]
async fn a_drain_that_times_out_leaves_the_job_to_its_lease() {
    let env = Env::new().await;
    let id = env.enqueue(NewJob::handler("notes.hold", json!({}))).await;
    let worker = env.worker(WorkerConfig {
        poll: Duration::from_secs(30),
        drain: Duration::from_millis(50),
        ..WorkerConfig::default()
    });
    let stop = Arc::new(Notify::new());
    let signal = stop.clone();
    let running =
        tokio::spawn(async move { worker.run(async move { signal.notified().await }).await });
    while env.state.probe.calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    stop.notify_one();
    let pass = running.await.unwrap();
    assert_eq!((pass.claimed, pass.done), (1, 0));
    assert_eq!(
        env.status(id).await.status,
        "running",
        "abandoned under its lease"
    );
    env.advance().await;
    assert_eq!(
        env.worker(quick()).run_once(1).await.unwrap().done,
        1,
        "recovered"
    );
    env.finish().await;
}

#[tokio::test]
async fn purging_removes_only_finished_jobs_past_retention() {
    let env = Env::new().await;
    let done = env
        .enqueue(NewJob::handler("notes.deliver", json!({})))
        .await;
    let failed = env
        .enqueue(NewJob::handler(
            "notes.deliver",
            json!({ "forbidden": true }),
        ))
        .await;
    let pending = env
        .enqueue(NewJob::handler("notes.deliver", json!({})).after(Duration::from_secs(3600)))
        .await;
    env.worker(quick()).run_once(10).await.unwrap();
    let mut conn = env.state.pool.acquire().await.unwrap();
    assert_eq!(
        jobs::purge_finished(&mut conn, Duration::from_secs(60))
            .await
            .unwrap(),
        0,
        "too recent"
    );
    sqlx::query("UPDATE trestle_jobs SET finished_at = now() - interval '2 minutes' WHERE finished_at IS NOT NULL")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        jobs::purge_finished(&mut conn, Duration::from_secs(60))
            .await
            .unwrap(),
        2
    );
    for (id, exists) in [(done, false), (failed, false), (pending, true)] {
        assert_eq!(jobs::status(&mut conn, id).await.unwrap().is_some(), exists);
    }
    drop(conn);
    env.finish().await;
}

#[test]
fn job_kinds_are_namespaced_and_unique() {
    struct Bad;
    impl App<State> for Bad {
        fn id(&self) -> &str {
            "notes"
        }
        fn register(&self, app: &mut AppBuilder<State>) {
            app.job("other.deliver", Deliver);
            app.job("notes.twice", Deliver);
            app.job("notes.twice", Deliver);
        }
    }
    let Err(error) = Project::<State>::new("t", "0").install(Bad).assemble() else {
        panic!("assembly must fail");
    };
    let issues = error.issues.join("\n");
    assert!(
        issues.contains("job kind \"other.deliver\" must be a stable ID namespaced to app notes"),
        "{issues}"
    );
    assert!(
        issues.contains("job kind notes.twice is registered twice"),
        "{issues}"
    );
}
