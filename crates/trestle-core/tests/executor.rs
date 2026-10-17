//! The executor refuses outputs that break their contract (ADR-012): a write
//! rolls back instead of committing, and a read fails rather than returning
//! an undeclared shape. Needs PostgreSQL (`TRESTLE_TEST_DATABASE_URL`).

use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Executor, PgConnection, PgPool};
use trestle_core::Problem;
use trestle_core::operation::*;
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use trestle_historical::testing::TestDatabase;

struct Ctx;

impl RequestContext for Ctx {
    fn describe() -> ContextDescriptor {
        ContextDescriptor::default()
    }
}

struct Everyone;

impl Policy<Ctx> for Everyone {
    fn id(&self) -> &'static str {
        "demo.everyone"
    }

    fn admit(&self, _: &Ctx) -> Result<(), Problem> {
        Ok(())
    }

    async fn recheck(&self, _: &mut PgConnection, _: &Ctx) -> Result<(), Problem> {
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
        &CREATE
    }
}

static CREATE: LazyLock<ContractDoc> = LazyLock::new(|| {
    ContractDoc::input("demo.note.create", "1").field(Field::text("body").chars(1, 200))
});

#[derive(Serialize)]
struct NoteView {
    id: String,
    body: String,
}

impl Contract for NoteView {
    fn contract() -> &'static ContractDoc {
        &VIEW
    }
}

static VIEW: LazyLock<ContractDoc> = LazyLock::new(|| {
    ContractDoc::output("demo.note.view", "1")
        .field(Field::uuid("id"))
        .field(Field::text("body"))
});

/// Inserts a note and returns it; `broken` returns an `id` that is not a
/// UUID, as a handler bug would.
struct CreateNote {
    broken: bool,
}

impl Operation for CreateNote {
    type Context = Ctx;
    type Input = NewNote;
    type Output = NoteView;
    type Policy = Everyone;

    const INFO: OperationInfo = OperationInfo {
        id: "demo.note.create",
        version: "1",
        mode: Mode::Write,
        audiences: &[Audience::Product],
        summary: "Create a note.",
    };

    fn policy(&self) -> &Everyone {
        &Everyone
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        _: &Ctx,
        input: NewNote,
    ) -> Result<NoteView, Problem> {
        let id: uuid::Uuid =
            sqlx::query_scalar("INSERT INTO notes (body) VALUES ($1) RETURNING id")
                .bind(&input.body)
                .fetch_one(&mut *conn)
                .await?;
        Ok(NoteView {
            id: if self.broken {
                "note-1".into()
            } else {
                id.to_string()
            },
            body: input.body,
        })
    }
}

/// A read whose result is missing a required field.
struct ShowNote;

impl Operation for ShowNote {
    type Context = Ctx;
    type Input = NewNote;
    type Output = NoteView;
    type Policy = Everyone;

    const INFO: OperationInfo = OperationInfo {
        id: "demo.note.show",
        version: "1",
        mode: Mode::Read,
        audiences: &[Audience::Product],
        summary: "Echo a note.",
    };

    fn policy(&self) -> &Everyone {
        &Everyone
    }

    async fn run(
        &self,
        _: &mut PgConnection,
        _: &Ctx,
        input: NewNote,
    ) -> Result<NoteView, Problem> {
        Ok(NoteView {
            id: "not-a-uuid".into(),
            body: input.body,
        })
    }
}

/// A runtime-assembled write that inserts a note and returns `output(id)`.
struct DynCreate {
    output: fn(uuid::Uuid) -> Value,
}

static DYN_CREATE: LazyLock<OperationDescriptor> = LazyLock::new(|| {
    OperationDescriptor::new("demo.note.dyn_create", "1", Mode::Write)
        .audiences(&[Audience::Product])
        .summary("Create a note.")
});

impl DynOperation<()> for DynCreate {
    fn info(&self) -> &OperationDescriptor {
        &DYN_CREATE
    }

    fn policy_id(&self) -> &'static str {
        "demo.everyone"
    }

    fn context(&self) -> ContextDescriptor {
        ContextDescriptor::default()
    }

    fn input(&self) -> &'static ContractDoc {
        &CREATE
    }

    fn output(&self) -> &'static ContractDoc {
        &VIEW
    }

    fn resolve_context<'a>(
        &'a self,
        _: &'a trestle_core::invocation::Invocation,
        _: &'a (),
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
            let inserted = sqlx::query_scalar("INSERT INTO notes (body) VALUES ($1) RETURNING id")
                .bind(input["body"].as_str())
                .fetch_one(conn.conn())
                .await;
            let result = inserted.map(self.output).map_err(Problem::from);
            (conn, result)
        })
    }
}

async fn setup() -> (TestDatabase, PgPool) {
    let db = TestDatabase::create().await;
    let pool = db.pool().await;
    pool.execute(
        "CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text NOT NULL)",
    )
    .await
    .expect("create notes table");
    (db, pool)
}

async fn notes(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notes")
        .fetch_one(pool)
        .await
        .expect("count notes")
}

fn note() -> NewNote {
    NewNote {
        body: "hello".into(),
    }
}

#[tokio::test]
async fn valid_typed_output_commits() {
    let (db, pool) = setup().await;
    let view = execute(&pool, &CreateNote { broken: false }, &Ctx, note())
        .await
        .expect("valid output");
    assert_eq!(view.body, "hello");
    assert_eq!(notes(&pool).await, 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn invalid_typed_output_rolls_back_the_write() {
    let (db, pool) = setup().await;
    let problem = execute(&pool, &CreateNote { broken: true }, &Ctx, note())
        .await
        .err()
        .expect("invalid output is refused");
    assert_eq!(problem.code(), "internal_error");
    assert!(
        problem.errors().is_empty(),
        "no field details reach the caller"
    );
    assert_eq!(notes(&pool).await, 0, "the insert must not commit");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn invalid_typed_output_fails_a_read() {
    let (db, pool) = setup().await;
    let problem = execute(&pool, &ShowNote, &Ctx, note())
        .await
        .err()
        .expect("invalid output is refused");
    assert_eq!(problem.code(), "internal_error");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn invalid_dyn_output_rolls_back_the_write() {
    let (db, pool) = setup().await;
    let op = Arc::new(DynCreate {
        output: |id| json!({ "id": id, "body": "hello", "secret": "leaked" }),
    });
    let problem = execute_dyn(&pool, op, Arc::new(()), &json!({ "body": "hello" }))
        .await
        .expect_err("an undeclared field is refused");
    assert_eq!(problem.code(), "internal_error");
    assert_eq!(notes(&pool).await, 0, "the insert must not commit");

    let rendered = trestle_core::metrics::render(&[]);
    assert!(
        rendered
            .contains("trestle_operation_output_invalid_total{operation=\"demo.note.dyn_create\"}"),
        "refusals are counted per operation:\n{rendered}"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn valid_dyn_output_commits() {
    let (db, pool) = setup().await;
    let op = Arc::new(DynCreate {
        output: |id| json!({ "id": id, "body": "hello" }),
    });
    let output = execute_dyn(&pool, op, Arc::new(()), &json!({ "body": "hello" }))
        .await
        .expect("valid output");
    assert_eq!(output["body"], "hello");
    assert_eq!(notes(&pool).await, 1);
    pool.close().await;
    db.drop().await;
}
