//! F3: the intent file. Parsing and its error messages, handler linking,
//! and a CRUD server built from `trestle.toml` against Postgres: tenancy,
//! roles, If-Match, hooks, a Rust handler.

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use trestle::core::identity::DevTokenIdentity;
use trestle::historical::load;
use trestle::historical::testing::TestDatabase;
use trestle::prelude::*;

const NOTES: &str = include_str!("../examples/notes_intent/trestle.toml");
const MIGRATIONS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/notes_intent/migrations"
);

async fn pin(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("notes.note", input.id()?, json!({ "pinned": true }))
        .await
}

async fn tidy(_ctx: Ctx, mut note: Row) -> Result<Row> {
    let title: String = note.get("title")?;
    if title.trim().is_empty() {
        return Err(Problem::validation(vec![trestle::core::FieldError::new(
            "/title",
            "blank",
            "Give the note a title.",
        )]));
    }
    note.set("title", title.trim())?;
    Ok(note)
}

fn notes() -> trestle::Project {
    trestle::project()
        .intent_str("trestle.toml", NOTES)
        .handler("notes::pin", pin)
        .hook("notes::tidy", tidy)
        .migrations_dir(MIGRATIONS)
}

fn problems(text: &str) -> Vec<String> {
    trestle::intent::parse("trestle.toml", text)
        .unwrap_err()
        .problems
}

#[test]
fn the_design_example_parses() {
    let intent = trestle::intent::parse("trestle.toml", NOTES).unwrap();
    assert_eq!(intent.name, "notes");
    assert!(intent.auth.passwords);
    assert_eq!(intent.auth.oidc, None);
    assert_eq!(intent.models.len(), 1);
    assert_eq!(intent.endpoints.len(), 2);
}

#[test]
fn unknown_keys_are_errors_with_a_line() {
    let p = problems("[project]\nname = \"x\"\ncolour = 1\n");
    assert_eq!(p.len(), 1);
    assert!(p[0].starts_with("trestle.toml:3"), "{p:?}");
    assert!(p[0].contains("colour"), "{p:?}");
}

#[test]
fn every_problem_is_reported_with_its_line() {
    let text = r#"
[project]
name = "x"

[tenancy]
model = "team"

[roles]
member = ["x.read"]

[[models]]
id = "Bad"
fields = []

[[models]]
id = "x.item"
fields = [{ id = "size", type = "float" }, { id = "version", type = "int" }]

[[endpoints]]
model = "x.item"
crud = ["list", "explode"]
read = "x.write"

[[endpoints]]
id = "x.go"
path = "POST /api/v1/items/{id}/go"
handler = "x::go"
"#;
    let p = problems(text).join("\n");
    assert!(p.contains("trestle.toml:12: model id \"Bad\""), "{p}");
    assert!(p.contains("trestle.toml:17: unknown type \"float\""), "{p}");
    assert!(p.contains("field \"version\" is reserved"), "{p}");
    assert!(p.contains("unknown crud operation \"explode\""), "{p}");
    assert!(
        p.contains("permission \"x.write\" is not granted by any role"),
        "{p}"
    );
    assert!(p.contains("endpoint \"x.go\" needs a permission"), "{p}");
}

