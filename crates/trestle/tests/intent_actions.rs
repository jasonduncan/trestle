//! ADR-025: domain actions in `trestle.toml`. A declared transition locks
//! one row in the caller's tenant, checks `If-Match` and the states it may
//! start from, writes its `set` constants (`"now"` for instants) and the
//! fields it `accept`s, and returns the row. ADR-037: a field with
//! `writable = "actions"` is written by actions only. ADR-036: `require`,
//! `check` and `exists` guard an action, and `repeat = "unchanged"` makes
//! a repeated request a no-op. ADR-039: a model's write hooks run on every
//! write path to it. ADR-041: a model with `history = true` logs every
//! committed write, on every path. Needs PostgreSQL.

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

/// ADR-039: entering `won` needs an unconsumed approval of the deal's
/// organization, whichever path moves the deal (the action, CRUD update or
/// a handler's `Ctx::update`), and a win marks the organization a
/// customer. `stage` is deliberately not managed: the rule lives in the
/// model's write hooks, which no write path skips.
const HOOKED: &str = r#"
[project]
name = "crm"

[tenancy]
model = "workspace"

[roles]
member = ["crm.read", "crm.write"]
viewer = ["crm.read"]

[[models]]
id = "crm.organization"
fields = [
  { id = "name",     type = "text", max = 100 },
  { id = "customer", type = "bool", default = false },
  { id = "writes",   type = "int", default = 0 },
  { id = "last",     type = "text", optional = true },
]

[[models]]
id = "crm.approval"
fields = [
  { id = "organization", type = "ref", to = "crm.organization" },
  { id = "consumed",     type = "bool", default = false },
]

[[models]]
id = "crm.deal"
fields = [
  { id = "title",        type = "text", max = 100 },
  { id = "organization", type = "ref", to = "crm.organization" },
  { id = "stage",        type = "enum", values = ["open", "won"], default = "open" },
  { id = "note",         type = "text", optional = true },
]

[[endpoints]]
model = "crm.organization"
crud = ["get", "create"]
read = "crm.read"
write = "crm.write"

[[endpoints]]
model = "crm.approval"
crud = ["get", "create"]
read = "crm.read"
write = "crm.write"

[[endpoints]]
model = "crm.deal"
crud = ["get", "create", "update"]
read = "crm.read"
write = "crm.write"
hooks = { before_update = "crm::mark", after_update = "crm::late" }

[[endpoints]]
id = "crm.win"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/win"
permission = "crm.write"
set = { stage = "won" }
when = { stage = ["open"] }

[[endpoints]]
id = "crm.close"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/close"
permission = "crm.write"
handler = "crm::close"
input = [{ id = "expected", type = "int", optional = true }]
"#;

/// `HOOKED` built at run time (ADR-030), with the model write hooks added
/// to the document in code and registered like any hook.
fn hooked() -> trestle::Project {
    hooked_with(|_| {})
}

/// [`hooked`], with `edit` applied to the document first.
fn hooked_with(edit: impl FnOnce(&mut trestle::intent::IntentDoc)) -> trestle::Project {
    use std::collections::BTreeMap;
    let mut doc = trestle::intent::IntentDoc::from_toml("trestle.toml", HOOKED).unwrap();
    edit(&mut doc);
    for model in &mut doc.models {
        model.hooks = match model.id.as_str() {
            "crm.deal" => BTreeMap::from([
                ("before_write".into(), "crm::deal_rules".into()),
                ("after_write".into(), "crm::deal_effects".into()),
            ]),
            "crm.organization" => BTreeMap::from([("after_write".into(), "crm::count".into())]),
            _ => BTreeMap::new(),
        };
    }
    trestle::project()
        .intent_doc("crm", doc)
        .handler("crm::close", close)
        .hook("crm::mark", mark)
        .hook("crm::late", late)
        .write_hook("crm::deal_rules", deal_rules)
        .write_hook("crm::deal_effects", deal_effects)
        .write_hook("crm::count", count)
}

