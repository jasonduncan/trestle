//! ADR-031: Rust operations in an intent project. A `DynOperation` built
//! from the linked intent (`Project::intent_app`) checks the intent's roles
//! with `Permission` and reads and writes its models with `Ctx::run`: the
//! same tenancy, membership recheck and contracts as the generated
//! operations, for outputs that aren't a model row.

use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};
use trestle::core::FieldError;
use trestle::core::identity::ActorId;
use trestle::core::invocation::Invocation;
use trestle::core::operation::{
    BoxFuture, ContextDescriptor, DynConn, DynContext, DynOperation, Effects, Mode,
    OperationDescriptor, execute_dyn,
};
use trestle::core::registry::{AdminView, App as CoreApp, AppBuilder, Http};
use trestle::intent::{Ctx, Permission, Resolved};
use trestle::prelude::*;
use trestle::protocol::{Audience, ContractDoc, Field};
use trestle::testing::TestApp;
use uuid::Uuid;

const NOTES: &str = include_str!("../examples/notes_intent/trestle.toml");
const MIGRATIONS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/notes_intent/migrations"
);
const MEMBER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const VIEWER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const STRANGER: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
const TOKENS: &str = "member-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa,\
viewer-token=bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb,\
stranger-token=cccccccc-cccc-4ccc-8ccc-cccccccccccc";

#[derive(Clone, Copy)]
enum Does {
    /// Counts the tenant's notes: an output that isn't a row.
    Report,
    /// Creates a note, then returns `{id}`, or breaks its output contract,
    /// or fails, as `then` says.
    Quick,
    /// A read that tries to write.
    Sneak,
    /// The quick-note resource's list: a page of `{id, title}`.
    List,
}

struct NoteOp {
    info: OperationDescriptor,
    permission: Permission,
    resolved: Arc<Resolved>,
    input: ContractDoc,
    output: ContractDoc,
    does: Does,
}

fn counts(id: &str) -> ContractDoc {
    ContractDoc::output(id, "1")
        .field(Field::integer("count"))
        .field(Field::integer("pinned"))
}

impl NoteOp {
    fn build(resolved: &Arc<Resolved>, does: Does) -> Arc<dyn DynOperation<State>> {
        let (id, mode, permission, input, output, effects) = match does {
            Does::Report => (
                "reports.notes",
                Mode::Read,
                "notes.read",
                ContractDoc::input("reports.notes", "1"),
                counts("reports.notes.view"),
                Effects::new().reads("notes.note"),
            ),
            Does::Quick => (
                "reports.quick.create",
                Mode::Write,
                "notes.write",
                ContractDoc::input("reports.quick.create", "1")
                    .field(Field::text("title"))
                    .field(Field::text("then").optional()),
                ContractDoc::output("reports.quick.view", "1").field(Field::uuid("id")),
                Effects::new().writes("notes.note"),
            ),
            Does::List => (
                "reports.quick.list",
                Mode::Read,
                "notes.read",
                ContractDoc::input("reports.quick.list", "1"),
                ContractDoc::output("reports.quick.page", "1")
                    .field(Field::items(
                        "items",
                        &ContractDoc::output("reports.quick.item", "1")
                            .field(Field::uuid("id"))
                            .field(Field::text("title")),
                        100,
                    ))
                    .field(Field::text("next_cursor").nullable()),
                Effects::new().reads("notes.note"),
            ),
            Does::Sneak => (
                "reports.sneak",
                Mode::Read,
                "notes.read",
                ContractDoc::input("reports.sneak", "1"),
                counts("reports.sneak.view"),
                Effects::new().reads("notes.note"),
            ),
        };
        Arc::new(NoteOp {
            info: OperationDescriptor::new(id, "1", mode)
                .audiences(&[Audience::Product])
                .effects(effects),
            permission: Permission::new(resolved, permission).unwrap(),
            resolved: resolved.clone(),
            input,
            output,
            does,
        })
    }
}

impl DynOperation<State> for NoteOp {
    fn info(&self) -> &OperationDescriptor {
        &self.info
    }

    fn policy_id(&self) -> &str {
        self.permission.id()
    }

    fn context(&self) -> ContextDescriptor {
        self.permission.context()
    }

    fn input(&self) -> &ContractDoc {
        &self.input
    }

