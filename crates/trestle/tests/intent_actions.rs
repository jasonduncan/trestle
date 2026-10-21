//! ADR-025: domain actions in `trestle.toml`. A declared transition locks
//! one row in the caller's tenant, checks `If-Match` and the states it may
//! start from, writes its `set` constants (`"now"` for instants) and the
//! fields it `accept`s, and returns the row. ADR-037: a field with
//! `writable = "actions"` is written by actions only. ADR-036: `require`,
//! `check` and `exists` guard an action, and `repeat = "unchanged"` makes
//! a repeated request a no-op. Needs PostgreSQL.

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

/// `CRM` with `stage` managed (ADR-037), CRUD update, a handler and a hook
/// that try to write it anyway, and an admin view.
fn managed() -> trestle::Project {
    let text = CRM
        .replace(
            r#"values = ["open", "won", "lost"], default = "open" }"#,
            r#"values = ["open", "won", "lost"], default = "open", writable = "actions" }"#,
        )
        .replace(r#"crud = ["get", "create"]"#, r#"crud = ["list", "get", "create", "update"]"#)
        .replace(
            "write = \"crm.write\"\n",
            "write = \"crm.write\"\nforms = true\nfilter = [\"stage\"]\nhooks = { before_update = \"crm::sneak\" }\n",
        )
        + r#"
[[endpoints]]
id = "crm.force"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/force"
permission = "crm.write"
handler = "crm::force"

[[admin]]
model = "crm.deal"
permission = "crm.read"
actions = ["crm.win"]
"#;
    trestle::project()
        .intent_str("trestle.toml", &text)
        .handler("crm::force", force)
        .hook("crm::sneak", sneak)
}

/// Writes the title, then tries the stage: the whole operation rolls back.
async fn force(ctx: Ctx, input: Input) -> Result<Row> {
    let id = input.id()?;
    ctx.update("crm.deal", id, json!({ "title": "Forced" }))
        .await?;
    ctx.update("crm.deal", id, json!({ "stage": "won" })).await
}

/// Moves a deal titled "Sneak" to won behind the action's back.
async fn sneak(_ctx: Ctx, mut row: Row) -> Result<Row> {
    if row.get::<String>("title")? == "Sneak" {
        row.set("stage", "won")?;
    }
    Ok(row)
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
        Self::start_with(crm()).await
    }

    async fn start_with(project: trestle::Project) -> Self {
        let registry = project.assemble().unwrap();
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
        let method = if body.is_some() || uri.ends_with("/win") || uri.ends_with("/reopen") {
            "POST"
        } else {
            "GET"
        };
        self.send(method, uri, token, body, if_match).await
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let reply = self.keyed(method, uri, token, body, if_match, None).await;
        (
            reply.status,
            serde_json::from_slice(&reply.bytes).unwrap_or(Value::Null),
            reply.etag,
        )
    }

    /// [`send`](Self::send) with an optional `Idempotency-Key` (ADR-038),
    /// keeping the exact body bytes and the replay marker.
    async fn keyed(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
        if_match: Option<&str>,
        key: Option<&str>,
    ) -> Reply {
        let tenant = if token == OUTSIDER {
            self.south
        } else {
            self.north
        };
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header("Trestle-Tenant", tenant.to_string());
        if let Some(tag) = if_match {
            req = req.header(header::IF_MATCH, tag);
        }
        if let Some(key) = key {
            req = req.header("Idempotency-Key", key);
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
        let replayed = res.headers().contains_key("idempotency-replayed");
        let location = res
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status,
            bytes,
            etag,
            location,
            replayed,
        }
    }
}

