//! ADR-017 through the executor: once a fence migration this build lacks is
//! applied, its writes, job claims and readiness answer 503
//! `schema_fenced` and write nothing; reads go on; a build that contains
//! the migration writes again on the same pool. Needs PostgreSQL
//! (`TRESTLE_TEST_DATABASE_URL`).
//!
//! The fence is process-wide, so this file is one test.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::FromRef;
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgConnection, PgPool};
use tower::ServiceExt;
use trestle_core::Problem;
use trestle_core::fence::{self, BuildFence};
use trestle_core::identity::{ActorId, DevTokenIdentity, Identity};
use trestle_core::invocation::{FromInvocation, Invocation};
use trestle_core::jobs::{self, JobsApp, NewJob, Worker, WorkerConfig};
use trestle_core::operation::*;
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use trestle_core::registry::{App, AppBuilder, Http, Project, Registry};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::SchemaState;
use trestle_historical::testing::TestDatabase;
use trestle_historical::{Migration, MigrationRef, MigrationSet, migrate};
use uuid::Uuid;

const ALICE: Uuid = Uuid::from_u128(0xa11ce);
const TEAM: Uuid = Uuid::from_u128(0x7ea3);
const TOKENS: &str = "alice-dev-token=00000000-0000-0000-0000-0000000a11ce";

#[derive(Clone)]
struct State {
    pool: PgPool,
    identity: Identity,
}

impl FromRef<State> for PgPool {
    fn from_ref(state: &State) -> Self {
        state.pool.clone()
    }
}

impl FromRef<State> for Identity {
    fn from_ref(state: &State) -> Self {
        state.identity.clone()
    }
}

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
    async fn resolve(invocation: &Invocation, _: &State) -> Result<Self, Problem> {
        let team = invocation
            .tenant()
            .ok_or_else(|| Problem::malformed("tenant_required", "Select a team."))?;
        Ok(Member {
            actor: invocation.actor()?,
            team,
        })
    }
}

struct Anyone;

impl Policy<Member> for Anyone {
    fn id(&self) -> &'static str {
        "notes.anyone"
    }

    fn admit(&self, _: &Member) -> Result<(), Problem> {
        Ok(())
    }

    async fn recheck(&self, _: &mut PgConnection, _: &Member) -> Result<(), Problem> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewNote {
    body: String,
}

impl Contract for NewNote {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::input("notes.new", "1").field(Field::text("body").chars(1, 200))
        });
        &C
    }
}

#[derive(Serialize)]
struct Note {
    id: Uuid,
}

impl Contract for Note {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> =
            LazyLock::new(|| ContractDoc::output("notes.view", "1").field(Field::uuid("id")));
        &C
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Nothing {}

impl Contract for Nothing {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| ContractDoc::input("notes.none", "1"));
        &C
    }
}

#[derive(Serialize)]
struct Count {
    count: i64,
}

impl Contract for Count {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::output("notes.count", "1").field(Field::integer("count"))
        });
        &C
    }
}

struct AddNote;

impl Operation for AddNote {
    type Context = Member;
    type Input = NewNote;
    type Output = Note;
    type Policy = Anyone;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.add",
        version: "1",
        mode: Mode::Write,
        audiences: &[Audience::Product],
        summary: "Add a note.",
    };

    fn policy(&self) -> &Anyone {
        &Anyone
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &Member,
        input: NewNote,
    ) -> Result<Note, Problem> {
        let id = sqlx::query_scalar(
            "INSERT INTO notes (team, body, created_by) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(ctx.team)
        .bind(&input.body)
        .bind(ctx.actor.0)
        .fetch_one(&mut *conn)
        .await?;
        Ok(Note { id })
    }
}

struct CountNotes;

impl Operation for CountNotes {
    type Context = Member;
    type Input = Nothing;
    type Output = Count;
    type Policy = Anyone;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.count",
        version: "1",
        mode: Mode::Read,
        audiences: &[Audience::Product],
        summary: "Count notes.",
    };

    fn policy(&self) -> &Anyone {
        &Anyone
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &Member,
        _: Nothing,
    ) -> Result<Count, Problem> {
        let count = sqlx::query_scalar("SELECT count(*) FROM notes WHERE team = $1")
            .bind(ctx.team)
            .fetch_one(&mut *conn)
            .await?;
        Ok(Count { count })
    }
}

struct NotesApp;

impl App<State> for NotesApp {
    fn id(&self) -> &str {
        "notes"
    }

