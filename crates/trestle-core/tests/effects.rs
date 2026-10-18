//! ADR-018: declared effects and read-only reads. A read cannot write, a
//! write that declared its effects may enqueue only what it lists in
//! `emits`, declarations name things that exist, and `inspect` shows them.
//! Needs PostgreSQL (`TRESTLE_TEST_DATABASE_URL`).

use std::sync::{Arc, LazyLock};

use axum::extract::FromRef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Executor, PgConnection, PgPool};
use trestle_core::Problem;
use trestle_core::identity::{DevTokenIdentity, Identity};
use trestle_core::jobs::{self, JobHandler, JobRun, NewJob};
use trestle_core::operation::*;
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use trestle_core::registry::{App, AppBuilder, Project};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::{Column, Model, SchemaState};
use trestle_historical::testing::TestDatabase;

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

struct Ctx;

impl RequestContext for Ctx {
    fn describe() -> ContextDescriptor {
        ContextDescriptor::default()
    }
}

impl trestle_core::invocation::FromInvocation<State> for Ctx {
    async fn resolve(_: &trestle_core::invocation::Invocation, _: &State) -> Result<Self, Problem> {
        Ok(Ctx)
    }
}

struct Everyone;

impl Policy<Ctx> for Everyone {
    fn id(&self) -> &'static str {
        "notes.everyone"
    }

    fn admit(&self, _: &Ctx) -> Result<(), Problem> {
        Ok(())
    }

    async fn recheck(&self, _: &mut PgConnection, _: &Ctx) -> Result<(), Problem> {
        Ok(())
    }
}

/// `job`: what to enqueue after inserting the note.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Post {
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    job: Option<String>,
}

impl Contract for Post {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::input("notes.post", "1")
                .field(Field::text("body").chars(1, 200))
                .field(Field::text("job").chars(1, 64).optional())
        });
        &C
    }
}

#[derive(Serialize)]
struct Posted {
    count: i64,
}

impl Contract for Posted {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::output("notes.posted", "1").field(Field::integer("count"))
        });
        &C
    }
}

async fn post(conn: &mut PgConnection, input: &Post) -> Result<Posted, Problem> {
    sqlx::query("INSERT INTO notes (body) VALUES ($1)")
        .bind(&input.body)
        .execute(&mut *conn)
        .await?;
    if let Some(job) = &input.job {
        jobs::enqueue(conn, NewJob::handler(job.as_str(), json!({}))).await?;
    }
    let count = sqlx::query_scalar("SELECT count(*) FROM notes")
        .fetch_one(&mut *conn)
        .await?;
    Ok(Posted { count })
}

macro_rules! post_op {
    ($name:ident, $id:literal, $mode:expr, $effects:expr) => {
        struct $name;

        impl Operation for $name {
            type Context = Ctx;
            type Input = Post;
            type Output = Posted;
            type Policy = Everyone;

            const INFO: OperationInfo = OperationInfo {
                id: $id,
                version: "1",
                mode: $mode,
                audiences: &[Audience::Product],
                summary: "Post a note.",
            };

            const EFFECTS: Option<EffectsInfo> = $effects;

            fn policy(&self) -> &Everyone {
                &Everyone
            }

            async fn run(
                &self,
                conn: &mut PgConnection,
                _: &Ctx,
                input: Post,
            ) -> Result<Posted, Problem> {
                post(conn, &input).await
            }
        }
    };
}

post_op!(
    Declared,
    "notes.declared",
    Mode::Write,
    Some(
        EffectsInfo::new()
            .writes(&["notes.note"])
            .emits(&["notes.deliver"])
            .egress(&["smtp"])
    )
);
post_op!(Undeclared, "notes.undeclared", Mode::Write, None);
// A read that writes anyway: the bug read-only reads stop.
post_op!(Sneaky, "notes.sneaky", Mode::Read, None);

/// A runtime-assembled write declaring the same effects as `Declared`.
struct DynPost {
    info: OperationDescriptor,
}

impl DynOperation<State> for DynPost {
    fn info(&self) -> &OperationDescriptor {
        &self.info
    }

    fn policy_id(&self) -> &str {
        "notes.everyone"
    }

    fn context(&self) -> ContextDescriptor {
        ContextDescriptor::default()
    }

    fn input(&self) -> &ContractDoc {
        Post::contract()
    }

    fn output(&self) -> &ContractDoc {
        Posted::contract()
    }