    fn output(&self) -> &ContractDoc {
        &self.output
    }

    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
        state: &'a State,
    ) -> BoxFuture<'a, Result<DynContext, Problem>> {
        Box::pin(self.permission.resolve(invocation, state))
    }

    fn admit(&self, ctx: &DynContext) -> Result<(), Problem> {
        self.permission.admit(ctx)
    }

    fn recheck<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        ctx: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(self.permission.recheck(conn, ctx))
    }

    fn run(
        self: Arc<Self>,
        conn: DynConn,
        ctx: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)> {
        Box::pin(async move {
            let resolved = self.resolved.clone();
            Ctx::run(&resolved, conn, &ctx, async |data| match self.does {
                Does::Report => {
                    let notes = data.list("notes.note", json!({})).await?;
                    let pinned = notes
                        .iter()
                        .filter(|n| n.get::<bool>("pinned").unwrap_or(false))
                        .count();
                    Ok(json!({ "count": notes.len(), "pinned": pinned }))
                }
                Does::Quick => {
                    let note = data
                        .create("notes.note", json!({ "title": input["title"] }))
                        .await?;
                    match input["then"].as_str() {
                        Some("break") => Ok(json!({ "id": 5 })),
                        Some("fail") => Err(Problem::validation(vec![FieldError::new(
                            "/then",
                            "refused",
                            "Refused after writing.",
                        )])),
                        _ => Ok(json!({ "id": note.id() })),
                    }
                }
                Does::List => {
                    let notes = data.list("notes.note", json!({})).await?;
                    let items = notes
                        .iter()
                        .map(|n| Ok(json!({ "id": n.id(), "title": n.get::<String>("title")? })))
                        .collect::<Result<Vec<_>, Problem>>()?;
                    Ok(json!({ "items": items, "next_cursor": null }))
                }
                Does::Sneak => {
                    data.create("notes.note", json!({ "title": "sneaky" }))
                        .await?;
                    Ok(json!({ "count": 0, "pinned": 0 }))
                }
            })
            .await
        })
    }
}

/// The Rust app: a report route, an unrouted read that tries to write, and
/// a quick-note resource in the admin shell behind `notes.write`.
struct Reports {
    resolved: Arc<Resolved>,
}

impl CoreApp<State> for Reports {
    fn id(&self) -> &str {
        "reports"
    }

    fn register(&self, app: &mut AppBuilder<State>) {
        app.dyn_operation(
            NoteOp::build(&self.resolved, Does::Report),
            Some(Http::get("/api/v1/reports/notes")),
        );
        app.dyn_operation(NoteOp::build(&self.resolved, Does::Sneak), None);
        app.resource("reports.quick", "/api/v1/quick-notes")
            .list(NoteOp::build(&self.resolved, Does::List))
            .create(NoteOp::build(&self.resolved, Does::Quick))
            .admin_dyn(
                AdminView::new("Quick notes", "title").columns(&["title"]),
                Arc::new(Permission::new(&self.resolved, "notes.write").unwrap()),
            );
    }
}

async fn tidy(_ctx: Ctx, row: Row) -> Result<Row> {
    Ok(row)
}

async fn pin(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("notes.note", input.id()?, json!({ "pinned": true }))
        .await
}

fn project(linked: Arc<OnceLock<Arc<Resolved>>>) -> trestle::Project {
    trestle::project()
        .intent_str("trestle.toml", NOTES)
        .handler("notes::pin", pin)
        .hook("notes::tidy", tidy)
        .migrations_dir(MIGRATIONS)
        .intent_app(move |resolved| {
            let _ = linked.set(resolved.clone());
            Reports {
                resolved: resolved.clone(),
            }
        })
}

struct Env {
    app: TestApp,
    resolved: Arc<Resolved>,
    tenant: Uuid,
}

impl Env {
    async fn start() -> Self {
        let linked = Arc::new(OnceLock::new());
        let app = TestApp::start(project(linked.clone()), TOKENS).await;
        let tenant = app.tenant("North").await;
        let other = app.tenant("South").await;
        app.grant(tenant, MEMBER.parse().unwrap(), "member").await;
        app.grant(tenant, VIEWER.parse().unwrap(), "viewer").await;
        app.grant(other, STRANGER.parse().unwrap(), "member").await;
        Env {
            app,
            resolved: linked.get().expect("the intent app was built").clone(),
            tenant,
        }
    }