/// Whether the write moves a deal into `won`.
fn entering_won(write: &trestle::intent::Write) -> Result<bool> {
    let stage = |row: Option<&Row>| -> Result<Option<String>> {
        row.map(|r| r.get::<Option<String>>("stage"))
            .transpose()
            .map(Option::flatten)
    };
    Ok(stage(write.row())?.as_deref() == Some("won")
        && stage(write.old())?.as_deref() != Some("won"))
}

/// `before_write` on deal: consumes an approval of the organization, locked
/// with `get_for_update` and written with `update_versioned`; and shows it
/// runs after the endpoint's `before_update`, by extending its note.
async fn deal_rules(ctx: Ctx, mut write: trestle::intent::Write) -> Result<trestle::intent::Write> {
    let Some(new) = write.row() else {
        return Ok(write);
    };
    if new.get::<Option<String>>("note")?.as_deref() == Some("endpoint") {
        write.set("note", "endpoint, model")?;
    }
    if !entering_won(&write)? {
        return Ok(write);
    }
    let refused = || {
        Problem::conflict("approval_required")
            .with_detail("Winning a deal needs an approval of its organization.")
    };
    let org: Id = write.row().unwrap().get("organization")?;
    let open = ctx
        .list(
            "crm.approval",
            json!({ "organization": org, "consumed": false }),
        )
        .await?;
    let Some(approval) = open.first().and_then(Row::id) else {
        return Err(refused());
    };
    // Another win may have consumed it since the list: the lock waits for
    // that one to commit, and then shows the row as it is.
    let locked = ctx.get_for_update("crm.approval", approval).await?;
    if locked.get::<bool>("consumed")? {
        return Err(refused());
    }
    ctx.update_versioned(
        "crm.approval",
        approval,
        locked.get("version")?,
        json!({ "consumed": true }),
    )
    .await?;
    Ok(write)
}

/// `after_write` on deal: a win makes the organization a customer, in the
/// same transaction.
async fn deal_effects(ctx: Ctx, write: trestle::intent::Write) -> Result<trestle::intent::Write> {
    if entering_won(&write)? {
        let org: Id = write.row().unwrap().get("organization")?;
        ctx.update("crm.organization", org, json!({ "customer": true }))
            .await?;
    }
    Ok(write)
}

/// `after_write` on organization: counts writes by writing the organization
/// itself. Without the recursion rule this would never end.
async fn count(ctx: Ctx, write: trestle::intent::Write) -> Result<trestle::intent::Write> {
    if let Some(new) = write.row() {
        let writes: i64 = new.get("writes")?;
        ctx.update(
            "crm.organization",
            new.id().unwrap(),
            json!({ "writes": writes + 1, "last": write.operation() }),
        )
        .await?;
    }
    Ok(write)
}

/// The endpoint's `before_update`: marks a deal titled "Order".
async fn mark(_ctx: Ctx, mut row: Row) -> Result<Row> {
    if row.get::<String>("title")? == "Order" {
        row.set("note", "endpoint")?;
    }
    Ok(row)
}

/// The endpoint's `after_update`: fails a deal titled "Fail late", after
/// the model's `after_write` already made its organization a customer.
async fn late(ctx: Ctx, row: Row) -> Result<Row> {
    if row.get::<String>("title")? == "Fail late" {
        let org = ctx
            .get("crm.organization", row.get("organization")?)
            .await?;
        return Err(if org.get::<bool>("customer")? {
            Problem::conflict("late_failure")
        } else {
            Problem::conflict("after_write_did_not_run_first")
        });
    }
    Ok(row)
}

/// A handler that wins a deal through `Ctx`: `update_versioned` when the
/// input names a version, `update` otherwise.
async fn close(ctx: Ctx, input: Input) -> Result<Row> {
    let id = input.id()?;
    let stage = json!({ "stage": "won" });
    match input.get::<Option<i64>>("expected")? {
        Some(version) => ctx.update_versioned("crm.deal", id, version, stage).await,
        None => ctx.update("crm.deal", id, stage).await,
    }
}

impl Server {
    /// Creates an organization and returns its ID.
    async fn organization(&self, name: &str) -> String {
        self.create("organizations", json!({ "name": name })).await
    }

