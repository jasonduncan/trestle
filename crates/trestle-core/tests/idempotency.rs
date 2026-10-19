//! ADR-016: Idempotency-Key receipts (design doc §6.4). A repeat with the
//! same key and input replays the committed answer; a different input is
//! 409; concurrent duplicates are serialized; a rolled-back attempt leaves no
//! receipt; replays are re-authorized; keys are scoped to the caller; a key
//! is refused where it is not accepted; operation jobs are exactly once.
//! Needs PostgreSQL (`TRESTLE_TEST_DATABASE_URL`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use axum::Router;
use axum::body::Body;
use axum::extract::FromRef;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Executor, PgConnection, PgPool};
use tower::ServiceExt;
use trestle_core::Problem;
use trestle_core::idempotency::{self, IdempotencyApp};
use trestle_core::identity::{ActorId, DevTokenIdentity, Identity};
use trestle_core::invocation::{FromInvocation, Invocation};
use trestle_core::jobs::{self, JobsApp, NewJob, Worker, WorkerConfig};
use trestle_core::operation::*;
use trestle_core::protocol::{Audience, Contract, ContractDoc, Field};
use trestle_core::registry::{App, AppBuilder, Http, Project, Registry};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::SchemaState;
use trestle_historical::testing::TestDatabase;
use uuid::Uuid;

const ALICE: Uuid = Uuid::from_u128(0xa11ce);
const BOB: Uuid = Uuid::from_u128(0xb0b);
const TEAM: Uuid = Uuid::from_u128(0x7ea3);
const TOKENS: &str = "alice-dev-token=00000000-0000-0000-0000-0000000a11ce,\
                      bob-dev-token=00000000-0000-0000-0000-000000000b0b";

#[derive(Clone)]
struct State {
    pool: PgPool,
    identity: Identity,
    /// Makes the next `notes.add` fail after its insert (then rolls back).
    fail_next: Arc<AtomicBool>,
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
struct NewNote {
    body: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    slow: bool,
}

impl Contract for NewNote {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::input("notes.new", "1")
                .field(Field::text("body").chars(1, 200))
                .field(Field::boolean("slow").optional())
        });
        &C
    }
}

#[derive(Serialize)]
struct Note {
    id: Uuid,
    body: String,
}

impl Contract for Note {
    fn contract() -> &'static ContractDoc {
        static C: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::output("notes.view", "1")
                .field(Field::uuid("id"))
                .field(Field::text("body"))
        });
        &C
    }
}

async fn insert(conn: &mut PgConnection, ctx: &Member, input: &NewNote) -> Result<Note, Problem> {
    if input.slow {
        sqlx::query("SELECT pg_sleep(0.3)")
            .execute(&mut *conn)
            .await?;
    }
    let id = sqlx::query_scalar(
        "INSERT INTO notes (team, body, created_by) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(ctx.team)
    .bind(&input.body)
    .bind(ctx.actor.0)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Note {
        id,
        body: input.body.clone(),
    })
}

/// Accepts an Idempotency-Key.
struct AddNote {
    fail_next: Arc<AtomicBool>,
}

impl Operation for AddNote {
    type Context = Member;
    type Input = NewNote;
    type Output = Note;
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
        input: NewNote,
    ) -> Result<Note, Problem> {
        let note = insert(conn, ctx, &input).await?;
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(Problem::unavailable("flaky"));
        }
        Ok(note)
    }
}

/// Does not accept a key.
struct PlainNote;

impl Operation for PlainNote {
    type Context = Member;
    type Input = NewNote;
    type Output = Note;
    type Policy = Members;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.plain",
        version: "1",
        mode: Mode::Write,
        audiences: &[Audience::Product],
        summary: "Add a note, no key.",
    };

    fn policy(&self) -> &Members {
        &Members
    }

    async fn run(
        &self,
        conn: &mut PgConnection,
        ctx: &Member,
        input: NewNote,
    ) -> Result<Note, Problem> {
        insert(conn, ctx, &input).await
    }
}

/// The same write, assembled at runtime.
struct DynAdd {
    info: OperationDescriptor,
}

impl DynOperation<State> for DynAdd {
    fn info(&self) -> &OperationDescriptor {
        &self.info
    }

    fn policy_id(&self) -> &str {
        "notes.members"
    }

    fn context(&self) -> ContextDescriptor {
        Member::describe()
    }

    fn input(&self) -> &ContractDoc {
        NewNote::contract()
    }

