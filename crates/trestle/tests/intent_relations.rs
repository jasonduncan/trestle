//! ADR-024: `trestle.toml` relations and cursor paging. A `ref` field is a
//! foreign key whose writes must name a row in the caller's tenant; a
//! referenced row cannot be deleted from under it; lists page with signed
//! cursors, as Rust lists do. Needs PostgreSQL.

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

const CRM: &str = r#"
[project]
name = "crm"
trestle = "0.1"

[tenancy]
model = "workspace"

[roles]
member = ["crm.read", "crm.write"]

[[models]]
id = "crm.account"
fields = [{ id = "name", type = "text", max = 100 }]

[[models]]
id = "crm.deal"
fields = [
  { id = "title",   type = "text", max = 100 },
  { id = "account", type = "ref", to = "crm.account" },
  { id = "partner", type = "ref", to = "crm.account", optional = true },
]

[[endpoints]]
model = "crm.account"
crud = ["list", "get", "create", "delete"]
read = "crm.read"
write = "crm.write"
order = "name"

[[endpoints]]
model = "crm.deal"
crud = ["list", "get", "create", "update"]
read = "crm.read"
write = "crm.write"
"#;

const NORTH_TOKEN: &str = "north-token";
const SOUTH_TOKEN: &str = "south-token";
const NORTH_ACTOR: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const SOUTH_ACTOR: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

fn crm() -> trestle::Project {
    trestle::project().intent_str("trestle.toml", CRM)
}

struct Server {
    app: Router,
    db: TestDatabase,
    north: uuid::Uuid,
    south: uuid::Uuid,
}

