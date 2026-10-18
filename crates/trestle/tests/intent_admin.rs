//! ADR-029: admin views for `trestle.toml` apps. Every CRUD endpoint is a
//! resource; `[[admin]]` shows one in the admin shell to callers with its
//! permission, with list columns and filters, generated admin forms, and its
//! actions as buttons that send `If-Match`. Showing grants nothing: each
//! button runs its operation's own permission. Partly needs PostgreSQL.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use trestle::core::identity::DevTokenIdentity;
use trestle::historical::planner::{PlanOptions, diff};
use trestle::historical::schema::SchemaState;
use trestle::historical::testing::TestDatabase;
use trestle::prelude::*;
use trestle::protocol::Audience;

const CRM: &str = r#"
[project]
name = "crm"
trestle = "0.1"

[tenancy]
model = "workspace"

[roles]
admin = ["crm.read", "crm.write", "crm.admin"]
member = ["crm.read", "crm.write"]
clerk = ["crm.read", "crm.admin"]

[[models]]
id = "crm.account"
fields = [{ id = "name", type = "text", max = 100 }]

[[models]]
id = "crm.deal"
fields = [
  { id = "title",     type = "text", max = 100 },
  { id = "amount",    type = "money", precision = 10, scale = 2, optional = true },
  { id = "account",   type = "ref", to = "crm.account" },
  { id = "stage",     type = "enum", values = ["open", "won"], default = "open" },
  { id = "closed_at", type = "instant", optional = true },
]

[[endpoints]]
model = "crm.account"
crud = ["list", "get", "create"]
read = "crm.read"
write = "crm.write"

[[endpoints]]
model = "crm.deal"
crud = ["list", "get", "create", "update"]
read = "crm.read"
write = "crm.write"
filter = ["stage"]
forms = true

[[endpoints]]
id = "crm.win"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/win"
permission = "crm.write"
set = { stage = "won", closed_at = "now" }
when = { stage = ["open"] }

[[admin]]
model = "crm.deal"
permission = "crm.admin"
columns = ["title", "amount", "stage"]
actions = ["crm.win"]
"#;

const ADMIN: &str = "admin-token";
const MEMBER: &str = "member-token";
const CLERK: &str = "clerk-token";
const ADMIN_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const MEMBER_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const CLERK_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn registry() -> trestle::core::registry::Registry<State> {
    trestle::project()
        .intent_str("trestle.toml", CRM)
        .assemble()
        .unwrap()
}

fn problems(from: &str, to: &str) -> Vec<String> {
    let text = CRM.replace(from, to);
    assert_ne!(text, CRM, "the replacement applies: {from}");
    trestle::intent::parse("trestle.toml", &text)
        .err()
        .map(|e| e.problems)
        .unwrap_or_default()
}

fn has(problems: Vec<String>, text: &str) -> bool {
    problems.iter().any(|m| m.contains(text)) || {
        eprintln!("{problems:#?}");
        false
    }
}