    fn output(&self) -> &ContractDoc {
        Note::contract()
    }

    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
        state: &'a State,
    ) -> BoxFuture<'a, Result<DynContext, Problem>> {
        Box::pin(
            async move { Ok(Arc::new(Member::resolve(invocation, state).await?) as DynContext) },
        )
    }

    fn admit(&self, _: &DynContext) -> Result<(), Problem> {
        Ok(())
    }

    fn recheck<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        ctx: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async move {
            Members
                .recheck(conn, ctx.downcast_ref::<Member>().unwrap())
                .await
        })
    }

    fn run(
        self: Arc<Self>,
        mut conn: DynConn,
        ctx: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)> {
        Box::pin(async move {
            let member = ctx.downcast_ref::<Member>().unwrap();
            let input = NewNote {
                body: input["body"].as_str().unwrap_or_default().to_owned(),
                slow: false,
            };
            let result = insert(conn.conn(), member, &input)
                .await
                .map(|note| json!({ "id": note.id, "body": note.body }));
            (conn, result)
        })
    }
}

struct NotesApp {
    fail_next: Arc<AtomicBool>,
}

impl App<State> for NotesApp {
    fn id(&self) -> &str {
        "notes"
    }

    fn register(&self, app: &mut AppBuilder<State>) {
        app.operation(AddNote {
            fail_next: self.fail_next.clone(),
        })
        .http(
            Http::post("/notes")
                .created("/notes/{id}")
                .idempotency_key(),
        );
        app.operation(PlainNote).http(Http::post("/plain"));
        app.dyn_operation(
            Arc::new(DynAdd {
                info: OperationDescriptor::new("notes.dyn_add", "1", Mode::Write)
                    .audiences(&[Audience::Product]),
            }),
            Some(Http::post("/dyn-notes").idempotency_key()),
        );
    }
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
        let pool = db.pool().await;
        let mut desired = SchemaState::default();
        for model in [idempotency::model(), jobs::model()] {
            let state = model.state();
            desired.models.insert(state.id.clone(), state);
        }
        let plan = diff(&SchemaState::default(), &desired, &PlanOptions::default());
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
        let ops: Vec<_> = plan.changes.into_iter().flat_map(|c| c.ops).collect();
        let sql = trestle_historical::sql::render(&SchemaState::default(), &ops).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        pool.execute(
            "CREATE TABLE members (team uuid, actor uuid, PRIMARY KEY (team, actor));
             CREATE TABLE notes (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), team uuid,
                                 body text NOT NULL, created_by uuid NOT NULL);",
        )
        .await
        .unwrap();
        for actor in [ALICE, BOB] {
            sqlx::query("INSERT INTO members VALUES ($1, $2)")
                .bind(TEAM)
                .bind(actor)
                .execute(&pool)
                .await
                .unwrap();
        }
        let fail_next = Arc::new(AtomicBool::new(false));
        let registry = Arc::new(
            Project::new("keys", "0")
                .install(IdempotencyApp)
                .install(JobsApp)
                .install(NotesApp {
                    fail_next: fail_next.clone(),
                })
                .assemble()
                .expect("assembles"),
        );
        let state = State {
            pool,
            identity: Arc::new(DevTokenIdentity::parse(TOKENS).unwrap()),
            fail_next,
        };
        let app = registry.router().with_state(state.clone());
        Self {
            db,
            state,
            registry,
            app,
        }
    }

    async fn post(&self, path: &str, who: &str, key: Option<&str>, body: Value) -> Reply {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {who}-dev-token"))
            .header("trestle-tenant", TEAM.to_string())
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    async fn notes(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM notes")
            .fetch_one(&self.state.pool)
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.state.pool.close().await;
        self.db.drop().await;
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Reply {
    fn code(&self) -> String {
        serde_json::from_str::<Value>(&self.text).unwrap()["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn replayed(&self) -> bool {
        self.headers.get("idempotency-replayed").is_some()
    }
}

fn alice() -> Invocation {
    Invocation::for_actor(ActorId(ALICE)).with_tenant(TEAM)
}

#[tokio::test]
async fn a_repeat_replays_the_committed_answer() {
    let env = Env::new().await;
    let first = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text);
    assert!(!first.replayed());
    let again = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert_eq!(again.status, StatusCode::CREATED);
    assert!(again.replayed(), "marked as a replay");
    assert_eq!(again.text, first.text, "the same bytes");
    assert_eq!(
        again.headers.get(header::LOCATION),
        first.headers.get(header::LOCATION)
    );
    assert_eq!(env.notes().await, 1, "written once");
    env.finish().await;
}

#[tokio::test]
async fn a_key_reused_with_different_input_is_a_conflict() {
    let env = Env::new().await;
    env.post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    let other = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "goodbye" }))
        .await;
    assert_eq!(
        (other.status, other.code().as_str()),
        (StatusCode::CONFLICT, "idempotency_key_reused")
    );
    assert_eq!(env.notes().await, 1);
    env.finish().await;
}