impl Server {
    async fn start() -> Self {
        let registry = crm().assemble().unwrap();
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
        let (north, south) = (uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        for (ws, actor) in [(north, NORTH_ACTOR), (south, SOUTH_ACTOR)] {
            sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
                .bind(ws)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO workspace_memberships (workspace_id, actor_id, role) VALUES ($1, $2, 'member')",
            )
            .bind(ws)
            .bind(uuid::Uuid::parse_str(actor).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        let tokens = format!("{NORTH_TOKEN}={NORTH_ACTOR},{SOUTH_TOKEN}={SOUTH_ACTOR}");
        let identity = Arc::new(DevTokenIdentity::parse(&tokens).unwrap());
        let app = trestle::router_with(&registry, State::new(pool, identity), Default::default());
        Server {
            app,
            db,
            north,
            south,
        }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        south: bool,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let (token, tenant) = if south {
            (SOUTH_TOKEN, self.south)
        } else {
            (NORTH_TOKEN, self.north)
        };
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            etag,
        )
    }

    async fn account(&self, name: &str, south: bool) -> String {
        let (status, body, _) = self
            .call(
                "POST",
                "/api/v1/accounts",
                south,
                Some(json!({ "name": name })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_owned()
    }
}

#[test]
fn a_ref_field_is_an_indexed_foreign_key() {
    let registry = crm().assemble().unwrap();
    let deal = registry.desired_schema().model("crm.deal").unwrap();
    let account = deal.field("crm.deal.account").unwrap();
    assert_eq!(account.references.as_deref(), Some("crm.account"));
    assert!(deal.indexes.iter().any(|i| i.name == "deals_account_idx"));
}

#[test]
fn refs_and_orders_are_checked() {
    let problems = |from: &str, to: &str| {
        trestle::intent::parse("trestle.toml", &CRM.replace(from, to))
            .err()
            .map(|e| e.problems)
            .unwrap_or_default()
    };
    let has = |p: Vec<String>, text: &str| p.iter().any(|m| m.contains(text));
    assert!(has(
        problems(
            r#"type = "ref", to = "crm.account" },"#,
            r#"type = "ref" },"#
        ),
        "needs to ="
    ));
    assert!(has(
        problems(
            r#"{ id = "name", type = "text", max = 100 }"#,
            r#"{ id = "name", type = "text", to = "crm.deal" }"#
        ),
        "is for ref fields"
    ));
    assert!(has(
        problems(r#"to = "crm.account" },"#, r#"to = "crm.contact" },"#),
        "not a model in this file"
    ));
    let optional_order = CRM.replace(
        r#"{ id = "name", type = "text", max = 100 }"#,
        r#"{ id = "name", type = "text", max = 100, optional = true }"#,
    );
    let p = trestle::intent::parse("trestle.toml", &optional_order)
        .err()
        .unwrap()
        .problems;
    assert!(has(p, "is optional; order by a required field"));
}

#[tokio::test]
async fn references_must_name_a_row_in_the_callers_tenant() {
    let server = Server::start().await;
    let ours = server.account("Acme", false).await;
    let theirs = server.account("Globex", true).await;

    let (status, deal, etag) = server
        .call(
            "POST",
            "/api/v1/deals",
            false,
            Some(json!({ "title": "Big", "account": ours })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{deal}");

    for stranger in [theirs.clone(), uuid::Uuid::now_v7().to_string()] {
        let (status, refused, _) = server
            .call(
                "POST",
                "/api/v1/deals",
                false,
                Some(json!({ "title": "x", "account": stranger })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
        assert_eq!(refused["errors"][0]["pointer"], "/account");
        assert_eq!(refused["errors"][0]["code"], "not_found");
    }

    let id = deal["id"].as_str().unwrap();
    let path = format!("/api/v1/deals/{id}");
    let (status, refused, _) = server
        .call(
            "PATCH",
            &path,
            false,
            Some(json!({ "partner": theirs })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["errors"][0]["pointer"], "/partner");
    let (status, updated, etag) = server
        .call(
            "PATCH",
            &path,
            false,
            Some(json!({ "partner": ours })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["partner"], json!(ours));
    let (status, cleared, _) = server
        .call(
            "PATCH",
            &path,
            false,
            Some(json!({ "partner": null })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");

    // The account is in use, so it cannot go.
    let (_, account, etag) = server
        .call(
            "GET",
            &format!("/api/v1/accounts/{ours}"),
            false,
            None,
            None,
        )
        .await;
    assert_eq!(account["name"], "Acme");
    let (status, refused, _) = server
        .call(
            "DELETE",
            &format!("/api/v1/accounts/{ours}"),
            false,
            None,
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "still_referenced");
    server.db.drop().await;
}

#[tokio::test]
async fn lists_page_with_signed_cursors() {
    let server = Server::start().await;
    let mut names: Vec<String> = ["Echo", "Alpha", "Delta", "Bravo", "Charlie"]
        .iter()
        .map(|n| (*n).to_owned())
        .collect();
    for name in &names {
        server.account(name, false).await;
    }
    server.account("Zulu", true).await; // another tenant's
    names.sort();

    let mut seen = Vec::new();
    let mut uri = "/api/v1/accounts?limit=2".to_owned();
    let mut cursors = Vec::new();
    loop {
        let (status, page, _) = server.call("GET", &uri, false, None, None).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 2);
        seen.extend(items.iter().map(|i| i["name"].as_str().unwrap().to_owned()));
        match page["next_cursor"].as_str() {
            Some(cursor) => {
                assert!(cursors.len() < 5, "the cursor never reaches the end");
                cursors.push(cursor.to_owned());
                uri = format!("/api/v1/accounts?limit=2&cursor={cursor}");
            }
            None => break,
        }
    }
    assert_eq!(seen, names, "every row once, in order, only this tenant's");
    assert_eq!(cursors.len(), 2);

    // A cursor belongs to its query.
    let (status, refused, _) = server
        .call(
            "GET",
            &format!("/api/v1/accounts?limit=3&cursor={}", cursors[0]),
            false,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["code"], "cursor_mismatch");
    let tampered = format!("{}x", cursors[0]);
    let (status, _, _) = server
        .call(
            "GET",
            &format!("/api/v1/accounts?limit=2&cursor={tampered}"),
            false,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The default order (-created_at) pages too.
    let ours = server.account("Owner", false).await;
    for n in 0..3 {
        server
            .call(
                "POST",
                "/api/v1/deals",
                false,
                Some(json!({ "title": format!("deal {n}"), "account": ours })),
                None,
            )
            .await;
    }
    let (_, first, _) = server
        .call("GET", "/api/v1/deals?limit=2", false, None, None)
        .await;
    let cursor = first["next_cursor"].as_str().unwrap();
    let (_, second, _) = server
        .call(
            "GET",
            &format!("/api/v1/deals?limit=2&cursor={cursor}"),
            false,
            None,
            None,
        )
        .await;
    let titles: Vec<&str> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["items"].as_array().unwrap())
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["deal 2", "deal 1", "deal 0"]);
    assert!(second["next_cursor"].is_null());
    server.db.drop().await;
}