    fn resolve_context<'a>(
        &'a self,
        _: &'a trestle_core::invocation::Invocation,
        _: &'a State,
    ) -> BoxFuture<'a, Result<DynContext, Problem>> {
        Box::pin(async { Ok(Arc::new(()) as DynContext) })
    }

    fn admit(&self, _: &DynContext) -> Result<(), Problem> {
        Ok(())
    }

    fn recheck<'a>(
        &'a self,
        _: &'a mut PgConnection,
        _: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async { Ok(()) })
    }

    fn run(
        self: Arc<Self>,
        mut conn: DynConn,
        _: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)> {
        Box::pin(async move {
            let input: Post = serde_json::from_value(input).unwrap();
            let result = post(conn.conn(), &input)
                .await
                .map(|p| json!({ "count": p.count }));
            (conn, result)
        })
    }
}

fn dyn_post(id: &str, mode: Mode) -> Arc<DynPost> {
    Arc::new(DynPost {
        info: OperationDescriptor::new(id, "1", mode)
            .audiences(&[Audience::Product])
            .effects(Effects::new().writes("notes.note").emits("notes.deliver")),
    })
}

struct Deliver;

impl JobHandler<State> for Deliver {
    fn run<'a>(
        &'a self,
        _: &'a State,
        _: &'a JobRun<'a, State>,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async { Ok(()) })
    }
}

fn note_model() -> Model {
    Model::new("notes.note")
        .table("notes")
        .field(Column::uuid("id").primary_key())
        .field(Column::text("body"))
}

struct NotesApp;

impl App<State> for NotesApp {
    fn id(&self) -> &str {
        "notes"
    }

    fn register(&self, app: &mut AppBuilder<State>) {
        app.model(note_model());
        app.operation(Declared);
        app.operation(Undeclared);
        app.dyn_operation(dyn_post("notes.dyn_post", Mode::Write), None);
        app.job("notes.deliver", Deliver);
    }
}

async fn setup() -> (TestDatabase, State) {
    let db = TestDatabase::create().await;
    let pool = db.pool().await;
    let mut desired = SchemaState::default();
    let state = jobs::model().state();
    desired.models.insert(state.id.clone(), state);
    let plan = diff(&SchemaState::default(), &desired, &PlanOptions::default());
    let ops: Vec<_> = plan.changes.into_iter().flat_map(|c| c.ops).collect();
    let sql = trestle_historical::sql::render(&SchemaState::default(), &ops).unwrap();
    sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
    pool.execute(
        "CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text NOT NULL)",
    )
    .await
    .unwrap();
    let state = State {
        pool,
        identity: Arc::new(
            DevTokenIdentity::parse("a-dev-token=00000000-0000-0000-0000-00000000000a").unwrap(),
        ),
    };
    (db, state)
}

async fn counts(pool: &PgPool) -> (i64, i64) {
    let notes = sqlx::query_scalar("SELECT count(*) FROM notes")
        .fetch_one(pool)
        .await
        .unwrap();
    let jobs = sqlx::query_scalar("SELECT count(*) FROM trestle_jobs")
        .fetch_one(pool)
        .await
        .unwrap();
    (notes, jobs)
}

fn posting(job: Option<&str>) -> Post {
    Post {
        body: "hi".into(),
        job: job.map(str::to_owned),
    }
}

/// `read_only_reads` is process-wide; tests that set it take turns.
static READ_ONLY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn a_read_cannot_write() {
    let _turn = READ_ONLY.lock().await;
    trestle_core::db::set_read_only_reads(true);
    let (db, state) = setup().await;
    let problem = execute(&state.pool, &Sneaky, &Ctx, posting(None))
        .await
        .err()
        .expect("the insert fails in a read-only transaction");
    assert_eq!(problem.code(), "internal_error");

    let sneaky = dyn_post("notes.dyn_sneaky", Mode::Read);
    let problem = execute_dyn(&state.pool, sneaky, Arc::new(()), &json!({ "body": "hi" }))
        .await
        .expect_err("dynamic reads are read-only too");
    assert_eq!(problem.code(), "internal_error");
    assert_eq!(counts(&state.pool).await, (0, 0));

    // The pool hands those connections back usable.
    execute(&state.pool, &Undeclared, &Ctx, posting(None))
        .await
        .unwrap();
    assert_eq!(counts(&state.pool).await, (1, 0));
    state.pool.close().await;
    db.drop().await;
}