    fn register(&self, app: &mut AppBuilder<State>) {
        app.operation(AddNote)
            .http(Http::post("/notes").created("/notes/{id}"));
        app.operation(CountNotes);
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn notes_migration(
    id: &'static str,
    deps: &'static [MigrationRef],
    sql: &str,
    extra: &str,
) -> Migration {
    let names: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
    Migration {
        app: "notes",
        id,
        depends_on: deps,
        sql: leak(sql.to_owned()),
        manifest: Some(leak(format!(
            r#"{{"format": 2, "app": "notes", "id": "{id}", "depends_on": [{}], "operations": []{extra}}}"#,
            names.join(", ")
        ))),
        data: None,
    }
}

const INITIAL: &[MigrationRef] = &[MigrationRef::new("notes", "0001_initial")];

/// Release 1 creates the tables; release 2 adds a fence migration.
fn release(n: usize) -> MigrationSet {
    let mut desired = SchemaState::default();
    let state = jobs::model().state();
    desired.models.insert(state.id.clone(), state);
    let plan = diff(&SchemaState::default(), &desired, &PlanOptions::default());
    let ops: Vec<_> = plan.changes.into_iter().flat_map(|c| c.ops).collect();
    let jobs_sql = trestle_historical::sql::render(&SchemaState::default(), &ops).unwrap();
    let initial = notes_migration(
        "0001_initial",
        &[],
        &format!(
            "CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), team uuid,
                                 body text NOT NULL, created_by uuid NOT NULL);
             {jobs_sql}"
        ),
        "",
    );
    let fenced = notes_migration(
        "0002_body_is_markdown",
        INITIAL,
        "UPDATE notes SET body = replace(body, '*', '\\*')",
        r#", "fence": true"#,
    );
    MigrationSet::new([initial, fenced].into_iter().take(n).collect()).unwrap()
}

struct Env {
    db: TestDatabase,
    state: State,
    registry: Arc<Registry<State>>,
    app: Router,
}

impl Env {
    async fn new() -> Self {
        let db = TestDatabase::create().await;
        migrate(db.options(), &release(1)).await.unwrap();
        // One connection: a refused write must hand it back usable.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(db.options().clone())
            .await
            .unwrap();
        let registry = Arc::new(
            Project::new("fence", "0")
                .install(JobsApp)
                .install(NotesApp)
                .assemble()
                .expect("assembles"),
        );
        let state = State {
            pool,
            identity: Arc::new(DevTokenIdentity::parse(TOKENS).unwrap()),
        };
        let app = registry.router().with_state(state.clone());
        Self {
            db,
            state,
            registry,
            app,
        }
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value, Option<String>) {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, "Bearer alice-dev-token")
            .header("trestle-tenant", TEAM.to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            retry,
        )
    }

    async fn notes(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM notes")
            .fetch_one(&self.state.pool)
            .await
            .unwrap()
    }

    fn alice(&self) -> Invocation {
        Invocation::for_actor(ActorId(ALICE)).with_tenant(TEAM)
    }

    fn worker(&self) -> Worker<State> {
        Worker::new(
            self.registry.clone(),
            self.state.clone(),
            WorkerConfig {
                poll: Duration::from_millis(20),
                ..WorkerConfig::default()
            },
        )
    }
}

#[tokio::test]
async fn an_older_build_stops_writing_once_a_fence_migration_applies() {
    let env = Env::new().await;
    fence::install(BuildFence::of(&release(1)));

    let (status, _, _) = env
        .send(Method::POST, "/notes", Some(json!({ "body": "a" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        env.send(Method::GET, "/readyz", None).await.0,
        StatusCode::OK
    );

    // A newer release applies its fence migration.
    migrate(env.db.options(), &release(2)).await.unwrap();

    let (status, problem, retry) = env
        .send(Method::POST, "/notes", Some(json!({ "body": "b" })))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(problem["code"], "schema_fenced");
    assert_eq!(retry.as_deref(), Some("1"));
    let detail = problem["detail"].as_str().unwrap();
    assert!(
        !detail.contains("0002"),
        "no migration names on the wire: {detail}"
    );
    assert_eq!(env.notes().await, 1, "nothing written");

    let refused = env
        .registry
        .invoke(
            &env.state,
            "notes.add",
            &env.alice(),
            &json!({ "body": "c" }),
        )
        .await
        .expect_err("fenced");
    assert_eq!(refused.code(), "schema_fenced");
    let count = env
        .registry
        .invoke(&env.state, "notes.count", &env.alice(), &json!({}))
        .await
        .expect("reads go on");
    assert_eq!(count["count"], 1);

    let (status, problem, _) = env.send(Method::GET, "/readyz", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem["code"], "schema_fenced", "drops out of rotation");

    // A job enqueued by a current build is left for a current worker.
    let job = {
        let mut conn = env.state.pool.acquire().await.unwrap();
        jobs::enqueue(
            &mut conn,
            NewJob::operation("notes.add", json!({ "body": "from a job" }))
                .on_behalf_of(&env.alice()),
        )
        .await
        .unwrap()
    };
    let err = env.worker().run_once(4).await.expect_err("claims nothing");
    assert_eq!(err.code(), "schema_fenced");
    let mut conn = env.state.pool.acquire().await.unwrap();
    let status = jobs::status(&mut conn, job).await.unwrap().unwrap();
    drop(conn);
    assert_eq!((status.status.as_str(), status.attempts), ("ready", 0));

    // The build that contains the migration writes on the same pool.
    fence::install(BuildFence::of(&release(2)));
    let (status, _, _) = env
        .send(Method::POST, "/notes", Some(json!({ "body": "d" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        env.send(Method::GET, "/readyz", None).await.0,
        StatusCode::OK
    );
    let pass = env.worker().run_once(4).await.unwrap();
    assert_eq!(pass.claimed, 1);
    assert_eq!(env.notes().await, 3);

    fence::uninstall();
    env.state.pool.close().await;
    env.db.drop().await;
}