    fn caller(&self, actor: &str) -> Invocation {
        Invocation::for_actor(ActorId(actor.parse().unwrap())).with_tenant(self.tenant)
    }

    async fn notes(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM notes")
            .fetch_one(self.app.pool())
            .await
            .unwrap()
    }
}

#[test]
fn a_permission_must_be_granted_by_a_role() {
    let linked = Arc::new(OnceLock::new());
    project(linked.clone())
        .assemble()
        .unwrap_or_else(|e| panic!("{e}"));
    let resolved = linked.get().unwrap();
    assert!(Permission::new(resolved, "notes.read").is_ok());
    assert!(Permission::new(resolved, trestle::intent::AUTHENTICATED).is_ok());
    let err = Permission::new(resolved, "notes.delete_everything")
        .err()
        .unwrap();
    assert!(err.contains("notes.delete_everything"), "{err}");
    assert_eq!(
        Permission::new(resolved, "notes.write").unwrap().id(),
        "intent.permission.notes.write",
        "the same policy ID as the generated operations"
    );
}

#[test]
fn an_intent_app_needs_an_intent() {
    let issues = match trestle::project()
        .without_intent()
        .intent_app(|resolved| Reports {
            resolved: resolved.clone(),
        })
        .assemble()
    {
        Ok(_) => panic!("assembled without an intent"),
        Err(e) => e.issues,
    };
    assert!(
        issues.iter().any(|i| i.contains("intent app")),
        "{issues:?}"
    );
}

/// HTTP and `Registry::invoke` reach the same decisions, with the intent's
/// roles and tenancy, for an output that isn't a model row.
#[tokio::test]
async fn rust_operations_use_the_intents_roles_and_tenancy() {
    let env = Env::start().await;
    let t = env.tenant;

    let created = env
        .app
        .post("/api/v1/quick-notes", json!({ "title": "Quick" }))
        .token("member-token")
        .tenant(t)
        .send()
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let id = created.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        created.headers["location"].to_str().unwrap(),
        format!("/api/v1/quick-notes/{id}")
    );
    let pinned = env
        .app
        .post(&format!("/api/v1/notes/{id}/pin"), json!({}))
        .token("member-token")
        .tenant(t)
        .send()
        .await;
    assert_eq!(pinned.status, 200, "a generated route sees the note");

    let report = env
        .app
        .get("/api/v1/reports/notes")
        .token("viewer-token")
        .tenant(t)
        .send()
        .await;
    assert_eq!(report.status, 200, "{}", report.body);
    assert_eq!(report.body, json!({ "count": 1, "pinned": 1 }));
    let direct = env
        .app
        .invoke("reports.notes", &env.caller(VIEWER), &json!({}))
        .await
        .unwrap();
    assert_eq!(direct, report.body);

    for (who, token, actor, code) in [
        ("viewer", "viewer-token", VIEWER, "permission_denied"),
        (
            "stranger",
            "stranger-token",
            STRANGER,
            "tenant_not_permitted",
        ),
    ] {
        let over_http = env
            .app
            .post("/api/v1/quick-notes", json!({ "title": "No" }))
            .token(token)
            .tenant(t)
            .send()
            .await;
        assert_eq!(over_http.status, 403, "{who}: {}", over_http.body);
        assert_eq!(over_http.body["code"], code, "{who} over HTTP");
        let direct = env
            .app
            .invoke(
                "reports.quick.create",
                &env.caller(actor),
                &json!({ "title": "No" }),
            )
            .await
            .expect_err(who);
        assert_eq!(direct.code(), code, "{who} directly");
    }
    let stranger_report = env
        .app
        .get("/api/v1/reports/notes")
        .token("stranger-token")
        .tenant(t)
        .send()
        .await;
    assert_eq!(stranger_report.status, 403);
    assert_eq!(env.notes().await, 1, "refusals wrote nothing");
    env.app.finish().await;
}