/// Release builds skip the read-only transaction unless asked: a read runs
/// on a plain pooled connection, as before.
#[tokio::test]
async fn with_read_only_reads_off_a_read_runs_without_a_transaction() {
    let _turn = READ_ONLY.lock().await;
    trestle_core::db::set_read_only_reads(false);
    let (db, state) = setup().await;
    execute(&state.pool, &Sneaky, &Ctx, posting(None))
        .await
        .expect("nothing stops it");
    let sneaky = dyn_post("notes.dyn_sneaky", Mode::Read);
    execute_dyn(&state.pool, sneaky, Arc::new(()), &json!({ "body": "hi" }))
        .await
        .expect("nothing stops it");
    assert_eq!(counts(&state.pool).await, (2, 0));
    trestle_core::db::set_read_only_reads(true);
    state.pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_declared_write_enqueues_only_what_it_emits() {
    let (db, state) = setup().await;
    execute(&state.pool, &Declared, &Ctx, posting(Some("notes.deliver")))
        .await
        .expect("declared");
    assert_eq!(counts(&state.pool).await, (1, 1));

    let problem = execute(&state.pool, &Declared, &Ctx, posting(Some("notes.other")))
        .await
        .err()
        .expect("not declared in emits");
    assert_eq!(problem.code(), "internal_error");
    assert_eq!(counts(&state.pool).await, (1, 1), "the write rolled back");

    // Declaring nothing checks nothing.
    execute(&state.pool, &Undeclared, &Ctx, posting(Some("notes.other")))
        .await
        .expect("undeclared operations are unchecked");
    assert_eq!(counts(&state.pool).await, (2, 2));

    let declared = dyn_post("notes.dyn_post", Mode::Write);
    execute_dyn(
        &state.pool,
        declared.clone(),
        Arc::new(()),
        &json!({ "body": "hi", "job": "notes.deliver" }),
    )
    .await
    .expect("declared");
    let problem = execute_dyn(
        &state.pool,
        declared,
        Arc::new(()),
        &json!({ "body": "hi", "job": "notes.other" }),
    )
    .await
    .expect_err("dynamic operations are checked the same way");
    assert_eq!(problem.code(), "internal_error");
    assert_eq!(counts(&state.pool).await, (3, 3));

    // Outside any operation (a job handler, a command), enqueue is unchecked.
    let mut conn = state.pool.acquire().await.unwrap();
    jobs::enqueue(&mut conn, NewJob::handler("notes.other", json!({})))
        .await
        .unwrap();
    drop(conn);
    state.pool.close().await;
    db.drop().await;
}

#[test]
fn inspect_shows_what_was_declared() {
    let registry = Project::new("effects", "0")
        .install(trestle_core::jobs::JobsApp)
        .install(NotesApp)
        .assemble()
        .expect("valid declarations assemble");
    let declared = registry.inspect_operation("notes.declared").unwrap();
    assert_eq!(
        declared["effects"],
        json!({
            "reads": [],
            "writes": ["notes.note"],
            "emits": ["notes.deliver"],
            "egress": ["smtp"],
        })
    );
    assert!(registry.inspect_operation("notes.undeclared").unwrap()["effects"].is_null());
    assert_eq!(
        registry.inspect_operation("notes.dyn_post").unwrap()["effects"]["writes"],
        json!(["notes.note"])
    );
}

struct Wrong;

impl App<State> for Wrong {
    fn id(&self) -> &str {
        "wrong"
    }

    fn register(&self, app: &mut AppBuilder<State>) {
        app.dyn_operation(
            Arc::new(DynPost {
                info: OperationDescriptor::new("wrong.read", "1", Mode::Read)
                    .effects(Effects::new().reads("notes.note").writes("notes.note")),
            }),
            None,
        );
        app.dyn_operation(
            Arc::new(DynPost {
                info: OperationDescriptor::new("wrong.write", "1", Mode::Write).effects(
                    Effects::new()
                        .writes("billing.invoice")
                        .emits("wrong.nothing")
                        .egress(" "),
                ),
            }),
            None,
        );
    }
}

#[test]
fn declarations_must_name_things_that_exist() {
    let issues = Project::<State>::new("effects", "0")
        .install(NotesApp)
        .install(Wrong)
        .assemble()
        .err()
        .expect("refused")
        .issues;
    for expected in [
        "operation wrong.read is a read; it cannot declare writes or emits",
        "operation wrong.write declares an effect on model billing.invoice, which no installed app declares",
        "operation wrong.write emits wrong.nothing, which is neither a registered operation nor a job kind",
        "operation wrong.write declares an empty egress",
    ] {
        assert!(
            issues.iter().any(|i| i == expected),
            "missing {expected:?} in {issues:#?}"
        );
    }
}

#[test]
fn declarations_are_sorted_and_unique() {
    let effects = Effects::new().reads("b.x").reads("a.y").reads("b.x");
    assert_eq!(effects.reads, [Arc::from("a.y"), Arc::from("b.x")]);
    assert!(effects.touches("a.y") && !effects.touches("c.z"));
    let typed = Effects::from(EffectsInfo::new().writes(&["n.b", "n.a"]));
    assert_eq!(typed, Effects::new().writes("n.a").writes("n.b"));
}