struct Reply {
    status: StatusCode,
    bytes: Vec<u8>,
    etag: Option<String>,
    location: Option<String>,
    replayed: bool,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
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
    // ADR-037: a managed field takes its default on create, and some action
    // writes it.
    assert!(has(
        problems(
            r#"default = "open" }"#,
            r#"default = "open", writable = "sometimes" }"#
        ),
        r#"writable = "sometimes"; use "always" (the default) or "actions""#
    ));
    let title = problems(
        r#"type = "text", max = 100 }"#,
        r#"type = "text", max = 100, writable = "actions" }"#,
    );
    assert!(has(
        title.clone(),
        r#"field "title" is written only by actions, so create cannot set it"#
    ));
    assert!(has(
        title,
        r#"field "title" is written only by actions, but no action on crm.deal sets or accepts it"#
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

#[tokio::test]
async fn a_managed_field_is_written_only_by_actions() {
    let server = Server::start_with(managed()).await;
    // Offline surface: CRUD contracts and forms leave stage out; the
    // representation, the list filter and the action keep it.
    let registry = &server.registry;
    let keys = |op: &str, side: &str| -> Vec<String> {
        registry.inspect_operation(op).unwrap()[side]["schema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    for op in ["crm.deal.create", "crm.deal.update"] {
        assert!(!keys(op, "input").contains(&"stage".to_owned()), "{op}");
        assert!(keys(op, "output").contains(&"stage".to_owned()), "{op}");
    }
    for form in [
        "crm.deal.create.form",
        "crm.deal.update.form",
        "crm.deal.admin_create.form",
        "crm.deal.admin_edit.form",
    ] {
        let descriptor = &registry.form(form).expect("published").descriptor;
        let pointers: Vec<&str> = descriptor["presentation"]["layout"]["children"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["pointer"].as_str().unwrap())
            .collect();
        assert_eq!(pointers, ["/title", "/closed_at", "/reason"], "{form}");
    }
    let admin = &registry.admin_export()["resources"][0];
    assert_eq!(
        admin["columns"],
        json!(["title", "stage", "closed_at", "reason"])
    );
    assert_eq!(admin["filters"][0]["name"], "stage");
    // Managed is a contract change only: the schema is CRM's.
    assert_eq!(
        serde_json::to_value(registry.desired_schema()).unwrap(),
        serde_json::to_value(crm().assemble().unwrap().desired_schema()).unwrap()
    );

    // Create takes the default; sending stage is the unknown-field 422.
    let (status, refused, _) = server
        .call(
            "/api/v1/deals",
            MEMBER,
            Some(json!({ "title": "Big", "stage": "won" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["errors"][0]["pointer"], "/stage");
    assert_eq!(refused["errors"][0]["code"], "unknown_field");
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
    let row = format!("/api/v1/deals/{id}");

    // Update refuses stage the same way, with or without other changes.
    let (status, refused, _) = server
        .send(
            "PATCH",
            &row,
            MEMBER,
            Some(json!({ "title": "Bigger", "stage": "won" })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["errors"][0]["pointer"], "/stage");
    assert_eq!(refused["errors"][0]["code"], "unknown_field");

    // A hook that sets it is a 500, and nothing is written.
    let (status, refused, _) = server
        .send(
            "PATCH",
            &row,
            MEMBER,
            Some(json!({ "title": "Sneak" })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{refused}");
    assert_eq!(refused["code"], "internal_error");

    // So is a handler's Ctx::update, and its earlier write rolls back.
    let (status, refused, _) = server
        .call(&format!("{row}/force"), MEMBER, Some(json!({})), None)
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{refused}");
    let (_, unchanged, etag) = server.call(&row, MEMBER, None, None).await;
    assert_eq!(unchanged["title"], "Big");
    assert_eq!(unchanged["stage"], "open");
    assert_eq!(unchanged["version"], 1);

    // Ordinary updates still work and leave stage alone.
    let (status, renamed, etag) = server
        .send(
            "PATCH",
            &row,
            MEMBER,
            Some(json!({ "title": "Bigger" })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["stage"], "open");
    assert_eq!(renamed["version"], 2);

    // The action writes it, with its when rule and If-Match as before.
    let (status, won, _) = server
        .call(&format!("{row}/win"), MEMBER, None, etag.as_deref())
        .await;
    assert_eq!(status, StatusCode::OK, "{won}");
    assert_eq!(won["stage"], "won");
    assert_eq!(won["version"], 3);
    let (status, listed, _) = server
        .call("/api/v1/deals?stage=won", MEMBER, None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["items"][0]["id"], id, "filters on it still work");
    server.db.drop().await;
}

// ADR-036: guarded actions. `require`, `check` and `exists` hold on the
// row as it would be after the write; `repeat = "unchanged"` makes asking
// again for the current state a no-op.
const PIPELINE: &str = r#"
[project]
name = "pipeline"

[tenancy]
model = "workspace"

[roles]
member = ["crm.read", "crm.write"]
viewer = ["crm.read"]

[[models]]
id = "crm.organization"
fields = [{ id = "name", type = "text", max = 100 }]

[[models]]
id = "crm.contact"
fields = [{ id = "name", type = "text", max = 100 }]

[[models]]
id = "crm.affiliation"
fields = [
  { id = "contact",      type = "ref", to = "crm.contact" },
  { id = "organization", type = "ref", to = "crm.organization" },
  { id = "ended_at",     type = "instant", optional = true },
]

[[models]]
id = "crm.deal"
fields = [
  { id = "title",             type = "text", max = 100 },
  { id = "stage",             type = "enum", values = ["lead", "proposal", "contract"], default = "lead", writable = "actions" },
  { id = "value",             type = "money", precision = 12, scale = 2, optional = true },
  { id = "expected_close",    type = "date", optional = true },
  { id = "organization",      type = "ref", to = "crm.organization" },
  { id = "principal_contact", type = "ref", to = "crm.contact", optional = true },
  { id = "approved",          type = "bool", default = false },
  { id = "proposed_at",       type = "instant", optional = true },
]

[[endpoints]]
model = "crm.organization"
crud = ["create"]
write = "crm.write"

[[endpoints]]
model = "crm.contact"
crud = ["create"]
write = "crm.write"

[[endpoints]]
model = "crm.affiliation"
crud = ["create"]
write = "crm.write"

[[endpoints]]
model = "crm.deal"
crud = ["get", "create", "update"]
read = "crm.read"
write = "crm.write"

[[endpoints]]
id = "crm.propose"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/propose"
permission = "crm.write"
set = { stage = "proposal", proposed_at = "now" }
accept = ["expected_close"]
require = ["value", "expected_close"]
check = [{ field = "value", ge = "0" }]
repeat = "unchanged"

[[endpoints]]
id = "crm.contract"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/contract"
permission = "crm.write"
set = { stage = "contract" }
when = { stage = ["proposal"] }
require = ["principal_contact"]
exists = [{ model = "crm.affiliation", match = { contact = "principal_contact", organization = "organization" }, null = ["ended_at"] }]

[[endpoints]]
id = "crm.approve"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/approve"
permission = "crm.write"
set = { approved = true }
check = [{ field = "value", gt = { amount = "50000.00", currency = "USD" } }]
"#;

fn pipeline() -> trestle::Project {
    trestle::project().intent_str("trestle.toml", PIPELINE)
}

impl Server {
    async fn post(
        &self,
        uri: &str,
        body: Value,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        self.send("POST", uri, MEMBER, Some(body), if_match).await
    }

    /// Creates a row and returns its ID.
    async fn create(&self, table: &str, body: Value) -> String {
        let (status, row, _) = self.post(&format!("/api/v1/{table}"), body, None).await;
        assert_eq!(status, StatusCode::CREATED, "{row}");
        row["id"].as_str().unwrap().to_owned()
    }

    /// A deal, of a new organization unless the body names one, with its
    /// ETag and organization.
    async fn deal(&self, mut body: Value) -> (String, String, String) {
        let org = match body["organization"].as_str() {
            Some(org) => org.to_owned(),
            None => {
                self.create("organizations", json!({ "name": "Acme" }))
                    .await
            }
        };
        body["title"] = json!("Deal");
        body["organization"] = json!(org);
        let (status, deal, etag) = self.post("/api/v1/deals", body, None).await;
        assert_eq!(status, StatusCode::CREATED, "{deal}");
        (deal["id"].as_str().unwrap().to_owned(), etag.unwrap(), org)
    }

    async fn patch(&self, id: &str, body: Value, etag: &str) -> String {
        let (status, deal, etag) = self
            .send(
                "PATCH",
                &format!("/api/v1/deals/{id}"),
                MEMBER,
                Some(body),
                Some(etag),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{deal}");
        etag.unwrap()
    }

    async fn version(&self, id: &str) -> (Value, String) {
        let (status, deal, etag) = self
            .send("GET", &format!("/api/v1/deals/{id}"), MEMBER, None, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{deal}");
        (deal, etag.unwrap())
    }
}

/// `(pointer, code)` of each field error.
fn unmet(problem: &Value) -> Vec<(String, String)> {
    problem["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["pointer"].as_str().unwrap().to_owned(),
                e["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(p, c)| (p.to_string(), c.to_string()))
        .collect()
}

#[test]
fn guards_are_checked_when_the_file_is_read() {
    let problems = |from: &str, to: &str| {
        let text = PIPELINE.replace(from, to);
        assert_ne!(text, PIPELINE, "the replacement applies: {from}");
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems.join("\n"))
            .unwrap_or_default()
    };
    let check = r#"check = [{ field = "value", ge = "0" }]"#;
    let exists = r#"exists = [{ model = "crm.affiliation", match = { contact = "principal_contact", organization = "organization" }, null = ["ended_at"] }]"#;
    for (from, to, expected) in [
        (
            r#"require = ["value", "expected_close"]"#,
            r#"require = ["value", "closing"]"#,
            r#""closing" is not a field of crm.deal"#,
        ),
        (
            check,
            r#"check = [{ field = "worth", ge = "0" }]"#,
            r#""worth" is not a field of crm.deal"#,
        ),
        (
            check,
            r#"check = [{ field = "value", gte = "0" }]"#,
            "unknown field `gte`",
        ),
        (
            check,
            r#"check = [{ field = "value", ge = "0", lt = "9" }]"#,
            "a check on value takes exactly one of eq, ne, lt, le, gt, ge",
        ),
        (
            check,
            r#"check = [{ field = "value" }]"#,
            "a check on value takes exactly one of",
        ),
        (
            check,
            r#"check = [{ field = "expected_close", ge = "soon" }]"#,
            r#"check ge "soon" does not fit expected_close"#,
        ),
        (
            check,
            r#"check = [{ field = "value", ge = { amount = "1.005", currency = "USD" } }]"#,
            "does not fit value",
        ),
        (
            check,
            r#"check = [{ field = "value", ge = { amount = "1", currency = "usd" } }]"#,
            "does not fit value",
        ),
        (
            check,
            r#"check = [{ field = "stage", gt = "lead" }]"#,
            "check gt on stage: bool, text and enum fields take eq or ne",
        ),
        (
            check,
            r#"check = [{ field = "principal_contact", eq = "x" }]"#,
            "check cannot compare principal_contact, an ID",
        ),
        (
            exists,
            r#"exists = [{ model = "crm.membership", match = { contact = "principal_contact" } }]"#,
            r#"exists names "crm.membership", which is not a model in this file"#,
        ),
        (
            exists,
            r#"exists = [{ model = "crm.affiliation", match = { contact = "organization" } }]"#,
            "matches contact = organization, which are not the same type",
        ),
        (
            exists,
            r#"exists = [{ model = "crm.affiliation", match = { contact = "manager" } }]"#,
            "manager id or a field of crm.deal",
        ),
        (
            exists,
            r#"exists = [{ model = "crm.affiliation", match = { contact = "principal_contact" }, null = ["contact"] }]"#,
            "crm.affiliation.contact is required, so never null",
        ),
        (
            exists,
            r#"exists = [{ model = "crm.affiliation" }]"#,
            "exists crm.affiliation needs match",
        ),
        (
            r#"repeat = "unchanged""#,
            r#"repeat = "again""#,
            r#"repeat = "again"; use "refuse" (the default) or "unchanged""#,
        ),
        (
            r#"set = { stage = "proposal", proposed_at = "now" }"#,
            r#"set = { proposed_at = "now" }"#,
            r#"repeat = "unchanged" compares the set constants, and it has none"#,
        ),
        // An app that `exists` reads is one the action's app requires, so
        // it joins the one-way rule for refs between apps (ADR-032).
        (
            "crm.affiliation",
            "people.affiliation",
            "apps refer to each other's models in a circle (crm → people → crm)",
        ),
    ] {
        let found = problems(from, to);
        assert!(
            found.contains(expected),
            "{to}: expected {expected:?} in\n{found}"
        );
    }
    // Guards make an endpoint an action, so they can't go on a handler.
    assert!(
        problems(
            r#"repeat = "unchanged""#,
            "repeat = \"unchanged\"\nhandler = \"crm::propose\""
        )
        .contains("either a handler or an action")
    );
}

#[tokio::test]
async fn require_and_check_see_the_row_as_written_and_repeats_are_no_ops() {
    let server = Server::start_with(pipeline()).await;
    let (id, etag, _) = server.deal(json!({})).await;
    let propose = format!("/api/v1/deals/{id}/propose");

    // Neither value nor date: every unmet condition, nothing written.
    let (status, refused, _) = server.post(&propose, json!({}), Some(&etag)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["code"], "requirement_unmet");
    assert_eq!(
        unmet(&refused),
        pairs(&[
            ("/value", "required"),
            ("/expected_close", "required"),
            ("/value", "check"),
        ])
    );
    assert_eq!(
        refused["errors"][2]["params"],
        json!({ "op": "ge", "value": "0" })
    );
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("value must be at least 0."),
        "{refused}"
    );
    let (deal, _) = server.version(&id).await;
    assert_eq!(
        (deal["stage"].clone(), deal["version"].clone()),
        (json!("lead"), json!(1))
    );

    // A negative value fails the check; the accepted date counts as written.
    let etag = server
        .patch(
            &id,
            json!({ "value": { "amount": "-1.00", "currency": "USD" } }),
            &etag,
        )
        .await;
    let close = json!({ "expected_close": "2026-12-31" });
    let (status, refused, _) = server.post(&propose, close.clone(), Some(&etag)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(unmet(&refused), pairs(&[("/value", "check")]));
    let (deal, _) = server.version(&id).await;
    assert_eq!(deal["expected_close"], Value::Null, "nothing written");

    // Complete: the stage moves.
    let etag = server
        .patch(
            &id,
            json!({ "value": { "amount": "0", "currency": "USD" } }),
            &etag,
        )
        .await;
    let (status, proposed, etag) = server.post(&propose, close, Some(&etag)).await;
    assert_eq!(status, StatusCode::OK, "{proposed}");
    assert_eq!(proposed["stage"], "proposal");
    assert_eq!(proposed["expected_close"], "2026-12-31");
    assert_eq!(proposed["version"], 4);

    // Again: a no-op with the same row and ETag, even with other inputs.
    let (status, again, again_etag) = server
        .post(
            &propose,
            json!({ "expected_close": "2027-01-31" }),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, proposed, "nothing written, \"now\" included");
    assert_eq!(again_etag, etag);
    // If-Match still comes first.
    let (status, _, _) = server.post(&propose, json!({}), Some("\"3\"")).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    // The same rules through Registry::invoke.
    let member = trestle::core::invocation::Invocation::for_actor(
        trestle::core::identity::ActorId(uuid::Uuid::parse_str(MEMBER_ID).unwrap()),
    )
    .with_tenant(server.north);
    let (other, _, _) = server.deal(json!({})).await;
    let refused = server
        .registry
        .invoke(
            &server.state,
            "crm.propose",
            &member,
            &json!({ "id": other, "expected_version": "1" }),
        )
        .await
        .expect_err("unmet");
    assert_eq!(
        refused.code(),
        "requirement_unmet",
        "{:?}",
        refused.errors()
    );
    assert_eq!(refused.errors().len(), 3);
    server.db.drop().await;
}

#[tokio::test]
async fn money_checks_compare_the_amount_in_the_same_currency() {
    let server = Server::start_with(pipeline()).await;
    let (id, mut etag, _) = server.deal(json!({})).await;
    let approve = format!("/api/v1/deals/{id}/approve");
    for (amount, currency, code) in [
        ("50000.00", "USD", "check"),
        ("90000.00", "EUR", "currency_mismatch"),
    ] {
        etag = server
            .patch(
                &id,
                json!({ "value": { "amount": amount, "currency": currency } }),
                &etag,
            )
            .await;
        let (status, refused, _) = server.post(&approve, json!({}), Some(&etag)).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{amount} {currency}: {refused}"
        );
        assert_eq!(unmet(&refused), pairs(&[("/value", code)]), "{refused}");
    }
    etag = server
        .patch(
            &id,
            json!({ "value": { "amount": "50000.01", "currency": "USD" } }),
            &etag,
        )
        .await;
    let (status, approved, _) = server.post(&approve, json!({}), Some(&etag)).await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["approved"], true);
    server.db.drop().await;
}

/// A deal in `proposal` whose principal contact is `contact`, of `org` or
/// a new organization.
async fn proposed(
    server: &Server,
    contact: Option<&str>,
    org: Option<&str>,
) -> (String, String, String) {
    let (id, etag, org) = server
        .deal(json!({
            "value": { "amount": "10.00", "currency": "USD" },
            "expected_close": "2026-12-31",
            "principal_contact": contact,
            "organization": org,
        }))
        .await;
    let (status, deal, etag) = server
        .post(
            &format!("/api/v1/deals/{id}/propose"),
            json!({}),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{deal}");
    (id, etag.unwrap(), org)
}

#[tokio::test]
async fn exists_needs_a_current_related_row_in_the_callers_tenant() {
    let server = Server::start_with(pipeline()).await;
    let contract = server.registry.inspect_operation("crm.contract").unwrap();
    assert_eq!(contract["effects"]["reads"], json!(["crm.affiliation"]));
    assert_eq!(contract["effects"]["writes"], json!(["crm.deal"]));

    // No principal contact: required, and nothing to match.
    let (id, etag, _) = proposed(&server, None, None).await;
    let (status, refused, _) = server
        .post(
            &format!("/api/v1/deals/{id}/contract"),
            json!({}),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(
        unmet(&refused),
        pairs(&[
            ("/principal_contact", "required"),
            ("/principal_contact", "exists")
        ])
    );
    assert_eq!(
        refused["errors"][1]["params"],
        json!({ "model": "crm.affiliation" })
    );

    let pool = server.db.pool().await;
    let contact = server.create("contacts", json!({ "name": "Ada" })).await;
    let (id, etag, org) = proposed(&server, Some(&contact), None).await;
    let contract = format!("/api/v1/deals/{id}/contract");
    let refused_with = async |why: &str| {
        let (status, refused, _) = server.post(&contract, json!({}), Some(&etag)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{why}: {refused}");
        assert_eq!(
            unmet(&refused),
            pairs(&[("/principal_contact", "exists")]),
            "{why}"
        );
    };
    refused_with("no affiliation").await;

    // Another tenant's affiliation with the same IDs never counts.
    sqlx::query(
        "INSERT INTO affiliations (id, workspace_id, contact, organization) VALUES ($1, $2, $3::uuid, $4::uuid)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(server.south)
    .bind(&contact)
    .bind(&org)
    .execute(&pool)
    .await
    .unwrap();
    refused_with("another tenant's affiliation").await;

    // An ended affiliation doesn't either.
    let ended = server
        .create(
            "affiliations",
            json!({ "contact": contact, "organization": org, "ended_at": "2026-01-01T00:00:00Z" }),
        )
        .await;
    assert!(!ended.is_empty());
    refused_with("an ended affiliation").await;

    let current = server
        .create(
            "affiliations",
            json!({ "contact": contact, "organization": org }),
        )
        .await;
    let (status, signed, _) = server.post(&contract, json!({}), Some(&etag)).await;
    assert_eq!(status, StatusCode::OK, "{signed}");
    assert_eq!(signed["stage"], "contract");

    // The matched row is locked FOR SHARE: an action that finds it while
    // another transaction is ending it waits, then sees it ended.
    let (id, etag, _) = proposed(&server, Some(&contact), Some(&org)).await;
    let mut ending = pool.begin().await.unwrap();
    sqlx::query("UPDATE affiliations SET ended_at = now() WHERE id = $1::uuid")
        .bind(&current)
        .execute(&mut *ending)
        .await
        .unwrap();
    let app = server.app.clone();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/deals/{id}/contract"))
        .header(header::AUTHORIZATION, format!("Bearer {MEMBER}"))
        .header("Trestle-Tenant", server.north.to_string())
        .header(header::IF_MATCH, &etag)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let action = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    let mut waited = false;
    for _ in 0..200 {
        if action.is_finished() {
            let res = action.await.unwrap();
            let status = res.status();
            let body = res.into_body().collect().await.unwrap().to_bytes();
            panic!("the action did not wait for the lock: {status} {body:?}");
        }
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if blocked > 0 {
            waited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(waited, "the action never waited on the affiliation");
    ending.commit().await.unwrap();
    let res = action.await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(unmet(&body), pairs(&[("/principal_contact", "exists")]));
    server.db.drop().await;
}

/// ADR-038: `CRM` with an idempotent CRUD endpoint (create and update) and
/// an idempotent `crm.win`; `crm.reopen` stays without keys.
fn keyed_text() -> String {
    let text = CRM
        .replace(
            r#"crud = ["get", "create"]"#,
            r#"crud = ["get", "create", "update"]
idempotent = true"#,
        )
        .replace(
            r#"accept = ["reason"]"#,
            r#"accept = ["reason"]
idempotent = true"#,
        );
    assert_eq!(text.matches("idempotent = true").count(), 2);
    text
}

/// Runs a built-in command the way the project binary would.
fn cli(text: &str, dir: &std::path::Path, args: &[&str]) {
    let project = || {
        trestle::project()
            .intent_str("trestle.toml", text)
            .migrations_dir(dir)
    };
    let mut argv = vec!["crm"];
    argv.extend_from_slice(args);
    let matches = project().command().try_get_matches_from(argv).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(project().run_matches(&matches))
        .unwrap_or_else(|e| panic!("{args:?}: {e}"));
}

#[test]
fn idempotent_endpoints_are_writes_and_bring_the_receipt_table() {
    let problems = |from: &str, to: &str| {
        let text = CRM.replace(from, to);
        assert_ne!(text, CRM, "the replacement applies: {from}");
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems.join("\n"))
            .unwrap_or_default()
    };
    // Reads can't take a key.
    let read_only = problems(
        r#"crud = ["get", "create"]"#,
        r#"crud = ["get"]
idempotent = true"#,
    );
    assert!(
        read_only
            .contains("idempotent = true is for writes, and crud = [...] lists only list and get"),
        "{read_only}"
    );
    let get = problems(
        "[[endpoints]]\nid = \"crm.reopen\"",
        "[[endpoints]]\nid = \"crm.peek\"\nmodel = \"crm.deal\"\npath = \"GET /api/v1/deals/{id}/peek\"\n\
         permission = \"crm.read\"\nhandler = \"crm::peek\"\nidempotent = true\n\n\
         [[endpoints]]\nid = \"crm.reopen\"",
    );
    assert!(
        get.contains(
            r#"endpoint "crm.peek" is a GET, which only reads; idempotent = true is for writes"#
        ),
        "{get}"
    );

    // The receipt app is installed only when some endpoint is idempotent,
    // and a project that installed it itself keeps its own.
    let receipts = |registry: &trestle::Registry<State>| {
        registry
            .apps()
            .iter()
            .filter(|a| &*a.id == "idempotency")
            .count()
    };
    let text = keyed_text();
    let keyed = trestle::project()
        .intent_str("trestle.toml", &text)
        .assemble()
        .unwrap();
    assert_eq!(receipts(&keyed), 1);
    let accepting: Vec<&str> = keyed
        .operations()
        .iter()
        .filter(|o| o.accepts_idempotency_key())
        .map(|o| &*o.info.id)
        .collect();
    assert_eq!(
        accepting,
        ["crm.deal.create", "crm.deal.update", "crm.win"],
        "the CRUD writes and the idempotent action; never get or crm.reopen"
    );
    // OpenAPI documents the optional header where it is accepted, as for
    // Rust bindings.
    let openapi = keyed
        .openapi(trestle::protocol::Audience::Product)
        .to_string();
    assert_eq!(openapi.matches(r#""name":"Idempotency-Key""#).count(), 3);
    let own = trestle::project()
        .install_app(trestle::core::idempotency::IdempotencyApp)
        .intent_str("trestle.toml", &text)
        .assemble()
        .unwrap();
    assert_eq!(receipts(&own), 1);
    let plain = crm().assemble().unwrap();
    assert_eq!(receipts(&plain), 0);
    assert!(
        plain
            .desired_schema()
            .model("idempotency.receipt")
            .is_none()
    );

    // `makemigrations` writes the receipt table's migration with the
    // intent's, and only then.
    let dir = std::env::temp_dir().join(format!("trestle-u19-{}", uuid::Uuid::new_v4()));
    cli(
        &text,
        &dir.join("keyed"),
        &["makemigrations", "--name", "initial"],
    );
    cli(
        CRM,
        &dir.join("plain"),
        &["makemigrations", "--name", "initial"],
    );
    for app in ["idempotency", "crm", "workspace"] {
        assert!(
            dir.join("keyed")
                .join(app)
                .join("0001_initial.sql")
                .exists(),
            "no migration for {app}"
        );
    }
    assert!(
        dir.join("plain")
            .join("crm")
            .join("0001_initial.sql")
            .exists()
    );
    assert!(!dir.join("plain").join("idempotency").exists());
    cli(&text, &dir.join("keyed"), &["makemigrations", "--check"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn idempotent_endpoints_replay_after_reauthorizing() {
    let server =
        Server::start_with(trestle::project().intent_str("trestle.toml", &keyed_text())).await;
    let pool = server.db.pool().await;
    let deals = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM deals")
            .fetch_one(&pool)
            .await
            .unwrap()
    };

    // Create: the repeat replays the stored bytes, status and Location.
    let body = json!({ "title": "Big" });
    let first = server
        .keyed(
            "POST",
            "/api/v1/deals",
            MEMBER,
            Some(body.clone()),
            None,
            Some("create-1"),
        )
        .await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json());
    assert!(!first.replayed);
    let again = server
        .keyed(
            "POST",
            "/api/v1/deals",
            MEMBER,
            Some(body.clone()),
            None,
            Some("create-1"),
        )
        .await;
    assert_eq!(again.status, StatusCode::CREATED);
    assert!(again.replayed);
    assert_eq!(again.bytes, first.bytes);
    assert_eq!(again.location, first.location);
    assert_eq!(again.etag, first.etag);
    assert_eq!(deals().await, 1, "one row");
    // The same key with another body is the ADR-016 conflict.
    let other = server
        .keyed(
            "POST",
            "/api/v1/deals",
            MEMBER,
            Some(json!({ "title": "Small" })),
            None,
            Some("create-1"),
        )
        .await;
    assert_eq!(other.status, StatusCode::CONFLICT);
    assert_eq!(other.json()["code"], "idempotency_key_reused");
    assert_eq!(deals().await, 1);

    // An action: the replay answers with the first version; nothing runs.
    let deal = first.json();
    let id = deal["id"].as_str().unwrap();
    let win = format!("/api/v1/deals/{id}/win");
    let reason = json!({ "reason": "Signed" });
    let won = server
        .keyed(
            "POST",
            &win,
            MEMBER,
            Some(reason.clone()),
            first.etag.as_deref(),
            Some("win-1"),
        )
        .await;
    assert_eq!(won.status, StatusCode::OK, "{}", won.json());
    assert_eq!(won.json()["version"], 2);
    let again = server
        .keyed(
            "POST",
            &win,
            MEMBER,
            Some(reason.clone()),
            first.etag.as_deref(),
            Some("win-1"),
        )
        .await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json());
    assert!(again.replayed);
    assert_eq!(again.bytes, won.bytes);
    assert_eq!(again.etag, won.etag);
    let (_, current, _) = server
        .call(&format!("/api/v1/deals/{id}"), MEMBER, None, None)
        .await;
    assert_eq!(current["version"], 2, "the replay wrote nothing");
    let other = server
        .keyed(
            "POST",
            &win,
            MEMBER,
            Some(json!({ "reason": "Lost" })),
            first.etag.as_deref(),
            Some("win-1"),
        )
        .await;
    assert_eq!(other.json()["code"], "idempotency_key_reused");

    // CRUD update shares its endpoint's opt-in.
    let patch = format!("/api/v1/deals/{id}");
    let renamed = server
        .keyed(
            "PATCH",
            &patch,
            MEMBER,
            Some(json!({ "title": "Bigger" })),
            won.etag.as_deref(),
            Some("patch-1"),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.json());
    let again = server
        .keyed(
            "PATCH",
            &patch,
            MEMBER,
            Some(json!({ "title": "Bigger" })),
            won.etag.as_deref(),
            Some("patch-1"),
        )
        .await;
    assert!(again.replayed);
    assert_eq!(again.bytes, renamed.bytes);

    // Endpoints without `idempotent = true` refuse a key, never ignore it.
    let reopen = server
        .keyed(
            "POST",
            &format!("/api/v1/deals/{id}/reopen"),
            MEMBER,
            None,
            renamed.etag.as_deref(),
            Some("reopen-1"),
        )
        .await;
    assert_eq!(reopen.status, StatusCode::BAD_REQUEST);
    assert_eq!(reopen.json()["code"], "idempotency_unsupported");
    let read = server
        .keyed("GET", &patch, MEMBER, None, None, Some("get-1"))
        .await;
    assert_eq!(read.json()["code"], "idempotency_unsupported");
    let (_, current, _) = server.call(&patch, MEMBER, None, None).await;
    assert_eq!(current["stage"], "won", "the refused reopen ran nothing");

    // Registry::invoke honours the opt-in the same way.
    let member = trestle::core::invocation::Invocation::for_actor(
        trestle::core::identity::ActorId(uuid::Uuid::parse_str(MEMBER_ID).unwrap()),
    )
    .with_tenant(server.north);
    let replayed = server
        .registry
        .invoke(
            &server.state,
            "crm.deal.create",
            &member.clone().with_idempotency_key("create-1"),
            &body,
        )
        .await
        .unwrap();
    assert_eq!(replayed, deal, "the receipt is shared with HTTP");
    let refused = server
        .registry
        .invoke(
            &server.state,
            "crm.reopen",
            &member.clone().with_idempotency_key("reopen-2"),
            &json!({ "id": id, "expected_version": 3 }),
        )
        .await
        .expect_err("no opt-in");
    assert_eq!(refused.code(), "idempotency_unsupported");
    assert_eq!(deals().await, 1);

    // A replay is authorized again: once the member is only a viewer, the
    // stored answer is not disclosed.
    sqlx::query("UPDATE workspace_memberships SET role = 'viewer' WHERE actor_id = $1")
        .bind(uuid::Uuid::parse_str(MEMBER_ID).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let demoted = server
        .keyed(
            "POST",
            "/api/v1/deals",
            MEMBER,
            Some(body),
            None,
            Some("create-1"),
        )
        .await;
    assert_eq!(demoted.status, StatusCode::FORBIDDEN, "{}", demoted.json());
    assert!(!demoted.replayed);
    assert_eq!(demoted.json()["code"], "permission_denied");
    server.db.drop().await;
}