    /// A deal of `org`, with its ETag.
    async fn hooked_deal(&self, org: &str, title: &str) -> (String, String) {
        let (status, deal, etag) = self
            .post(
                "/api/v1/deals",
                json!({ "title": title, "organization": org }),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{deal}");
        (deal["id"].as_str().unwrap().to_owned(), etag.unwrap())
    }

    async fn row(&self, table: &str, id: &str) -> Value {
        let (status, row, _) = self
            .send("GET", &format!("/api/v1/{table}/{id}"), MEMBER, None, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{row}");
        row
    }
}

#[test]
fn write_hooks_are_checked_when_the_file_is_read_and_linked() {
    let text = HOOKED.replace(
        "id = \"crm.deal\"\n",
        "id = \"crm.deal\"\nhooks = { before_write = \"crm::rules\" }\n",
    );
    assert_ne!(text, HOOKED);
    trestle::intent::parse("trestle.toml", &text).unwrap();
    let problems = |text: &str| match trestle::intent::parse("trestle.toml", text) {
        Ok(_) => panic!("parsed:\n{text}"),
        Err(e) => e.problems.join("\n"),
    };
    let unknown = problems(&text.replace("before_write", "before_save"));
    assert!(
        unknown.contains(
            r#"unknown write hook "before_save" on crm.deal; use before_write or after_write"#
        ),
        "{unknown}"
    );
    let typed = problems(&text.replace(r#""crm::rules""#, "1"));
    assert!(typed.contains("trestle.toml:"), "{typed}");

    // Linking: every named write hook is registered, and every registered
    // one is named. Endpoint hooks and write hooks are separate.
    let refused = |project: trestle::Project| match project.assemble() {
        Ok(_) => panic!("assembled"),
        Err(e) => e.issues.join("\n"),
    };
    let unregistered = refused(
        trestle::project()
            .intent_str("trestle.toml", &text)
            .handler("crm::close", close)
            .hook("crm::mark", mark)
            .hook("crm::late", late)
            .hook("crm::rules", mark),
    );
    assert!(
        unregistered.contains(
            r#"before_write hook of model crm.deal names "crm::rules", which is not registered; add .write_hook("crm::rules", rules)"#
        ),
        "{unregistered}"
    );
    let stray = refused(hooked().write_hook("crm::stray", count));
    assert!(
        stray.contains(r#"write hook "crm::stray" is registered but no model in crm names it"#),
        "{stray}"
    );
}

#[tokio::test]
async fn write_hooks_hold_on_every_write_path_in_its_transaction() {
    let server = Server::start_with(hooked()).await;
    // A hooked write declares no effects, like an endpoint hook's; an
    // unhooked model's writes still do (ADR-018).
    for op in ["crm.win", "crm.deal.update", "crm.organization.create"] {
        let info = server.registry.inspect_operation(op).unwrap();
        assert_eq!(info["effects"], Value::Null, "{op}");
    }
    let approval = server
        .registry
        .inspect_operation("crm.approval.create")
        .unwrap();
    assert_eq!(approval["effects"]["writes"], json!(["crm.approval"]));

    // The organization's after_write writes the organization itself, once:
    // a model's hooks don't re-enter themselves.
    let acme = server.organization("Acme").await;
    let org = server.row("organizations", &acme).await;
    assert_eq!(
        (&org["writes"], &org["last"], &org["version"]),
        (&json!(1), &json!("crm.organization.create"), &json!(2)),
        "{org}"
    );

    // Without an approval, every path is refused, and nothing changes.
    let (deal, etag) = server.hooked_deal(&acme, "One").await;
    let win = format!("/api/v1/deals/{deal}/win");
    let refusals = [
        server.post(&win, json!({}), Some(&etag)).await,
        server
            .send(
                "PATCH",
                &format!("/api/v1/deals/{deal}"),
                MEMBER,
                Some(json!({ "stage": "won" })),
                Some(&etag),
            )
            .await,
        server
            .post(&format!("/api/v1/deals/{deal}/close"), json!({}), None)
            .await,
    ];
    for (path, (status, problem, _)) in ["action", "CRUD update", "handler"].iter().zip(refusals) {
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {problem}");
        assert_eq!(problem["code"], "approval_required", "{path}");
    }
    let unchanged = server.row("deals", &deal).await;
    assert_eq!(
        (&unchanged["stage"], &unchanged["version"]),
        (&json!("open"), &json!(1))
    );

    // The action consumes the approval, and the deal's after_write makes
    // the organization a customer: a nested Ctx write, which runs the
    // organization's hooks too.
    let first = server
        .create("approvals", json!({ "organization": acme }))
        .await;
    let (status, won, _) = server.post(&win, json!({}), Some(&etag)).await;
    assert_eq!(status, StatusCode::OK, "{won}");
    assert_eq!(won["stage"], "won");
    assert_eq!(server.row("approvals", &first).await["consumed"], true);
    let org = server.row("organizations", &acme).await;
    assert_eq!(
        (&org["customer"], &org["writes"], &org["last"]),
        (&json!(true), &json!(2), &json!("ctx")),
        "{org}"
    );

    // CRUD update: a failure after the hooks (the endpoint's after_update,
    // which sees the organization already a customer) rolls back the
    // approval, the deal and the organization together.
    let beta = server.organization("Beta").await;
    let second = server
        .create("approvals", json!({ "organization": beta }))
        .await;
    let (deal, etag) = server.hooked_deal(&beta, "Two").await;
    let (status, problem, _) = server
        .send(
            "PATCH",
            &format!("/api/v1/deals/{deal}"),
            MEMBER,
            Some(json!({ "stage": "won", "title": "Fail late" })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "late_failure");
    assert_eq!(server.row("approvals", &second).await["consumed"], false);
    let org = server.row("organizations", &beta).await;
    assert_eq!(
        (&org["customer"], &org["writes"]),
        (&json!(false), &json!(1))
    );
    let deal_now = server.row("deals", &deal).await;
    assert_eq!(
        (&deal_now["stage"], &deal_now["title"], &deal_now["version"]),
        (&json!("open"), &json!("Two"), &json!(1))
    );
    // The same update without the late failure: the endpoint's
    // before_update ran before the model's before_write.
    let (status, won, _) = server
        .send(
            "PATCH",
            &format!("/api/v1/deals/{deal}"),
            MEMBER,
            Some(json!({ "stage": "won", "title": "Order" })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{won}");
    assert_eq!(
        (&won["stage"], &won["note"]),
        (&json!("won"), &json!("endpoint, model"))
    );
    assert_eq!(server.row("approvals", &second).await["consumed"], true);
    assert_eq!(server.row("organizations", &beta).await["customer"], true);

    // A handler's Ctx writes run the hooks too. A stale version in
    // update_versioned is the 412 of a stale If-Match, and leaves the
    // approval unconsumed.
    let third = server
        .create("approvals", json!({ "organization": beta }))
        .await;
    let (deal, _) = server.hooked_deal(&beta, "Three").await;
    let close = format!("/api/v1/deals/{deal}/close");
    let (status, stale, _) = server.post(&close, json!({ "expected": 7 }), None).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{stale}");
    assert_eq!(server.row("approvals", &third).await["consumed"], false);
    let (status, closed, _) = server.post(&close, json!({ "expected": 1 }), None).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    assert_eq!(
        (&closed["stage"], &closed["version"]),
        (&json!("won"), &json!(2))
    );
    assert_eq!(server.row("approvals", &third).await["consumed"], true);
    server.db.drop().await;
}

#[tokio::test]
async fn two_wins_never_consume_one_approval_twice() {
    let server = Server::start_with(hooked()).await;
    let pool = server.db.pool().await;
    let org = server.organization("Acme").await;
    let approval = server
        .create("approvals", json!({ "organization": org }))
        .await;
    let (one, one_etag) = server.hooked_deal(&org, "One").await;
    let (two, two_etag) = server.hooked_deal(&org, "Two").await;

    // Hold the approval so both wins reach get_for_update and wait there.
    let mut holding = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM approvals WHERE id = $1::uuid FOR UPDATE")
        .bind(&approval)
        .execute(&mut *holding)
        .await
        .unwrap();
    let win = |deal: &str, etag: &str| {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/deals/{deal}/win"))
            .header(header::AUTHORIZATION, format!("Bearer {MEMBER}"))
            .header("Trestle-Tenant", server.north.to_string())
            .header(header::IF_MATCH, etag)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let app = server.app.clone();
        tokio::spawn(async move { app.oneshot(request).await.unwrap().status() })
    };
    let wins = [win(&one, &one_etag), win(&two, &two_etag)];
    let mut waiting = 0;
    for _ in 0..200 {
        waiting = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting >= 2 {
            break;
        }
        assert!(
            wins.iter().all(|w| !w.is_finished()),
            "a win finished without waiting for the approval"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(waiting, 2, "both wins wait on the approval");
    holding.commit().await.unwrap();
    let mut statuses = Vec::new();
    for w in wins {
        statuses.push(w.await.unwrap());
    }
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    let mut won = 0;
    for deal in [&one, &two] {
        if server.row("deals", deal).await["stage"] == "won" {
            won += 1;
        }
    }
    assert_eq!(won, 1, "exactly one deal won");
    assert_eq!(server.row("approvals", &approval).await["consumed"], true);
    server.db.drop().await;
}

/// ADR-041: `HOOKED` with a change log on every model (approvals have no
/// write hooks), the deal's log published on its CRUD endpoint, a handler
/// that reads it with `Ctx::history`, and one that consumes an approval
/// with an unversioned `Ctx::update`.
fn logged() -> trestle::Project {
    hooked_with(|doc| {
        for model in &mut doc.models {
            model.history = true;
        }
        for endpoint in &mut doc.endpoints {
            endpoint.history =
                endpoint.model.as_deref() == Some("crm.deal") && endpoint.id.is_none();
        }
        doc.endpoints.push(trestle::intent::EndpointDoc {
            id: Some("crm.trail".into()),
            model: Some("crm.deal".into()),
            path: Some("GET /api/v1/deals/{id}/trail".into()),
            permission: Some("crm.read".into()),
            handler: Some("crm::trail".into()),
            ..Default::default()
        });
        doc.endpoints.push(trestle::intent::EndpointDoc {
            id: Some("crm.consume".into()),
            model: Some("crm.approval".into()),
            path: Some("POST /api/v1/approvals/{id}/consume".into()),
            permission: Some("crm.write".into()),
            handler: Some("crm::consume".into()),
            ..Default::default()
        });
    })
    .handler("crm::trail", trail)
    .handler("crm::consume", consume)
}

async fn consume(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("crm.approval", input.id()?, json!({ "consumed": true }))
        .await
}

/// The deal, with its note naming its log entries, newest first.
async fn trail(ctx: Ctx, input: Input) -> Result<Row> {
    let id = input.id()?;
    let entries = ctx.history("crm.deal", id).await?;
    let mut row = ctx.get("crm.deal", id).await?;
    let names: Vec<String> = entries
        .iter()
        .map(|e| format!("{} {}", e.kind.as_str(), e.operation))
        .collect();
    row.set("note", names.join(", "))?;
    Ok(row)
}

impl Server {
    /// A row's change log as stored, oldest first: kind, version,
    /// operation, outer operation, actor and changes.
    async fn logged(&self, table: &str, id: &str) -> Vec<Value> {
        let pool = self.db.pool().await;
        let rows: Vec<(String, i64, String, Option<String>, uuid::Uuid, String)> =
            sqlx::query_as(&format!(
                "SELECT kind, version, operation, outer_operation, actor_id, changes \
                 FROM {table}_history WHERE row_id = $1::uuid ORDER BY version, id"
            ))
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
        rows.into_iter()
            .map(|(kind, version, operation, outer, actor, changes)| {
                json!({
                    "kind": kind,
                    "version": version,
                    "operation": operation,
                    "outer": outer,
                    "actor": actor.to_string(),
                    "changes": serde_json::from_str::<Value>(&changes).unwrap(),
                })
            })
            .collect()
    }
}

#[test]
fn history_is_checked_when_the_file_is_read_and_planned_as_schema() {
    let problems = |text: &str| match trestle::intent::parse("trestle.toml", text) {
        Ok(_) => panic!("parsed:\n{text}"),
        Err(e) => e.problems.join("\n"),
    };
    let published = CRM.replace(
        r#"crud = ["get", "create"]"#,
        "crud = [\"create\"]\nhistory = true",
    );
    let refused = problems(&published);
    assert!(
        refused.contains("history = true publishes the change log of crm.deal, which needs history = true on the model crm.deal"),
        "{refused}"
    );
    assert!(
        refused.contains(r#"history = true shows the change log of rows the caller can get; add "get" to crud = [...]"#),
        "{refused}"
    );
    let handler = problems(&CRM.replace(
        "set = { stage = \"open\" }",
        "set = { stage = \"open\" }\nhistory = true",
    ));
    assert!(
        handler.contains(r#""history" is for CRUD endpoints"#),
        "{handler}"
    );
    let taken = problems(
        &(CRM.replace("id = \"crm.deal\"\n", "id = \"crm.deal\"\nhistory = true\n")
            + "\n[[models]]\nid = \"crm.log\"\ntable = \"deals_history\"\nfields = [{ id = \"note\", type = \"text\" }]\n"),
    );
    assert!(
        taken.contains(r#"trestle.toml:15: history of crm.deal is stored as model "crm.deal_history" in table "deals_history", which this file already uses"#),
        "{taken}"
    );

    // Turning history on is a schema change: `makemigrations` plans the
    // log's table, and the plan is then clean.
    let logged = CRM.replace("id = \"crm.deal\"\n", "id = \"crm.deal\"\nhistory = true\n");
    // A write declares the log it appends to (ADR-018).
    let registry = trestle::project()
        .intent_str("trestle.toml", &logged)
        .assemble()
        .unwrap();
    for op in ["crm.win", "crm.deal.create"] {
        let info = registry.inspect_operation(op).unwrap();
        assert_eq!(
            info["effects"]["writes"],
            json!(["crm.deal", "crm.deal_history"]),
            "{op}"
        );
    }
    let dir = std::env::temp_dir().join(format!("trestle-u20-{}", uuid::Uuid::new_v4()));
    cli(CRM, &dir, &["makemigrations", "--name", "initial"]);
    cli(&logged, &dir, &["makemigrations", "--name", "history"]);
    let sql = std::fs::read_to_string(dir.join("crm").join("0002_history.sql")).unwrap();
    assert!(sql.contains("CREATE TABLE deals_history"), "{sql}");
    assert!(
        sql.contains("changed_at timestamptz NOT NULL DEFAULT now()"),
        "{sql}"
    );
    assert!(
        !sql.contains("ALTER TABLE deals "),
        "the model's own table is unchanged: {sql}"
    );
    cli(&logged, &dir, &["makemigrations", "--check"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn history_logs_every_committed_write_once_on_every_path() {
    let server = Server::start_with(logged()).await;
    let entry = |kind: &str, version: i64, operation: &str, outer: &str, changes: Value| {
        json!({
            "kind": kind, "version": version, "operation": operation, "outer": outer,
            "actor": MEMBER_ID, "changes": changes,
        })
    };
    let change = |old: Value, new: Value| json!({ "old": old, "new": new });

    // A CRUD create logs the row as written, and the organization's own
    // after_write (a Ctx write in the create's transaction) logs once more.
    let acme = server.organization("Acme").await;
    let created = "crm.organization.create";
    assert_eq!(
        server.logged("organizations", &acme).await,
        [
            entry(
                "create",
                1,
                created,
                created,
                json!({
                    "name": change(json!(null), json!("Acme")),
                    "customer": change(json!(null), json!(false)),
                    "writes": change(json!(null), json!(0)),
                })
            ),
            entry(
                "update",
                2,
                "ctx",
                created,
                json!({
                    "writes": change(json!(0), json!(1)),
                    "last": change(json!(null), json!(created)),
                })
            ),
        ]
    );

    // Refused writes log nothing: the action, a CRUD update and a handler
    // without an approval.
    let (deal, etag) = server.hooked_deal(&acme, "One").await;
    let win = format!("/api/v1/deals/{deal}/win");
    assert_eq!(
        server.post(&win, json!({}), Some(&etag)).await.0,
        StatusCode::CONFLICT
    );
    let patch = server
        .send(
            "PATCH",
            &format!("/api/v1/deals/{deal}"),
            MEMBER,
            Some(json!({ "stage": "won" })),
            Some(&etag),
        )
        .await;
    assert_eq!(patch.0, StatusCode::CONFLICT);
    let close = server
        .post(&format!("/api/v1/deals/{deal}/close"), json!({}), None)
        .await;
    assert_eq!(close.0, StatusCode::CONFLICT);
    assert_eq!(
        server.logged("deals", &deal).await.len(),
        1,
        "only the create"
    );

    // The action logs one update with its own operation, and the nested
    // Ctx::update of its after_write logs one on the organization, with
    // the action as the outer operation.
    server
        .create("approvals", json!({ "organization": acme }))
        .await;
    let (status, won, _) = server.post(&win, json!({}), Some(&etag)).await;
    assert_eq!(status, StatusCode::OK, "{won}");
    let log = server.logged("deals", &deal).await;
    assert_eq!(log.len(), 2, "{log:#?}");
    assert_eq!(
        log[1],
        entry(
            "update",
            2,
            "crm.win",
            "crm.win",
            json!({ "stage": change(json!("open"), json!("won")) })
        )
    );
    let org_log = server.logged("organizations", &acme).await;
    assert_eq!(org_log.len(), 4, "{org_log:#?}");
    assert_eq!(
        org_log[2],
        entry(
            "update",
            3,
            "ctx",
            "crm.win",
            json!({ "customer": change(json!(false), json!(true)) })
        )
    );
    assert_eq!(org_log[3]["outer"], "crm.win");

    // A CRUD update refused after the row write (by the endpoint's
    // after_update) rolls its log entries back with it.
    let beta = server.organization("Beta").await;
    server
        .create("approvals", json!({ "organization": beta }))
        .await;
    let (two, etag) = server.hooked_deal(&beta, "Two").await;
    let before = server.logged("organizations", &beta).await.len();
    let path = format!("/api/v1/deals/{two}");
    let late = server
        .send(
            "PATCH",
            &path,
            MEMBER,
            Some(json!({ "stage": "won", "title": "Fail late" })),
            Some(&etag),
        )
        .await;
    assert_eq!(late.0, StatusCode::CONFLICT, "{}", late.1);
    assert_eq!(server.logged("deals", &two).await.len(), 1);
    assert_eq!(server.logged("organizations", &beta).await.len(), before);
    // The same update committed: one entry, with every changed field.
    let (status, _, _) = server
        .send(
            "PATCH",
            &path,
            MEMBER,
            Some(json!({ "stage": "won", "title": "Order" })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let log = server.logged("deals", &two).await;
    assert_eq!(
        log[1],
        entry(
            "update",
            2,
            "crm.deal.update",
            "crm.deal.update",
            json!({
                "stage": change(json!("open"), json!("won")),
                "title": change(json!("Two"), json!("Order")),
                "note": change(json!(null), json!("endpoint, model")),
            })
        )
    );

    // The published log: newest first, a page at a time, for any reader of
    // the row; another tenant's caller gets the 404 of a missing row.
    let url = format!("/api/v1/deals/{deal}/history");
    let (status, page, _) = server
        .send("GET", &format!("{url}?limit=1"), VIEWER, None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let newest = &page["items"][0];
    assert_eq!(
        (
            &newest["kind"],
            &newest["version"],
            &newest["operation"],
            &newest["outer_operation"],
            &newest["actor"]
        ),
        (
            &json!("update"),
            &json!(2),
            &json!("crm.win"),
            &json!("crm.win"),
            &json!(MEMBER_ID)
        )
    );
    assert_eq!(
        newest["changes"],
        json!([{ "change": { "field": "stage", "old": "open", "new": "won" } }])
    );
    assert!(newest["at"].as_str().unwrap().ends_with('Z'), "{newest}");
    let cursor = page["next_cursor"].as_str().unwrap();
    let (_, rest, _) = server
        .send(
            "GET",
            &format!("{url}?limit=1&cursor={cursor}"),
            VIEWER,
            None,
            None,
        )
        .await;
    assert_eq!(rest["items"][0]["kind"], "create", "{rest}");
    assert_eq!(rest["next_cursor"], Value::Null);
    let (status, _, _) = server.send("GET", &url, OUTSIDER, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // An unversioned Ctx::update of a model without hooks still logs the
    // value it replaced.
    let spare = server
        .create("approvals", json!({ "organization": acme }))
        .await;
    let (status, _, _) = server
        .post(
            &format!("/api/v1/approvals/{spare}/consume"),
            json!({}),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        server.logged("approvals", &spare).await[1],
        entry(
            "update",
            2,
            "ctx",
            "crm.consume",
            json!({ "consumed": change(json!(false), json!(true)) })
        )
    );
    // Rust reads the same log through Ctx::history.
    let (status, row, _) = server
        .send(
            "GET",
            &format!("/api/v1/deals/{deal}/trail"),
            MEMBER,
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["note"], "update crm.win, create crm.deal.create");
    server.db.drop().await;
}

/// `after_write` on deal, for [`a_hook_writing_another_row_of_its_model_runs_that_rows_hooks`]:
/// a deal noted `win <id>` wins that other deal through `Ctx`.
async fn win_named(ctx: Ctx, write: trestle::intent::Write) -> Result<trestle::intent::Write> {
    let named = match write.row() {
        Some(new) => new.get::<Option<String>>("note")?,
        None => None,
    };
    if let Some(other) = named.as_deref().and_then(|n| n.strip_prefix("win ")) {
        let other: uuid::Uuid = other.parse().map_err(|_| Problem::conflict("not_a_deal"))?;
        ctx.update("crm.deal", other, json!({ "stage": "won" }))
            .await?;
    }
    Ok(write)
}

#[tokio::test]
async fn a_hook_writing_another_row_of_its_model_runs_that_rows_hooks() {
    // Hooks are skipped only for a row whose hooks are already running
    // (ADR-039): deal A's after_write winning deal B runs B's
    // before_write, so B can't be won without an approval that way either.
    use std::collections::BTreeMap;
    let mut doc = trestle::intent::IntentDoc::from_toml("trestle.toml", HOOKED).unwrap();
    for model in &mut doc.models {
        if model.id == "crm.deal" {
            model.hooks = BTreeMap::from([
                ("before_write".into(), "crm::deal_rules".into()),
                ("after_write".into(), "crm::win_named".into()),
            ]);
        }
    }
    let project = trestle::project()
        .intent_doc("crm", doc)
        .handler("crm::close", close)
        .hook("crm::mark", mark)
        .hook("crm::late", late)
        .write_hook("crm::deal_rules", deal_rules)
        .write_hook("crm::win_named", win_named);
    let server = Server::start_with(project).await;
    let acme = server.organization("Acme").await;
    let (a, a_etag) = server.hooked_deal(&acme, "A").await;
    let (b, _) = server.hooked_deal(&acme, "B").await;
    let name_b = |etag: String| {
        let server = &server;
        let uri = format!("/api/v1/deals/{a}");
        let note = format!("win {b}");
        async move {
            server
                .send(
                    "PATCH",
                    &uri,
                    MEMBER,
                    Some(json!({ "note": note })),
                    Some(&etag),
                )
                .await
        }
    };

    // No approval: B's rule refuses, and A's update rolls back with it.
    let (status, problem, _) = name_b(a_etag.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert_eq!(problem["code"], "approval_required");
    assert_eq!(server.row("deals", &b).await["stage"], "open");
    assert_eq!(server.row("deals", &a).await["note"], Value::Null);

    // With one, B is won and the approval consumed, by B's own rule.
    let approval = server
        .create("approvals", json!({ "organization": acme }))
        .await;
    let (status, body, _) = name_b(a_etag).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(server.row("deals", &b).await["stage"], "won");
    assert_eq!(server.row("approvals", &approval).await["consumed"], true);
    server.db.drop().await;
}
