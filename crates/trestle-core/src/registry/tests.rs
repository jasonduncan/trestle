use std::sync::LazyLock;

use axum::extract::FromRef;
use axum::routing::get;
use http::Method;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};
use trestle_protocol::{Audience, Contract, ContractDoc, Field, FormPresentation, Widget};

use super::*;
use crate::Problem;
use crate::identity::Identity;
use crate::invocation::{FromInvocation, Invocation};
use crate::operation::{ContextDescriptor, Mode, Operation, OperationInfo, Policy, RequestContext};

#[derive(Clone)]
struct TestState {
    pool: PgPool,
    identity: Identity,
}

impl FromRef<TestState> for PgPool {
    fn from_ref(state: &TestState) -> Self {
        state.pool.clone()
    }
}

impl FromRef<TestState> for Identity {
    fn from_ref(state: &TestState) -> Self {
        state.identity.clone()
    }
}

struct Ctx;

impl RequestContext for Ctx {
    fn describe() -> ContextDescriptor {
        ContextDescriptor::authenticated().header("X-Workspace", "Workspace ID.", Some("uuid"))
    }
}

impl FromInvocation<TestState> for Ctx {
    async fn resolve(invocation: &Invocation, _: &TestState) -> Result<Self, Problem> {
        invocation.actor()?;
        Ok(Ctx)
    }
}

struct Allow;

impl Policy<Ctx> for Allow {
    fn id(&self) -> &'static str {
        "notes.allow"
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
struct NoteInput {
    text: String,
}

impl Contract for NoteInput {
    fn contract() -> &'static ContractDoc {
        static CONTRACT: LazyLock<ContractDoc> = LazyLock::new(|| {
            ContractDoc::input("notes.create.input", "1.0.0")
                .field(Field::text("text").chars(1, 10))
        });
        &CONTRACT
    }
}

/// Same contract, but the Rust field is misspelled.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MisspelledInput {
    txt: String,
}

impl Contract for MisspelledInput {
    fn contract() -> &'static ContractDoc {
        NoteInput::contract()
    }
}

#[derive(Serialize)]
struct NoteView {
    id: String,
}

impl Contract for NoteView {
    fn contract() -> &'static ContractDoc {
        static CONTRACT: LazyLock<ContractDoc> =
            LazyLock::new(|| ContractDoc::output("notes.view", "1.0.0").field(Field::uuid("id")));
        &CONTRACT
    }
}

struct CreateNote;

impl Operation for CreateNote {
    type Context = Ctx;
    type Input = NoteInput;
    type Output = NoteView;
    type Policy = Allow;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.create",
        version: "1.0.0",
        mode: Mode::Write,
        audiences: &[Audience::Product],
        summary: "Create a note.",
    };

    fn policy(&self) -> &Allow {
        &Allow
    }

    async fn run(&self, _: &mut PgConnection, _: &Ctx, _: NoteInput) -> Result<NoteView, Problem> {
        Err(Problem::internal())
    }
}

struct MisspelledCreate;

impl Operation for MisspelledCreate {
    type Context = Ctx;
    type Input = MisspelledInput;
    type Output = NoteView;
    type Policy = Allow;

    const INFO: OperationInfo = OperationInfo {
        id: "notes.misspelled",
        ..CreateNote::INFO
    };

    fn policy(&self) -> &Allow {
        &Allow
    }

    async fn run(
        &self,
        _: &mut PgConnection,
        _: &Ctx,
        _: MisspelledInput,
    ) -> Result<NoteView, Problem> {
        Err(Problem::internal())
    }
}

/// An app defined by a closure, so each test states exactly what it registers.
struct TestApp<F>(&'static str, F);

impl<F> App<TestState> for TestApp<F>
where
    F: Fn(&mut AppBuilder<TestState>) + Send + Sync + 'static,
{
    fn id(&self) -> &'static str {
        self.0
    }

    fn register(&self, app: &mut AppBuilder<TestState>) {
        (self.1)(app);
    }
}

fn app(
    id: &'static str,
    register: impl Fn(&mut AppBuilder<TestState>) + Send + Sync + 'static,
) -> TestApp<impl Fn(&mut AppBuilder<TestState>) + Send + Sync + 'static> {
    TestApp(id, register)
}