#[test]
fn an_admin_section_describes_the_resource_the_shell_renders() {
    let registry = registry();
    let resources: Vec<&str> = registry.resources().iter().map(|r| &*r.id).collect();
    assert_eq!(
        resources,
        ["crm.account", "crm.deal"],
        "every CRUD endpoint is a resource"
    );
    let admin = registry.admin_export();
    let described = admin["resources"].as_array().unwrap();
    assert_eq!(
        described.len(),
        1,
        "only [[admin]] models are in the admin profile"
    );
    let deal = &described[0];
    assert_eq!(deal["id"], "crm.deal");
    assert_eq!(deal["title"], "Deals");
    assert_eq!(deal["label"], "title");
    assert_eq!(deal["columns"], json!(["title", "amount", "stage"]));
    assert_eq!(
        deal["filters"][0]["name"], "stage",
        "the endpoint's filters by default"
    );
    assert_eq!(deal["list"]["path"], "/api/v1/deals");
    assert_eq!(deal["retrieve"]["path"], "/api/v1/deals/{id}");
    assert_eq!(deal["update"]["form"], "crm.deal.admin_edit.form");
    assert_eq!(deal["create"]["form"], "crm.deal.admin_create.form");
    assert_eq!(
        deal["actions"],
        json!([{
            "name": "win",
            "label": "Win",
            "operation": "crm.win",
            "path": "/api/v1/deals/{id}/win",
            "bind": {},
            "ifMatch": { "pointer": "/version", "etagOf": "crm.deal" },
            "visibleWhen": { "pointer": "/stage", "equals": "open" },
        }])
    );

    for id in ["crm.deal.admin_edit.form", "crm.deal.admin_create.form"] {
        let form = registry.form(id).expect("published");
        assert_eq!(form.form.audience, Audience::Admin, "{id}");
    }
    let edit = &registry
        .form("crm.deal.admin_edit.form")
        .unwrap()
        .descriptor;
    assert_eq!(edit["presentation"]["title"], "Edit deal");
    for id in ["crm.deal.list", "crm.deal.update", "crm.win"] {
        let op = registry.inspect_operation(id).unwrap();
        assert_eq!(op["audiences"], json!(["product", "admin"]), "{id}");
    }
    let account = registry.inspect_operation("crm.account.list").unwrap();
    assert_eq!(account["audiences"], json!(["product"]));
}

#[test]
fn admin_sections_are_checked_when_the_file_is_read() {
    let section = r#"[[admin]]
model = "crm.deal"
permission = "crm.admin"
columns = ["title", "amount", "stage"]
actions = ["crm.win"]"#;
    let admin = |to: &str| problems(section, to);
    assert!(has(
        admin("[[admin]]\nmodel = \"crm.lead\"\npermission = \"crm.admin\""),
        "admin model \"crm.lead\" is not a model"
    ));
    assert!(has(
        admin("[[admin]]\nmodel = \"crm.deal\""),
        "needs permission"
    ));
    assert!(has(
        admin("[[admin]]\nmodel = \"crm.deal\"\npermission = \"crm.boss\""),
        "permission \"crm.boss\" is not granted by any role"
    ));
    assert!(has(
        admin("[[admin]]\nmodel = \"crm.deal\"\npermission = \"crm.admin\"\ncolumns = [\"owner\"]"),
        "admin column \"owner\" is not id, version, created_at or a field of crm.deal"
    ));
    assert!(has(
        admin("[[admin]]\nmodel = \"crm.deal\"\npermission = \"crm.admin\"\nfilters = [\"title\"]"),
        "admin filter \"title\" is not in the filter"
    ));
    assert!(has(
        admin(
            "[[admin]]\nmodel = \"crm.account\"\npermission = \"crm.admin\"\nactions = [\"crm.win\"]"
        ),
        "admin action \"crm.win\" is not an action endpoint on crm.account"
    ));
    assert!(has(
        admin(&format!("{section}\n\n{section}")),
        "model \"crm.deal\" has two [[admin]] sections"
    ));
    assert!(has(
        problems(
            "crud = [\"list\", \"get\", \"create\", \"update\"]",
            "crud = [\"get\", \"create\", \"update\"]"
        ),
        "its CRUD endpoint needs \"list\""
    ));
    // The admin forms show every field, so each needs a widget, even
    // without `forms = true`.
    let text = CRM.replace("filter = [\"stage\"]\nforms = true", "filter = [\"stage\"]").replace(
        "{ id = \"title\",     type = \"text\", max = 100 },",
        "{ id = \"title\", type = \"text\", max = 100 },\n  { id = \"owner\", type = \"uuid\" },",
    );
    assert!(!text.contains("forms = true") && text.contains("owner"));
    let found = trestle::intent::parse("trestle.toml", &text)
        .err()
        .unwrap()
        .problems;
    assert!(has(found, "cannot show uuid field owner"));
}

struct Server {
    app: Router,
    db: TestDatabase,
    north: uuid::Uuid,
}