/// Writes through `Ctx::run` are the operation's own transaction: a failed
/// operation or a refused output (ADR-012) leaves nothing, and a read can't
/// write at all.
#[tokio::test]
async fn refused_and_invalid_writes_leave_nothing() {
    let env = Env::start().await;
    let member = env.caller(MEMBER);

    let failed = env
        .app
        .invoke(
            "reports.quick.create",
            &member,
            &json!({ "title": "A", "then": "fail" }),
        )
        .await
        .expect_err("fails after writing");
    assert_eq!(
        (failed.status().as_u16(), failed.code()),
        (422, "validation_failed")
    );

    let broken = env
        .app
        .post(
            "/api/v1/quick-notes",
            json!({ "title": "B", "then": "break" }),
        )
        .token("member-token")
        .tenant(env.tenant)
        .send()
        .await;
    assert_eq!(broken.status, 500, "{}", broken.body);
    assert!(
        !broken.body.to_string().contains("\"id\":5"),
        "no output on the wire: {}",
        broken.body
    );

    let sneak = env
        .app
        .invoke("reports.sneak", &member, &json!({}))
        .await
        .expect_err("a read can't write");
    assert_eq!(sneak.status().as_u16(), 500);

    // The handle refuses by itself, even on a plain pooled connection with
    // no read-only transaction to stop it.
    let pool = env.app.pool().clone();
    let permission = Permission::new(&env.resolved, "notes.read").unwrap();
    let ctx = permission.resolve(&member, &pool).await.unwrap();
    let conn = DynConn::Read(pool.acquire().await.unwrap());
    let (_, refused) = Ctx::run(&env.resolved, conn, &ctx, async |data| {
        data.create("notes.note", json!({ "title": "pooled" }))
            .await
    })
    .await;
    assert_eq!(refused.expect_err("read handle").status().as_u16(), 500);
    assert_eq!(env.notes().await, 0);
    env.app.finish().await;
}

/// A context resolved before the membership changed is rechecked inside
/// the write's transaction: a demoted or removed member writes nothing.
#[tokio::test]
async fn a_stale_context_is_rechecked_in_the_transaction() {
    let env = Env::start().await;
    let pool = env.app.pool().clone();
    let op = NoteOp::build(&env.resolved, Does::Quick);
    let permission = Permission::new(&env.resolved, "notes.write").unwrap();
    let stale = permission
        .resolve(&env.caller(MEMBER), &pool)
        .await
        .unwrap();
    permission.admit(&stale).expect("a member when resolved");

    sqlx::query("UPDATE workspace_memberships SET role = 'viewer' WHERE actor_id = $1")
        .bind(Uuid::parse_str(MEMBER).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let demoted = execute_dyn(&pool, op.clone(), stale.clone(), &json!({ "title": "A" }))
        .await
        .expect_err("demoted since");
    assert_eq!(demoted.code(), "permission_denied");

    sqlx::query("DELETE FROM workspace_memberships WHERE actor_id = $1")
        .bind(Uuid::parse_str(MEMBER).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let removed = execute_dyn(&pool, op, stale, &json!({ "title": "B" }))
        .await
        .expect_err("removed since");
    assert_eq!(removed.code(), "tenant_not_permitted");
    assert_eq!(env.notes().await, 0);
    env.app.finish().await;
}

/// A hand-written operation behind `ResourceOp` and `admin_dyn`: the admin
/// view's `Permission` decides who sees it, and the button's operation
/// still checks its own.
#[tokio::test]
async fn the_admin_view_shows_and_the_operation_authorizes() {
    let env = Env::start().await;
    let manifest = env
        .app
        .get("/_meta/v1/admin/manifest")
        .token("member-token")
        .tenant(env.tenant)
        .send()
        .await;
    assert_eq!(manifest.status, 200, "{}", manifest.body);
    let resources = manifest.body["resources"].as_array().unwrap();
    assert!(
        resources.iter().any(|r| r["id"] == "reports.quick"),
        "{resources:?}"
    );
    let hidden = env
        .app
        .get("/_meta/v1/admin/manifest")
        .token("viewer-token")
        .tenant(env.tenant)
        .send()
        .await;
    assert_eq!(hidden.status, 403, "{}", hidden.body);

    // Showing grants nothing: the button's operation checks notes.write.
    let viewer = env
        .app
        .post("/api/v1/quick-notes", json!({ "title": "No" }))
        .token("viewer-token")
        .tenant(env.tenant)
        .send()
        .await;
    assert_eq!(viewer.status, 403, "{}", viewer.body);
    let listed = env
        .app
        .get("/api/v1/quick-notes")
        .token("viewer-token")
        .tenant(env.tenant)
        .send()
        .await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(listed.body["items"], json!([]));
    env.app.finish().await;
}