fn note_form() -> FormPresentation {
    FormPresentation::for_operation("notes.create.form", "notes.create")
        .title("New note")
        .field("/text", Widget::Textarea, "Text")
}

fn notes() -> TestApp<impl Fn(&mut AppBuilder<TestState>) + Send + Sync + 'static> {
    app("notes", |b| {
        b.requires("users");
        b.operation(CreateNote)
            .http(Http::post("/notes").created("/notes/{id}"));
        b.form(note_form());
    })
}

fn users() -> TestApp<impl Fn(&mut AppBuilder<TestState>) + Send + Sync + 'static> {
    app("users", |_| {})
}

fn issues(project: Project<TestState>) -> Vec<String> {
    match project.assemble() {
        Ok(_) => Vec::new(),
        Err(err) => err.issues,
    }
}

fn assert_issue(issues: &[String], needle: &str) {
    assert!(
        issues.iter().any(|i| i.contains(needle)),
        "expected an issue containing {needle:?}, got {issues:#?}"
    );
}

#[test]
fn assembles_in_dependency_order_and_describes_itself() {
    let registry = Project::new("test", "0.1.0")
        .install(notes())
        .install(users())
        .assemble()
        .unwrap();
    let order: Vec<&str> = registry.apps().iter().map(|a| &*a.id).collect();
    assert_eq!(order, ["users", "notes"]);

    let routes = registry.routes();
    assert!(routes.contains(&RouteInfo {
        method: "POST".into(),
        path: "/notes".into(),
        kind: RouteKind::Managed {
            operation: "notes.create".into()
        },
    }));
    assert_eq!(
        routes
            .iter()
            .filter(|r| r.kind == RouteKind::System)
            .count(),
        8
    );

    let inspected = registry.inspect_operation("notes.create").unwrap();
    assert_eq!(inspected["policy"], "notes.allow");
    assert_eq!(inspected["http"]["success"], 201);
    assert_eq!(inspected["forms"][0]["id"], "notes.create.form");

    let openapi = registry.openapi(Audience::Product);
    let post = &openapi["paths"]["/notes"]["post"];
    assert_eq!(post["operationId"], "notes.create");
    assert_eq!(post["parameters"][0]["name"], "X-Workspace");
    assert_eq!(
        post["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/notes.create.input"
    );
    assert!(
        post.get("x-trestle-policy").is_none(),
        "policies are never published"
    );

    let interactions = registry.interactions_export(Audience::Product);
    assert_eq!(
        interactions["interactions"][0]["interactionId"],
        "notes.create.form"
    );
    assert!(
        registry.interactions_export(Audience::Admin)["interactions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn missing_and_cyclic_dependencies_fail() {
    assert_issue(
        &issues(Project::new("t", "0").install(notes())),
        "app notes requires users, which is not installed",
    );
    let a = app("a", |b| {
        b.requires("b");
    });
    let b = app("b", |b| {
        b.requires("a");
    });
    assert_issue(
        &issues(Project::new("t", "0").install(a).install(b)),
        "dependency cycle among apps: a, b",
    );
    assert_issue(
        &issues(Project::new("t", "0").install(users()).install(users())),
        "app users is installed twice",
    );
}

#[test]
fn apps_register_only_inside_their_namespace() {
    let intruder = app("other", |b| {
        b.operation(CreateNote);
    });
    let found = issues(Project::new("t", "0").install(intruder));
    assert_issue(
        &found,
        "operation notes.create is registered by app other but namespaced to notes",
    );
    assert_issue(&found, "uses contract notes.create.input owned by notes");
}

#[test]
fn form_references_are_checked() {
    let bad_form = app("notes", |b| {
        b.requires("users");
        b.operation(CreateNote).http(Http::post("/notes"));
        b.form(note_form().field("/title", Widget::Text, "Title"));
        b.form(FormPresentation::for_operation("notes.ghost.form", "notes.ghost").title("Ghost"));
    });
    let found = issues(Project::new("t", "0").install(users()).install(bad_form));
    assert_issue(
        &found,
        "/title is not a field of input contract notes.create.input",
    );
    assert_issue(
        &found,
        "form notes.ghost.form is bound to unknown operation notes.ghost",
    );

    let unbound = app("notes", |b| {
        b.requires("users");
        b.operation(CreateNote);
        b.form(note_form());
    });
    assert_issue(
        &issues(Project::new("t", "0").install(users()).install(unbound)),
        "has no HTTP binding to submit to",
    );
}

#[test]
fn rust_type_must_match_its_contract() {
    let mismatched = app("notes", |b| {
        b.operation(MisspelledCreate);
    });
    assert_issue(
        &issues(Project::new("t", "0").install(mismatched)),
        "operation notes.misspelled: input type does not match contract notes.create.input",
    );
}

#[test]
fn routes_must_be_unique_bound_and_waived() {
    let clash = app("notes", |b| {
        b.operation(CreateNote).http(Http::post("/notes"));
        b.unmanaged(
            Method::POST,
            "/notes",
            axum::routing::post(|| async {}),
            "legacy",
        );
        b.unmanaged(Method::GET, "/healthz", get(|| async {}), "mine");
        b.unmanaged(Method::GET, "/raw", get(|| async {}), " ");
    });
    let found = issues(Project::new("t", "0").install(clash));
    assert_issue(
        &found,
        "POST /notes is claimed by both operation notes.create and app notes",
    );
    assert_issue(&found, "GET /healthz is claimed by both system");
    assert_issue(
        &found,
        "unmanaged route GET /raw in app notes needs a named waiver",
    );

    let bad_location = app("notes", |b| {
        b.operation(CreateNote)
            .http(Http::post("/notes").created("/notes/{slug}"));
        b.operation(CreateNote).http(Http::get("/notes/{id}"));
    });
    let found = issues(Project::new("t", "0").install(bad_location));
    assert_issue(
        &found,
        "Location /notes/{slug} uses {slug}, which is not a field of notes.view",
    );
    assert_issue(&found, "operation notes.create is registered twice");
    assert_issue(
        &found,
        "path parameter {id} is not a field of notes.create.input",
    );
}

#[test]
fn waived_unmanaged_routes_are_inventoried() {
    let raw = app("notes", |b| {
        b.unmanaged(
            Method::GET,
            "/notes/export.csv",
            get(|| async { "id\n" }),
            "CSV download; reviewed 2026-09-26",
        );
    });
    let registry = Project::new("t", "0").install(raw).assemble().unwrap();
    assert!(registry.routes().iter().any(|r| r.kind
        == RouteKind::Unmanaged {
            app: "notes".into(),
            waiver: "CSV download; reviewed 2026-09-26"
        }));
    assert!(
        registry.openapi(Audience::Product)["paths"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retained_revisions_are_validated_and_accepted() {
    let current = Project::new("t", "0")
        .install(users())
        .install(notes())
        .assemble()
        .unwrap()
        .form("notes.create.form")
        .unwrap()
        .revision
        .clone();
    assert!(current.starts_with("r-") && current.len() == 18);

    let retaining = app("notes", |b| {
        b.requires("users");
        b.operation(CreateNote).http(Http::post("/notes"));
        b.form(note_form())
            .retain("r-0123456789abcdef", "before the label change");
    });
    let registry = Project::new("t", "0")
        .install(users())
        .install(retaining)
        .assemble()
        .unwrap();
    let accepted = registry
        .operation("notes.create")
        .unwrap()
        .accepted_revisions();
    assert_eq!(accepted.len(), 2);
    assert_eq!(accepted[1], "r-0123456789abcdef");

    let bad = app("notes", move |b| {
        b.requires("users");
        b.operation(CreateNote)
            .http(Http::post("/notes").created("/notes/{id}"));
        b.form(note_form())
            .retain("v1", "old")
            .retain("r-0123456789abcdef", " ");
    });
    let found = issues(Project::new("t", "0").install(users()).install(bad));
    assert_issue(
        &found,
        "retained revision \"v1\" is not of the form r-<16 hex>",
    );
    assert_issue(
        &found,
        "retained revision r-0123456789abcdef needs a reason",
    );

    let leaked: &'static str = Box::leak(current.clone().into_boxed_str());
    let self_retaining = app("notes", move |b| {
        b.requires("users");
        b.operation(CreateNote)
            .http(Http::post("/notes").created("/notes/{id}"));
        b.form(note_form()).retain(leaked, "no-op");
    });
    assert_issue(
        &issues(
            Project::new("t", "0")
                .install(users())
                .install(self_retaining),
        ),
        "is the current revision",
    );
}