#[tokio::test]
async fn incompatible_stored_projections_are_refused_without_running_again() {
    for path in ["/notes", "/dyn-notes"] {
        let env = Env::new().await;
        let first = env
            .post(
                path,
                "alice",
                Some("projection-change"),
                json!({ "body": "hello" }),
            )
            .await;
        assert!(first.status.is_success(), "{}", first.text);
        // Model a retained answer containing a field no longer present in the
        // current output contract. The receipt and its committed effect stay.
        sqlx::query("UPDATE trestle_idempotency SET body = (body::jsonb || jsonb_build_object('retired_private', 'formerly permitted'))::text")
            .execute(&env.state.pool).await.unwrap();
        let again = env
            .post(
                path,
                "alice",
                Some("projection-change"),
                json!({ "body": "hello" }),
            )
            .await;
        assert_eq!(again.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!again.text.contains("formerly permitted"));
        assert!(!again.replayed());
        assert_eq!(env.notes().await, 1);
        env.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_duplicates_are_serialized() {
    let env = Env::new().await;
    let body = json!({ "body": "race", "slow": true });
    let (a, b) = tokio::join!(
        env.post("/notes", "alice", Some("k-race"), body.clone()),
        env.post("/notes", "alice", Some("k-race"), body.clone()),
    );
    assert_eq!(
        (a.status, b.status),
        (StatusCode::CREATED, StatusCode::CREATED),
        "{} / {}",
        a.text,
        b.text
    );
    assert_eq!(
        a.replayed() as u8 + b.replayed() as u8,
        1,
        "one ran, one replayed"
    );
    assert_eq!(a.text, b.text);
    assert_eq!(env.notes().await, 1);
    env.finish().await;
}

#[tokio::test]
async fn a_rolled_back_attempt_leaves_no_receipt() {
    let env = Env::new().await;
    env.state.fail_next.store(true, Ordering::SeqCst);
    let failed = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(env.notes().await, 0, "rolled back");
    let retried = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert_eq!(retried.status, StatusCode::CREATED);
    assert!(!retried.replayed(), "runs again: nothing had committed");
    assert_eq!(env.notes().await, 1);
    env.finish().await;
}

#[tokio::test]
async fn replays_are_reauthorized() {
    let env = Env::new().await;
    let first = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert_eq!(first.status, StatusCode::CREATED);
    sqlx::query("DELETE FROM members WHERE actor = $1")
        .bind(ALICE)
        .execute(&env.state.pool)
        .await
        .unwrap();
    let again = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert_eq!(
        again.status,
        StatusCode::FORBIDDEN,
        "not answered from the receipt"
    );
    assert!(!again.replayed());
    env.finish().await;
}

#[tokio::test]
async fn keys_are_scoped_to_the_caller() {
    let env = Env::new().await;
    let alice = env
        .post("/notes", "alice", Some("same"), json!({ "body": "hi" }))
        .await;
    let bob = env
        .post("/notes", "bob", Some("same"), json!({ "body": "hi" }))
        .await;
    assert!(!alice.replayed() && !bob.replayed());
    assert_ne!(alice.text, bob.text);
    assert_eq!(env.notes().await, 2);
    env.finish().await;
}

#[tokio::test]
async fn keys_are_refused_where_they_are_not_accepted_or_malformed() {
    let env = Env::new().await;
    let plain = env
        .post("/plain", "alice", Some("k-1"), json!({ "body": "hi" }))
        .await;
    assert_eq!(
        (plain.status, plain.code().as_str()),
        (StatusCode::BAD_REQUEST, "idempotency_unsupported")
    );
    for bad in ["has space", &"x".repeat(256)] {
        let reply = env
            .post("/notes", "alice", Some(bad), json!({ "body": "hi" }))
            .await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (StatusCode::BAD_REQUEST, "idempotency_key_invalid")
        );
    }
    assert_eq!(env.notes().await, 0);
    // Without a key both routes behave as before.
    assert_eq!(
        env.post("/notes", "alice", None, json!({ "body": "a" }))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        env.post("/notes", "alice", None, json!({ "body": "a" }))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(env.notes().await, 2, "no key, no deduplication");
    env.finish().await;
}

#[tokio::test]
async fn expired_receipts_no_longer_replay_and_can_be_purged() {
    let env = Env::new().await;
    env.post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    sqlx::query("UPDATE trestle_idempotency SET expires_at = now() - interval '1 second'")
        .execute(&env.state.pool)
        .await
        .unwrap();
    let again = env
        .post("/notes", "alice", Some("k-1"), json!({ "body": "hello" }))
        .await;
    assert!(!again.replayed(), "an expired key starts over");
    assert_eq!(env.notes().await, 2);
    sqlx::query("UPDATE trestle_idempotency SET expires_at = now() - interval '1 second'")
        .execute(&env.state.pool)
        .await
        .unwrap();
    let mut conn = env.state.pool.acquire().await.unwrap();
    assert_eq!(idempotency::purge_expired(&mut conn).await.unwrap(), 1);
    drop(conn);
    env.finish().await;
}

#[tokio::test]
async fn dynamic_operations_replay_too() {
    let env = Env::new().await;
    let first = env
        .post(
            "/dyn-notes",
            "alice",
            Some("k-dyn"),
            json!({ "body": "hi" }),
        )
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    let again = env
        .post(
            "/dyn-notes",
            "alice",
            Some("k-dyn"),
            json!({ "body": "hi" }),
        )
        .await;
    assert!(again.replayed());
    assert_eq!(again.text, first.text);
    assert_eq!(env.notes().await, 1);
    env.finish().await;
}

#[tokio::test]
async fn direct_invocation_honors_keys_where_accepted() {
    let env = Env::new().await;
    let keyed = alice().with_idempotency_key("job-like");
    let input = json!({ "body": "hi" });
    let first = env
        .registry
        .invoke(&env.state, "notes.add", &keyed, &input)
        .await
        .unwrap();
    let again = env
        .registry
        .invoke(&env.state, "notes.add", &keyed, &input)
        .await
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(env.notes().await, 1);
    let refused = env
        .registry
        .invoke(&env.state, "notes.plain", &keyed, &input)
        .await
        .expect_err("not accepted");
    assert_eq!(refused.code(), "idempotency_unsupported");
    let direct = invoke(&env.state, &PlainNote, &keyed, &input)
        .await
        .err()
        .expect("refused");
    assert_eq!(
        direct.code(),
        "idempotency_unsupported",
        "keys go through the registry"
    );
    env.finish().await;
}

#[tokio::test]
async fn operation_jobs_are_exactly_once_for_operations_that_accept_keys() {
    let env = Env::new().await;
    let mut conn = env.state.pool.acquire().await.unwrap();
    let job = jobs::enqueue(
        &mut conn,
        NewJob::operation("notes.add", json!({ "body": "later" })).on_behalf_of(&alice()),
    )
    .await
    .unwrap();
    drop(conn);
    // A first attempt that committed its write and died before recording it.
    env.registry
        .invoke(
            &env.state,
            "notes.add",
            &alice().with_idempotency_key(job.to_string()),
            &json!({ "body": "later" }),
        )
        .await
        .unwrap();
    let pass = Worker::new(
        env.registry.clone(),
        env.state.clone(),
        WorkerConfig::default(),
    )
    .run_once(1)
    .await
    .unwrap();
    assert_eq!(pass.done, 1);
    assert_eq!(
        env.notes().await,
        1,
        "the repeated attempt replayed instead of writing"
    );
    env.finish().await;
}

#[test]
fn keys_need_the_receipt_table_and_a_write() {
    struct Keyed;
    impl App<State> for Keyed {
        fn id(&self) -> &str {
            "notes"
        }
        fn register(&self, app: &mut AppBuilder<State>) {
            app.operation(PlainNote)
                .http(Http::post("/plain").idempotency_key());
        }
    }
    let Err(error) = Project::<State>::new("t", "0").install(Keyed).assemble() else {
        panic!("needs the receipt table");
    };
    assert!(
        error.issues.join("\n").contains("notes.plain accept Idempotency-Key, which needs trestle_core::idempotency::IdempotencyApp"),
        "{error}"
    );
}

#[test]
fn keyed_operations_document_the_header() {
    let registry = Project::<State>::new("t", "0")
        .install(IdempotencyApp)
        .install(JobsApp)
        .install(NotesApp {
            fail_next: Arc::default(),
        })
        .assemble()
        .unwrap();
    let openapi = registry.openapi(Audience::Product);
    let header = |path: &str| {
        openapi["paths"][path]["post"]["parameters"]
            .as_array()
            .map(|ps| ps.iter().any(|p| p["name"] == "Idempotency-Key"))
            .unwrap_or(false)
    };
    assert!(header("/notes"));
    assert!(!header("/plain"));
}