/// ADR-042: identifiers are emitted unquoted, so a field or table that is
/// a reserved SQL word (or, for a table, a type name) is refused when the
/// file is read, with its line and a suggestion.
#[test]
fn reserved_sql_words_are_refused_with_their_line() {
    let text = r#"
[project]
name = "shop"

[[models]]
id = "shop.item"
fields = [
  { id = "status", type = "text" },
  { id = "position", type = "int" },
  { id = "primary", type = "bool" },
  { id = "order", type = "int" },
  { id = "user", type = "uuid", optional = true },
]

[[models]]
id = "shop.group"
table = "group"
fields = [{ id = "end", type = "instant" }]

[[models]]
id = "shop.note"
table = "text"
fields = [{ id = "body", type = "text" }]
"#;
    assert_eq!(
        problems(text),
        [
            r#"trestle.toml:10: field "primary" is a reserved SQL word; choose another ID, e.g. "is_primary""#,
            r#"trestle.toml:11: field "order" is a reserved SQL word; choose another ID, e.g. "item_order""#,
            r#"trestle.toml:12: field "user" is a reserved SQL word; choose another ID, e.g. "user_id""#,
            r#"trestle.toml:17: table "group" is a reserved SQL word; choose another table, e.g. table = "groups""#,
            r#"trestle.toml:18: field "end" is a reserved SQL word; choose another ID, e.g. "end_at""#,
            r#"trestle.toml:22: table "text" is a PostgreSQL type name, which SQL would read as the type; choose another table, e.g. table = "texts""#,
        ]
    );
    // Non-reserved key words (`status`, `position`) stay usable as fields,
    // and the default plural tables are fine.
    let fine = text
        .replace("primary", "is_primary")
        .replace(r#""order""#, r#""rank""#)
        .replace(r#""user""#, r#""owner""#)
        .replace(r#"table = "group""#, "")
        .replace(r#""end""#, r#""ends_at""#)
        .replace(r#"table = "text""#, "");
    trestle::intent::parse("trestle.toml", &fine).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn permissions_other_than_authenticated_need_tenancy() {
    let text = r#"
[project]
name = "x"

[[models]]
id = "x.item"
fields = [{ id = "name", type = "text" }]

[[endpoints]]
model = "x.item"
crud = ["get"]
read = "authenticated"
"#;
    trestle::intent::parse("t", text).unwrap();
    let p = problems(&text.replace("[project]", "[roles]\nr = [\"x.read\"]\n\n[project]"));
    assert!(
        p.iter().any(|p| p.contains("[roles] needs [tenancy]")),
        "{p:?}"
    );
}

#[test]
fn handlers_are_linked_by_name_both_ways() {
    let missing = trestle::project()
        .intent_str("trestle.toml", NOTES)
        .hook("notes::tidy", tidy)
        .assemble()
        .err()
        .unwrap()
        .to_string();
    assert!(
        missing.contains(
            "trestle.toml:37 endpoint notes.pin names handler \"notes::pin\", which is not registered; add .handler(\"notes::pin\", pin) in src/main.rs"
        ),
        "{missing}"
    );
    let unused = notes()
        .handler("notes::extra", pin)
        .assemble()
        .err()
        .unwrap()
        .to_string();
    assert!(
        unused.contains("handler \"notes::extra\" is registered but no endpoint"),
        "{unused}"
    );
}

#[test]
fn the_intent_feeds_the_registry() {
    let registry = notes().assemble().unwrap();
    assert_eq!(registry.name(), "notes");
    let ops: Vec<&str> = registry.operations().iter().map(|o| &*o.info.id).collect();
    assert_eq!(
        ops,
        [
            "notes.note.list",
            "notes.note.get",
            "notes.note.create",
            "notes.note.update",
            "notes.note.delete",
            "notes.pin"
        ]
    );
    assert!(registry.desired_schema().model("notes.note").is_some());
    assert!(
        registry
            .desired_schema()
            .model("workspace.membership")
            .is_some()
    );
    let openapi = registry.openapi(trestle::protocol::Audience::Product);
    assert!(openapi["paths"]["/api/v1/notes/{id}/pin"]["post"].is_object());

    // Generated CRUD declares its model (ADR-018), except where a hook
    // (Rust code) runs: `validate` runs on create and update.
    let effects = |id: &str| {
        registry
            .inspect_operation(id)
            .unwrap()
            .get("effects")
            .cloned()
            .unwrap()
    };
    assert_eq!(
        effects("notes.note.list")["reads"],
        serde_json::json!(["notes.note"])
    );
    assert_eq!(
        effects("notes.note.get")["reads"],
        serde_json::json!(["notes.note"])
    );
    assert_eq!(
        effects("notes.note.delete")["writes"],
        serde_json::json!(["notes.note"])
    );
    for hooked in ["notes.note.create", "notes.note.update", "notes.pin"] {
        assert!(effects(hooked).is_null(), "{hooked}");
    }
}

struct Server {
    app: Router,
    db: TestDatabase,
    registry: trestle::core::registry::Registry<State>,
    state: State,
    tenant: uuid::Uuid,
    other: uuid::Uuid,
}

const MEMBER: &str = "member-token";
const VIEWER: &str = "viewer-token";
const STRANGER: &str = "stranger-token";
const MEMBER_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const VIEWER_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const STRANGER_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

impl Server {
    async fn start() -> Self {
        Self::start_with(notes()).await
    }

    async fn start_with(project: trestle::Project) -> Self {
        let db = TestDatabase::create().await;
        let set = load::history_from_dir(Path::new(MIGRATIONS), &[]).unwrap();
        trestle::historical::migrate(db.options(), &set)
            .await
            .unwrap();
        let pool = db.pool().await;
        let tenant = uuid::Uuid::now_v7();
        let other = uuid::Uuid::now_v7();
        for (id, name) in [(tenant, "North"), (other, "South")] {
            sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, $2)")
                .bind(id)
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        for (ws, actor, role) in [
            (tenant, MEMBER_ID, "member"),
            (tenant, VIEWER_ID, "viewer"),
            (other, STRANGER_ID, "member"),
        ] {
            sqlx::query(
                "INSERT INTO workspace_memberships (workspace_id, actor_id, role) VALUES ($1, $2, $3)",
            )
            .bind(ws)
            .bind(uuid::Uuid::parse_str(actor).unwrap())
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
        }
        let tokens = format!("{MEMBER}={MEMBER_ID},{VIEWER}={VIEWER_ID},{STRANGER}={STRANGER_ID}");
        let identity = Arc::new(DevTokenIdentity::parse(&tokens).unwrap());
        let registry = project.assemble().unwrap();
        let state = State::new(pool, identity);
        let app = trestle::router_with(&registry, state.clone(), Default::default());
        Server {
            app,
            db,
            registry,
            state,
            tenant,
            other,
        }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        tenant: uuid::Uuid,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("Trestle-Tenant", tenant.to_string());
        if let Some(tag) = if_match {
            req = req.header(header::IF_MATCH, tag);
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let etag = res
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json, etag)
    }
}

/// ADR-013: intent operations invoked by ID (as a job would) resolve the same
/// membership and reach the same decisions as their HTTP routes.
#[tokio::test]
async fn intent_operations_decide_the_same_without_http() {
    use trestle::core::identity::ActorId;
    use trestle::core::invocation::Invocation;

    let s = Server::start().await;
    let as_actor = |id: &str, tenant| {
        Invocation::for_actor(ActorId(uuid::Uuid::parse_str(id).unwrap())).with_tenant(tenant)
    };
    let invoke = |invocation: Invocation, op: &'static str, input: Value| {
        let s = &s;
        async move { s.registry.invoke(&s.state, op, &invocation, &input).await }
    };

    let note = invoke(
        as_actor(MEMBER_ID, s.tenant),
        "notes.note.create",
        json!({"title": "  Direct  "}),
    )
    .await
    .expect("a member creates");
    assert_eq!(note["title"], "Direct", "hooks run for direct callers too");

    for (who, token, id, tenant, code) in [
        ("viewer", VIEWER, VIEWER_ID, s.tenant, "permission_denied"),
        (
            "member elsewhere",
            STRANGER,
            STRANGER_ID,
            s.tenant,
            "tenant_not_permitted",
        ),
    ] {
        let (status, body, _) = s
            .call(
                "POST",
                "/api/v1/notes",
                token,
                tenant,
                Some(json!({"title": "x"})),
                None,
            )
            .await;
        let problem = invoke(
            as_actor(id, tenant),
            "notes.note.create",
            json!({"title": "x"}),
        )
        .await
        .expect_err(who);
        assert_eq!(status, StatusCode::FORBIDDEN, "{who}: {body}");
        assert_eq!(body["code"], code, "{who} over HTTP");
        assert_eq!(
            (problem.status(), problem.code()),
            (StatusCode::FORBIDDEN, code),
            "{who} directly"
        );
    }

    // What the direct caller wrote is what HTTP lists.
    let (status, list, _) = s
        .call("GET", "/api/v1/notes", VIEWER, s.tenant, None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(list["items"][0]["id"], note["id"]);
    s.db.drop().await;
}

#[tokio::test]
async fn a_crud_server_from_the_intent_file() {
    let s = Server::start().await;
    let t = s.tenant;

    // Create: the validate hook trims the title; defaults apply.
    let (status, note, etag) = s
        .call(
            "POST",
            "/api/v1/notes",
            MEMBER,
            t,
            Some(json!({"title": "  Hello  "})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{note}");
    assert_eq!(note["title"], "Hello");
    assert_eq!(note["pinned"], false);
    assert_eq!(note["version"], 1);
    let id = note["id"].as_str().unwrap().to_owned();
    assert_eq!(etag.as_deref(), Some("\"notes.note:1\""));

    // Unknown fields and contract violations are rejected.
    let (status, body, _) = s
        .call(
            "POST",
            "/api/v1/notes",
            MEMBER,
            t,
            Some(json!({"title": "x", "owner": "me"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, _, _) = s
        .call(
            "POST",
            "/api/v1/notes",
            MEMBER,
            t,
            Some(json!({"title": "   "})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "hook validation");

    // Viewers read but cannot write; other tenants' members see nothing.
    let (status, list, _) = s.call("GET", "/api/v1/notes", VIEWER, t, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let (status, _, _) = s
        .call(
            "POST",
            "/api/v1/notes",
            VIEWER,
            t,
            Some(json!({"title": "no"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = s
        .call("GET", "/api/v1/notes", STRANGER, t, None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "not a member of this tenant");
    let (status, _, _) = s
        .call(
            "GET",
            &format!("/api/v1/notes/{id}"),
            STRANGER,
            s.other,
            None,
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "other tenants' records are 404"
    );

    // Update needs the current version.
    let uri = format!("/api/v1/notes/{id}");
    let (status, _, _) = s
        .call(
            "PATCH",
            &uri,
            MEMBER,
            t,
            Some(json!({"body": "text"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
    let (status, _, _) = s
        .call(
            "PATCH",
            &uri,
            MEMBER,
            t,
            Some(json!({"body": "text"})),
            Some("\"notes.note:9\""),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let (status, note, _) = s
        .call(
            "PATCH",
            &uri,
            MEMBER,
            t,
            Some(json!({"body": "text"})),
            Some("\"notes.note:1\""),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{note}");
    assert_eq!(
        (note["body"].clone(), note["version"].clone()),
        (json!("text"), json!(2))
    );

    // The Rust handler.
    let (status, note, _) = s
        .call(
            "POST",
            &format!("{uri}/pin"),
            MEMBER,
            t,
            Some(json!({})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{note}");
    assert_eq!(note["pinned"], true);

    // Filters.
    let (_, pinned, _) = s
        .call("GET", "/api/v1/notes?pinned=true", MEMBER, t, None, None)
        .await;
    assert_eq!(pinned["items"].as_array().unwrap().len(), 1);
    let (_, unpinned, _) = s
        .call("GET", "/api/v1/notes?pinned=false", MEMBER, t, None, None)
        .await;
    assert!(unpinned["items"].as_array().unwrap().is_empty());

    // Delete.
    let (status, _, _) = s
        .call("DELETE", &uri, MEMBER, t, None, Some("\"notes.note:3\""))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = s.call("GET", &uri, MEMBER, t, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    s.db.drop().await;
}

fn auth(section: &str) -> trestle::intent::AuthSection {
    let text = format!("[project]\nname = \"x\"\n\n[auth]\n{section}\n");
    trestle::intent::parse("trestle.toml", &text).unwrap().auth
}

#[test]
fn passwords_are_on_by_default_and_oidc_is_opt_in() {
    let mut config = trestle::ServeConfig::for_tests();
    assert!(config.apply_auth(&auth(""), |_| None).is_empty());
    assert!(config.password_login);
    assert_eq!(config.oidc_issuer, None);

    let mut config = trestle::ServeConfig::for_tests();
    config.password_login = true;
    config.apply_auth(&auth("passwords = false"), |_| None);
    assert!(
        !config.password_login,
        "the intent file turns passwords off"
    );
}

#[test]
fn google_reads_its_client_from_named_variables() {
    let google = auth("oidc = \"google\"");
    let mut config = trestle::ServeConfig::for_tests();
    let problems = config.apply_auth(&google, |_| None);
    assert_eq!(
        problems,
        [
            "GOOGLE_CLIENT_ID is required by [auth] oidc",
            "GOOGLE_CLIENT_SECRET is required by [auth] oidc"
        ]
    );
    let mut config = trestle::ServeConfig::for_tests();
    let problems = config.apply_auth(&google, |k| Some(format!("value-of-{k}")));
    assert!(problems.is_empty());
    assert_eq!(
        config.oidc_issuer.as_deref(),
        Some("https://accounts.google.com")
    );
    assert_eq!(
        config.oidc_client_id.as_deref(),
        Some("value-of-GOOGLE_CLIENT_ID")
    );
}

#[test]
fn a_custom_provider_names_variables_not_values() {
    let custom = auth(
        "oidc = { issuer = \"https://id.example.com\", client_id_env = \"ID_CLIENT\", client_secret_env = \"ID_SECRET\" }",
    );
    let mut config = trestle::ServeConfig::for_tests();
    assert!(config.apply_auth(&custom, |_| Some("v".into())).is_empty());
    assert_eq!(
        config.oidc_issuer.as_deref(),
        Some("https://id.example.com")
    );

    let bad = |section: &str| {
        let text = format!("[project]\nname = \"x\"\n\n[auth]\n{section}\n");
        trestle::intent::parse("trestle.toml", &text)
            .unwrap_err()
            .problems
            .join("\n")
    };
    assert!(bad("oidc = \"okta\"").contains("unknown OpenID Connect provider \"okta\""));
    assert!(
        bad("oidc = { issuer = \"http://id.example.com\", client_id_env = \"A\", client_secret_env = \"B\" }")
            .contains("must be an https URL")
    );
    assert!(
        bad("oidc = { issuer = \"https://id.example.com\", client_id_env = \"abc123secret\", client_secret_env = \"B\" }")
            .contains("never their values")
    );
    assert!(!bad("oidc = { issuer = \"https://id.example.com\" }").is_empty());
}

#[test]
fn unique_sets_are_named_per_tenant_and_checked() {
    let text = |unique: &str| {
        format!(
            "[project]\nname = \"x\"\n\n[tenancy]\nmodel = \"team\"\n\n[roles]\nm = [\"x.read\"]\n\n\
             [[models]]\nid = \"x.item\"\nfields = [{{ id = \"name\", type = \"text\" }}]\nunique = {unique}\n"
        )
    };
    let intent = trestle::intent::parse("t", &text("[[\"name\"]]")).unwrap();
    assert_eq!(intent.models[0].unique[0].name, "items_team_name_key");
    let p = problems(&text("[[\"nope\"]]")).join("\n");
    assert!(
        p.contains("trestle.toml:13: unique field \"nope\" is not a field of x.item"),
        "{p}"
    );
}

// Hardening (v0.2 PR 3) --------------------------------------------------

const EXTRA: &str = r#"
[[endpoints]]
id = "notes.sneaky"
path = "GET /api/v1/notes/{id}/sneaky"
permission = "notes.read"
handler = "notes::sneaky"

[[endpoints]]
id = "notes.bad_list"
path = "POST /api/v1/notes/{id}/bad-list"
permission = "notes.write"
handler = "notes::bad_list"

[[endpoints]]
id = "notes.bad_update"
path = "POST /api/v1/notes/{id}/bad-update"
permission = "notes.write"
handler = "notes::bad_update"
"#;

/// A GET handler that tries to write.
async fn sneaky(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.create("notes.note", json!({ "title": "written by a read" }))
        .await?;
    ctx.get("notes.note", input.id()?).await
}

/// Filters must be objects; a mistake must not become "everything".
async fn bad_list(ctx: Ctx, input: Input) -> Result<Row> {
    let rows = ctx.list("notes.note", json!(["pinned", true])).await?;
    assert!(rows.len() <= 1);
    ctx.get("notes.note", input.id()?).await
}

/// Values must be objects; a mistake must not become an empty update.
async fn bad_update(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("notes.note", input.id()?, json!("pinned")).await
}

fn hardened() -> trestle::Project {
    let text: &'static str = Box::leak(format!("{NOTES}{EXTRA}").into_boxed_str());
    trestle::project()
        .intent_str("trestle.toml", text)
        .handler("notes::pin", pin)
        .handler("notes::sneaky", sneaky)
        .handler("notes::bad_list", bad_list)
        .handler("notes::bad_update", bad_update)
        .hook("notes::tidy", tidy)
        .migrations_dir(MIGRATIONS)
}

impl Server {
    async fn note(&self, title: &str) -> String {
        let (status, note, _) = self
            .call(
                "POST",
                "/api/v1/notes",
                MEMBER,
                self.tenant,
                Some(json!({ "title": title })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{note}");
        note["id"].as_str().unwrap().to_owned()
    }

    async fn version(&self, id: &str) -> Value {
        let (status, note, _) = self
            .call(
                "GET",
                &format!("/api/v1/notes/{id}"),
                MEMBER,
                self.tenant,
                None,
                None,
            )
            .await;
        if status == StatusCode::NOT_FOUND {
            return Value::Null;
        }
        note["version"].clone()
    }

    async fn count(&self) -> usize {
        let (_, list, _) = self
            .call("GET", "/api/v1/notes", MEMBER, self.tenant, None, None)
            .await;
        list["items"].as_array().unwrap().len()
    }
}

#[tokio::test]
async fn an_overflowing_version_is_refused_not_ignored() {
    let s = Server::start().await;
    let id = s.note("Keep me").await;
    let uri = format!("/api/v1/notes/{id}");
    let huge = "\"notes.note:9223372036854775808\"";
    let (status, body, _) = s
        .call(
            "PATCH",
            &uri,
            MEMBER,
            s.tenant,
            Some(json!({ "body": "overwritten" })),
            Some(huge),
        )
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert_eq!(s.version(&id).await, json!(1), "no write happened");
    let (status, body, _) = s
        .call("DELETE", &uri, MEMBER, s.tenant, None, Some(huge))
        .await;
    assert!(status.is_client_error(), "{status} {body}");
    assert_eq!(s.version(&id).await, json!(1), "not deleted");
    s.db.drop().await;
}

#[tokio::test]
async fn a_read_endpoint_cannot_write() {
    let s = Server::start_with(hardened()).await;
    let id = s.note("Only one").await;
    let (status, body, _) = s
        .call(
            "GET",
            &format!("/api/v1/notes/{id}/sneaky"),
            MEMBER,
            s.tenant,
            None,
            None,
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_eq!(s.count().await, 1, "the read created nothing");
    s.db.drop().await;
}

#[tokio::test]
async fn non_object_filters_and_values_are_refused() {
    let s = Server::start_with(hardened()).await;
    let id = s.note("First").await;
    s.note("Second").await;
    let (status, body, _) = s
        .call(
            "POST",
            &format!("/api/v1/notes/{id}/bad-list"),
            MEMBER,
            s.tenant,
            Some(json!({})),
            None,
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let (status, body, _) = s
        .call(
            "POST",
            &format!("/api/v1/notes/{id}/bad-update"),
            MEMBER,
            s.tenant,
            Some(json!({})),
            None,
        )
        .await;
    assert_ne!(status, StatusCode::OK, "{body}");
    assert_eq!(s.version(&id).await, json!(1), "no empty update happened");
    s.db.drop().await;
}

/// The `trestle.toml` example in README.md parses and assembles as written.
#[test]
fn the_readme_intent_example_is_valid() {
    let readme = include_str!("../../../README.md");
    let block = readme
        .split("<!-- readme-intent:start -->")
        .nth(1)
        .and_then(|rest| rest.split("<!-- readme-intent:end -->").next())
        .expect("README has the intent example markers");
    let text = block
        .trim()
        .strip_prefix("```toml")
        .and_then(|t| t.strip_suffix("```"))
        .expect("a toml code block");
    let intent = trestle::intent::parse("trestle.toml", text).unwrap();
    assert_eq!(intent.name, "notes");
    trestle::project()
        .intent_str("trestle.toml", text)
        .assemble()
        .unwrap();
}
