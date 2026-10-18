//! ADR-025: domain actions in `trestle.toml`. A declared transition locks
//! one row in the caller's tenant, checks `If-Match` and the states it may
//! start from, writes its `set` constants (`"now"` for instants) and the
//! fields it `accept`s, and returns the row. Needs PostgreSQL.

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
viewer = ["crm.read"]

[[models]]
id = "crm.deal"
fields = [
  { id = "title",     type = "text", max = 100 },
  { id = "stage",     type = "enum", values = ["open", "won", "lost"], default = "open" },
  { id = "closed_at", type = "instant", optional = true },
  { id = "reason",    type = "text", max = 200, optional = true },
]

[[endpoints]]
model = "crm.deal"
crud = ["get", "create"]
read = "crm.read"
write = "crm.write"

[[endpoints]]
id = "crm.win"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/win"
permission = "crm.write"
set = { stage = "won", closed_at = "now" }
when = { stage = ["open"] }
accept = ["reason"]

[[endpoints]]
id = "crm.reopen"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/reopen"
permission = "crm.write"
set = { stage = "open" }
when = { stage = ["won", "lost"] }
"#;

const MEMBER: &str = "member-token";
const VIEWER: &str = "viewer-token";
const OUTSIDER: &str = "outsider-token";
const MEMBER_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const VIEWER_ID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const OUTSIDER_ID: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

fn crm() -> trestle::Project {
    trestle::project().intent_str("trestle.toml", CRM)
}

struct Server {
    app: Router,
    db: TestDatabase,
    registry: trestle::core::registry::Registry<State>,
    state: State,
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
        for ws in [north, south] {
            sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
                .bind(ws)
                .execute(&pool)
                .await
                .unwrap();
        }
        for (ws, actor, role) in [
            (north, MEMBER_ID, "member"),
            (north, VIEWER_ID, "viewer"),
            (south, OUTSIDER_ID, "member"),
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
        let tokens = format!("{MEMBER}={MEMBER_ID},{VIEWER}={VIEWER_ID},{OUTSIDER}={OUTSIDER_ID}");
        let identity = Arc::new(DevTokenIdentity::parse(&tokens).unwrap());
        let state = State::new(pool, identity);
        let app = trestle::router_with(&registry, state.clone(), Default::default());
        Server {
            app,
            db,
            registry,
            state,
            north,
            south,
        }
    }

    async fn call(
        &self,
        uri: &str,
        token: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let tenant = if token == OUTSIDER {
            self.south
        } else {
            self.north
        };
        let method = if body.is_some() || uri.ends_with("/win") || uri.ends_with("/reopen") {
            "POST"
        } else {
            "GET"
        };
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("Trestle-Tenant", tenant.to_string());
        if let Some(tag) = if_match {
            req = req.header(header::IF_MATCH, tag);
        }
        let req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.unwrap_or(json!({})).to_string()))
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