impl Server {
    async fn start() -> Self {
        let registry = registry();
        let db = TestDatabase::create().await;
        let pool = db.pool().await;
        let plan = diff(
            &SchemaState::default(),
            registry.desired_schema(),
            &PlanOptions::default(),
        );
        let none = trestle::historical::MigrationSet::new(Vec::new()).unwrap();
        for artifact in trestle::historical::artifact::generate(&none, &plan, "initial").unwrap() {
            sqlx::raw_sql(&artifact.sql).execute(&pool).await.unwrap();
        }
        let north = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
            .bind(north)
            .execute(&pool)
            .await
            .unwrap();
        for (actor, role) in [
            (ADMIN_ID, "admin"),
            (MEMBER_ID, "member"),
            (CLERK_ID, "clerk"),
        ] {
            sqlx::query(
                "INSERT INTO workspace_memberships (workspace_id, actor_id, role) VALUES ($1, $2, $3)",
            )
            .bind(north)
            .bind(uuid::Uuid::parse_str(actor).unwrap())
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
        }
        let tokens = format!("{ADMIN}={ADMIN_ID},{MEMBER}={MEMBER_ID},{CLERK}={CLERK_ID}");
        let identity = Arc::new(DevTokenIdentity::parse(&tokens).unwrap());
        let app = trestle::router_with(&registry, State::new(pool, identity), Default::default());
        Server { app, db, north }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("Trestle-Tenant", self.north.to_string());
        if let Some(tag) = if_match {
            req = req.header(header::IF_MATCH, tag);
        }
        let req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let etag = res
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            etag,
        )
    }
}

#[tokio::test]
async fn the_admin_shell_sees_what_its_permission_shows_and_buttons_still_authorize() {
    let server = Server::start().await;

    let (status, manifest, _) = server
        .call("GET", "/_meta/v1/admin/manifest", ADMIN, None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{manifest}");
    assert_eq!(manifest["resources"][0]["id"], "crm.deal");
    let (status, refused, _) = server
        .call("GET", "/_meta/v1/admin/manifest", MEMBER, None, None)
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "no admin permission: {refused}"
    );

    let (status, form, _) = server
        .call(
            "GET",
            "/_meta/v1/admin/interactions/crm.deal.admin_edit.form",
            ADMIN,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert_eq!(form["availability"]["available"], true);
    // A clerk sees the admin view but cannot write: the form says so.
    let (status, form, _) = server
        .call(
            "GET",
            "/_meta/v1/admin/interactions/crm.deal.admin_edit.form",
            CLERK,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert_eq!(
        form["availability"],
        json!({ "available": false, "reason": "permission_denied" })
    );

    // Generated product forms are served too, with availability (ADR-026).
    let (status, form, _) = server
        .call(
            "GET",
            "/_meta/v1/interactions/crm.deal.create.form",
            MEMBER,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{form}");
    assert_eq!(form["availability"]["available"], true);

    let (status, account, _) = server
        .call(
            "POST",
            "/api/v1/accounts",
            ADMIN,
            Some(json!({ "name": "Acme" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    let (status, deal, _) = server
        .call(
            "POST",
            "/api/v1/deals",
            ADMIN,
            Some(json!({ "title": "Big", "account": account["id"], "amount": { "amount": "10.00", "currency": "USD" } })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{deal}");

    // The shell's button: POST the action path with the row's version as If-Match.
    let (status, page, _) = server
        .call("GET", "/api/v1/deals?stage=open", ADMIN, None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let row = &page["items"][0];
    let path = format!("/api/v1/deals/{}/win", row["id"].as_str().unwrap());
    let tag = format!("\"crm.deal:{}\"", row["version"]);
    let (status, refused, _) = server
        .call("POST", &path, CLERK, Some(json!({})), Some(&tag))
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "showing grants nothing: {refused}"
    );
    let (status, won, _) = server
        .call("POST", &path, ADMIN, Some(json!({})), Some(&tag))
        .await;
    assert_eq!(status, StatusCode::OK, "{won}");
    assert_eq!(won["stage"], "won");
    server.db.drop().await;
}