#[test]
fn actions_are_checked_when_the_file_is_read() {
    let problems = |from: &str, to: &str| {
        let text = CRM.replace(from, to);
        assert_ne!(text, CRM, "the replacement applies: {from}");
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems)
            .unwrap_or_default()
    };
    let has = |p: Vec<String>, text: &str| {
        p.iter().any(|m| m.contains(text)) || {
            eprintln!("{p:#?}");
            false
        }
    };
    assert!(has(
        problems(
            r#""POST /api/v1/deals/{id}/win""#,
            r#""GET /api/v1/deals/{id}/win""#
        ),
        "is POST on one row"
    ));
    assert!(has(
        problems(
            r#""POST /api/v1/deals/{id}/win""#,
            r#""POST /api/v1/deals/win""#
        ),
        "is POST on one row"
    ));
    assert!(has(
        problems(
            r#"set = { stage = "won", closed_at = "now" }"#,
            r#"set = { stage = "won", owner = "x" }"#
        ),
        "\"owner\" is not a field"
    ));
    assert!(has(
        problems(
            r#"set = { stage = "won", closed_at = "now" }"#,
            r#"set = { stage = "done" }"#
        ),
        "does not fit"
    ));
    assert!(has(
        problems(r#"when = { stage = ["open"] }"#, r#"when = { stage = [] }"#),
        "must list values"
    ));
    assert!(has(
        problems(r#"accept = ["reason"]"#, r#"accept = ["stage"]"#),
        "both sets and accepts stage"
    ));
    assert!(has(
        problems(
            r#"permission = "crm.write"
set = { stage = "open" }"#,
            r#"permission = "crm.write"
handler = "crm::reopen"
set = { stage = "open" }"#
        ),
        "either a handler or an action"
    ));
}

#[test]
fn an_action_is_a_write_bound_with_if_match() {
    let registry = crm().assemble().unwrap();
    let win = registry.inspect_operation("crm.win").unwrap();
    assert_eq!(win["mode"], "write");
    assert_eq!(win["http"]["method"], "POST");
    assert_eq!(win["http"]["path"], "/api/v1/deals/{id}/win");
    assert_eq!(win["effects"]["writes"], json!(["crm.deal"]));
    let fields: Vec<&str> = win["input"]["schema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(fields, ["expected_version", "id", "reason"]);
}

#[tokio::test]
async fn a_transition_runs_once_from_its_states_with_the_current_version() {
    let server = Server::start().await;
    let (status, deal, etag) = server
        .call(
            "/api/v1/deals",
            MEMBER,
            Some(json!({ "title": "Big" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{deal}");
    assert_eq!(deal["stage"], "open");
    let id = deal["id"].as_str().unwrap();
    let win = format!("/api/v1/deals/{id}/win");
    let reason = json!({ "reason": "Signed" });

    let (status, _, _) = server.call(&win, MEMBER, Some(reason.clone()), None).await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_REQUIRED,
        "If-Match is required"
    );
    let (status, _, _) = server
        .call(&win, MEMBER, Some(reason.clone()), Some("\"7\""))
        .await;
    assert_eq!(
        status,
        StatusCode::PRECONDITION_FAILED,
        "a stale version is refused"
    );
    let (status, refused, _) = server
        .call(&win, VIEWER, Some(reason.clone()), etag.as_deref())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    let (status, refused, _) = server
        .call(&win, OUTSIDER, Some(reason.clone()), etag.as_deref())
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "another tenant's deal: {refused}"
    );

    let (status, won, etag) = server
        .call(&win, MEMBER, Some(reason.clone()), etag.as_deref())
        .await;
    assert_eq!(status, StatusCode::OK, "{won}");
    assert_eq!(won["stage"], "won");
    assert_eq!(won["reason"], "Signed");
    assert_eq!(won["version"], 2);
    let closed = won["closed_at"].as_str().unwrap();
    assert!(closed.ends_with('Z'), "{closed}");

    let (status, refused, _) = server
        .call(&win, MEMBER, Some(reason), etag.as_deref())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "invalid_state");

    // Called directly (as a job would), the action still needs the version.
    let member = trestle::core::invocation::Invocation::for_actor(
        trestle::core::identity::ActorId(uuid::Uuid::parse_str(MEMBER_ID).unwrap()),
    )
    .with_tenant(server.north);
    let refused = server
        .registry
        .invoke(&server.state, "crm.reopen", &member, &json!({ "id": id }))
        .await
        .expect_err("no version");
    assert_eq!(
        refused.code(),
        "validation_failed",
        "the contract requires the version"
    );
    assert_eq!(refused.errors()[0].pointer, "/expected_version");

    let (status, reopened, _) = server
        .call(
            &format!("/api/v1/deals/{id}/reopen"),
            MEMBER,
            None,
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reopened}");
    assert_eq!(reopened["stage"], "open");
    assert_eq!(reopened["closed_at"], closed, "reopen sets only stage");
    server.db.drop().await;
}
