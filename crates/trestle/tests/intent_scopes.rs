//! ADR-040: row-scoped authority. A model declares row scopes that tie a
//! row to the acting actor (`own`: the owner's member row has the actor;
//! `team`: a team membership row has it), and a role may hold a permission
//! for some scopes only (`"crm.sell@own"`). Generated operations then see
//! and change only those rows: other rows are 404 like another tenant's,
//! a write that would leave the scopes is a 403, and the scope is rechecked
//! inside the write's transaction. ADR-041: a row's published change log
//! is as hidden as the row. ADR-044: a `through` scope's `where` admits
//! only matching `through` rows. Needs PostgreSQL.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use trestle::core::identity::{ActorId, DevTokenIdentity};
use trestle::core::invocation::Invocation;
use trestle::historical::planner::{PlanOptions, diff};
use trestle::historical::schema::SchemaState;
use trestle::historical::testing::TestDatabase;
use trestle::prelude::*;
use uuid::Uuid;

const CRM: &str = r#"
[project]
name = "crm"

[tenancy]
model = "workspace"

[roles]
salesperson = ["crm.read", "crm.sell@own"]
manager = ["crm.read", "crm.sell@team"]
director = ["crm.read", "crm.sell"]
admin = ["crm.read", "crm.admin"]

[[models]]
id = "crm.member"
fields = [
  { id = "name", type = "text", max = 100 },
  { id = "actor", type = "uuid" },
]

[[models]]
id = "crm.team"
fields = [{ id = "name", type = "text", max = 100 }]

[[models]]
id = "crm.team_member"
fields = [
  { id = "team", type = "ref", to = "crm.team" },
  { id = "actor", type = "uuid" },
]

[[models]]
id = "crm.opportunity"
table = "opportunities"
fields = [
  { id = "title", type = "text", max = 100 },
  { id = "stage", type = "enum", values = ["open", "won"], default = "open" },
  { id = "owner", type = "ref", to = "crm.member" },
  { id = "team", type = "ref", to = "crm.team" },
]

[models.scopes]
own = { field = "owner", through = "crm.member", actor = "actor" }
team = { field = "team", through = "crm.team_member", key = "team", actor = "actor" }

[[endpoints]]
model = "crm.member"
crud = ["list", "get"]
read = "crm.read"

[[endpoints]]
model = "crm.opportunity"
crud = ["list", "get", "create", "update", "delete"]
read = "crm.sell"
write = "crm.sell"
filter = ["stage"]
search = "title"
order = "title"
idempotent = true

[[endpoints]]
id = "crm.win"
model = "crm.opportunity"
path = "POST /api/v1/opportunities/{id}/win"
permission = "crm.sell"
set = { stage = "won" }
when = { stage = ["open"] }

[[endpoints]]
id = "crm.peek"
model = "crm.opportunity"
path = "GET /api/v1/opportunities/{id}/peek"
permission = "crm.sell"
handler = "crm::peek"
"#;

/// A handler: its `Ctx` sees the whole tenant.
async fn peek(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.get("crm.opportunity", input.id()?).await
}

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";
const S3: &str = "33333333-3333-4333-8333-333333333333";
const MANAGER: &str = "44444444-4444-4444-8444-444444444444";
const DIRECTOR: &str = "55555555-5555-4555-8555-555555555555";
const ADMIN: &str = "66666666-6666-4666-8666-666666666666";

struct Crm {
    app: Router,
    db: TestDatabase,
    registry: trestle::core::registry::Registry<State>,
    state: State,
    north: Uuid,
    /// Member rows of S1, S2, S3 and the manager.
    members: [String; 4],
    team_a: String,
    team_b: String,
    /// S1's and S2's in team A, S3's in team B.
    opportunities: [String; 3],
}

async fn insert(pool: &sqlx::PgPool, sql: &str, binds: &[Uuid]) -> String {
    let id = Uuid::now_v7();
    let mut q = sqlx::query(sql).bind(id);
    for b in binds {
        q = q.bind(*b);
    }
    q.execute(pool).await.unwrap();
    id.to_string()
}

fn uuid(text: &str) -> Uuid {
    Uuid::parse_str(text).unwrap()
}

const TEAM_SCOPE: &str =
    r#"team = { field = "team", through = "crm.team_member", key = "team", actor = "actor" }"#;

/// [`CRM`] with an `active` flag and a `role` on team memberships, and a
/// `team` scope that admits only active managers and leads (ADR-044).
fn conditioned() -> String {
    let fields = "to = \"crm.team\" },\n  { id = \"actor\", type = \"uuid\" },\n";
    assert_eq!(CRM.matches(fields).count(), 1);
    assert_eq!(CRM.matches(TEAM_SCOPE).count(), 1);
    CRM.replace(
        fields,
        &format!(
            "{fields}  {{ id = \"active\", type = \"bool\", default = true }},\n  \
             {{ id = \"role\", type = \"enum\", values = [\"member\", \"manager\", \"lead\"], \
             default = \"member\" }},\n"
        ),
    )
    .replace(
        TEAM_SCOPE,
        &TEAM_SCOPE.replace(
            " }",
            r#", where = { active = true, role = ["manager", "lead"] } }"#,
        ),
    )
}

/// Waits until a query of the database is blocked on a lock, failing if
/// `task` finishes first.
async fn wait_for_lock<T>(pool: &sqlx::PgPool, task: &tokio::task::JoinHandle<T>, what: &str) {
    for _ in 0..200 {
        assert!(!task.is_finished(), "the action did not wait for {what}");
        let blocked: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the action never waited on {what}");
}

impl Crm {
    async fn start() -> Self {
        Self::start_with(CRM).await
    }

    async fn start_with(text: &str) -> Self {
        let registry = trestle::project()
            .intent_str("trestle.toml", text)
            .handler("crm::peek", peek)
            .assemble()
            .unwrap();
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
        let north = Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'North')")
            .bind(north)
            .execute(&pool)
            .await
            .unwrap();
        for (actor, role) in [
            (S1, "salesperson"),
            (S2, "salesperson"),
            (S3, "salesperson"),
            (MANAGER, "manager"),
            (DIRECTOR, "director"),
            (ADMIN, "admin"),
        ] {
            sqlx::query(
                "INSERT INTO workspace_memberships (workspace_id, actor_id, role) VALUES ($1, $2, $3)",
            )
            .bind(north)
            .bind(uuid(actor))
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
        }
        let mut members = Vec::new();
        for actor in [S1, S2, S3, MANAGER] {
            members.push(
                insert(
                    &pool,
                    "INSERT INTO members (id, workspace_id, name, actor) VALUES ($1, $2, 'M', $3)",
                    &[north, uuid(actor)],
                )
                .await,
            );
        }
        let team = "INSERT INTO teams (id, workspace_id, name) VALUES ($1, $2, 'T')";
        let team_a = insert(&pool, team, &[north]).await;
        let team_b = insert(&pool, team, &[north]).await;
        for (team, actor) in [
            (&team_a, S1),
            (&team_a, S2),
            (&team_a, MANAGER),
            (&team_b, S3),
        ] {
            insert(
                &pool,
                "INSERT INTO team_members (id, workspace_id, team, actor) VALUES ($1, $2, $3, $4)",
                &[north, uuid(team), uuid(actor)],
            )
            .await;
        }
        // Each actor's token is its own ID.
        let tokens: Vec<String> = [S1, S2, S3, MANAGER, DIRECTOR, ADMIN]
            .iter()
            .map(|a| format!("{a}={a}"))
            .collect();
        let identity = Arc::new(DevTokenIdentity::parse(&tokens.join(",")).unwrap());
        let state = State::new(pool, identity);
        let app = trestle::router_with(&registry, state.clone(), Default::default());
        let mut crm = Crm {
            app,
            db,
            registry,
            state,
            north,
            members: members.try_into().unwrap(),
            team_a,
            team_b,
            opportunities: Default::default(),
        };
        // An unscoped grant creates rows for anyone.
        let mut opportunities = Vec::new();
        for (title, owner, team) in [
            ("Alpha", 0, crm.team_a.clone()),
            ("Bravo", 1, crm.team_a.clone()),
            ("Charlie", 2, crm.team_b.clone()),
        ] {
            let (status, body, _) = crm
                .send(
                    "POST",
                    "/api/v1/opportunities",
                    DIRECTOR,
                    Some(json!({ "title": title, "owner": crm.members[owner], "team": team })),
                    None,
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            opportunities.push(body["id"].as_str().unwrap().to_owned());
        }
        crm.opportunities = opportunities.try_into().unwrap();
        crm
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        actor: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let res = self
            .app
            .clone()
            .oneshot(self.request(method, uri, actor, body, if_match))
            .await
            .unwrap();
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

    fn request(
        &self,
        method: &str,
        uri: &str,
        actor: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {actor}"))
            .header("Trestle-Tenant", self.north.to_string())
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(tag) = if_match {
            req = req.header(header::IF_MATCH, tag);
        }
        req.body(Body::from(body.unwrap_or(json!({})).to_string()))
            .unwrap()
    }

    /// [`Crm::send`] with an `Idempotency-Key`: the status, the body, and
    /// whether it was a replay.
    async fn keyed(
        &self,
        method: &str,
        uri: &str,
        actor: &str,
        body: Value,
        if_match: Option<&str>,
        key: &str,
    ) -> (StatusCode, Value, bool) {
        let mut req = self.request(method, uri, actor, Some(body), if_match);
        req.headers_mut()
            .insert("Idempotency-Key", key.parse().unwrap());
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let replayed = res.headers().contains_key("idempotency-replayed");
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            replayed,
        )
    }

    /// Titles `actor` lists, in order.
    async fn titles(&self, actor: &str, query: &str) -> Vec<String> {
        let (status, page, _) = self
            .send(
                "GET",
                &format!("/api/v1/opportunities{query}"),
                actor,
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["title"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The row as the director sees it, with its ETag.
    async fn row(&self, id: &str) -> (Value, String) {
        let (status, row, etag) = self
            .send(
                "GET",
                &format!("/api/v1/opportunities/{id}"),
                DIRECTOR,
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{row}");
        (row, etag.unwrap())
    }
}

#[test]
fn scopes_and_scoped_grants_are_checked_when_the_file_is_read() {
    trestle::intent::parse("trestle.toml", CRM).unwrap_or_else(|e| panic!("{e}"));
    let bad = CRM
        .replace(
            r#""crm.sell@team"]"#,
            r#""crm.sell@team", "crm.sell@region", "authenticated@own", "crm.sell@"]"#,
        )
        .replace(
            TEAM_SCOPE,
            r#"team = { field = "team", through = "crm.team_member", key = "actor", actor = "team" }
mine = { field = "title" }
lead = { field = "owner", through = "crm.lead", actor = "actor" }
peer = { field = "owner", through = "crm.member" }
odd = { field = "nothing" }"#,
        );
    let problems = trestle::intent::parse("trestle.toml", &bad)
        .unwrap_err()
        .problems;
    for needle in [
        r#"grant "crm.sell@region" names scope "region", which no model declares"#,
        r#""authenticated" is held by every signed-in actor"#,
        r#"grant "crm.sell@" must be "<permission>@<scope>""#,
        r#"scope team of crm.opportunity: actor "team" must be a uuid field of crm.team_member"#,
        r#"scope team of crm.opportunity: field "team" and crm.team_member.actor are not the same type"#,
        r#"scope mine of crm.opportunity: without through, field "title" must be a uuid field"#,
        r#"scope lead of crm.opportunity: through "crm.lead" is not a model in this file"#,
        r#"scope peer of crm.opportunity needs actor = "<uuid field of crm.member>""#,
        r#"scope odd of crm.opportunity: field "nothing" is not a field of crm.opportunity"#,
    ] {
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "no problem says {needle:?}: {problems:#?}"
        );
    }
    assert_eq!(problems.len(), 9, "{problems:#?}");
    // Problems carry the line of the scope or grant they are about.
    let line = |needle: &str| {
        let problem = problems.iter().find(|p| p.contains(needle)).unwrap();
        let n: usize = problem.split(':').nth(1).unwrap().parse().unwrap();
        bad.lines().nth(n - 1).unwrap().to_owned()
    };
    assert!(line("scope lead").starts_with("lead = "));
    assert!(line("crm.sell@region").starts_with("manager = "));
}

#[test]
fn scoped_operations_declare_the_models_their_scopes_read() {
    let registry = trestle::project()
        .intent_str("trestle.toml", CRM)
        .handler("crm::peek", peek)
        .assemble()
        .unwrap();
    let list = registry.inspect_operation("crm.opportunity.list").unwrap();
    assert_eq!(
        list["effects"]["reads"],
        json!(["crm.member", "crm.opportunity", "crm.team_member"])
    );
    let win = registry.inspect_operation("crm.win").unwrap();
    assert_eq!(win["effects"]["writes"], json!(["crm.opportunity"]));
    assert_eq!(
        win["effects"]["reads"],
        json!(["crm.member", "crm.team_member"])
    );
}

#[tokio::test]
async fn a_scoped_grant_admits_only_the_rows_of_its_scopes() {
    let crm = Crm::start().await;
    let [alpha, bravo, charlie] = crm.opportunities.clone();
    let [m1, m2, _, _] = crm.members.clone();

    // Lists are filtered in SQL: own rows, team rows, every row.
    assert_eq!(crm.titles(S1, "").await, ["Alpha"]);
    assert_eq!(crm.titles(S3, "").await, ["Charlie"]);
    assert_eq!(crm.titles(MANAGER, "").await, ["Alpha", "Bravo"]);
    assert_eq!(
        crm.titles(DIRECTOR, "").await,
        ["Alpha", "Bravo", "Charlie"]
    );
    // Filters and search never reach past the scopes.
    assert_eq!(crm.titles(S1, "?q=a").await, ["Alpha"]);
    assert_eq!(crm.titles(MANAGER, "?stage=open&q=o").await, ["Bravo"]);
    // A role without the permission is refused, scoped or not.
    let (status, refused, _) = crm
        .send("GET", "/api/v1/opportunities", ADMIN, None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["code"], "permission_denied");

    // Another salesperson's row is missing, on every path.
    let (_, etag) = crm.row(&bravo).await;
    let url = format!("/api/v1/opportunities/{bravo}");
    for (method, uri, body) in [
        ("GET", url.clone(), None),
        ("PATCH", url.clone(), Some(json!({ "title": "Mine" }))),
        ("DELETE", url.clone(), None),
        ("POST", format!("{url}/win"), None),
    ] {
        let (status, body, _) = crm.send(method, &uri, S1, body, Some(&etag)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
        assert_eq!(body["code"], "not_found");
    }
    let direct = crm
        .registry
        .invoke(
            &crm.state,
            "crm.opportunity.get",
            &Invocation::for_actor(ActorId(uuid(S1))).with_tenant(crm.north),
            &json!({ "id": bravo }),
        )
        .await
        .expect_err("another salesperson's row");
    assert_eq!(direct.code(), "not_found");
    assert_eq!(crm.row(&bravo).await.0["title"], "Bravo");

    // Creating: the new row must be inside the scopes.
    let (status, refused, _) = crm
        .send(
            "POST",
            "/api/v1/opportunities",
            S1,
            Some(json!({ "title": "Gift", "owner": m2, "team": crm.team_a })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["code"], "out_of_scope");
    assert_eq!(
        crm.titles(DIRECTOR, "").await.len(),
        3,
        "nothing was created"
    );
    let (status, delta, _) = crm
        .send(
            "POST",
            "/api/v1/opportunities",
            S1,
            Some(json!({ "title": "Delta", "owner": m1, "team": crm.team_b })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{delta}");

    // Pages and cursors cover only the caller's rows.
    let (_, first, _) = crm
        .send("GET", "/api/v1/opportunities?limit=1", S1, None, None)
        .await;
    assert_eq!(first["items"][0]["title"], "Alpha");
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let (_, second, _) = crm
        .send(
            "GET",
            &format!("/api/v1/opportunities?limit=1&cursor={cursor}"),
            S1,
            None,
            None,
        )
        .await;
    assert_eq!(second["items"][0]["title"], "Delta");
    assert_eq!(second["next_cursor"], Value::Null);

    // A salesperson can't hand a row over: it would leave `own`.
    let (_, etag) = crm.row(&alpha).await;
    let alpha_url = format!("/api/v1/opportunities/{alpha}");
    let (status, refused, _) = crm
        .send(
            "PATCH",
            &alpha_url,
            S1,
            Some(json!({ "owner": m2 })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["code"], "out_of_scope");
    assert_eq!(crm.row(&alpha).await.0["owner"], m1.as_str());
    let (status, renamed, _) = crm
        .send(
            "PATCH",
            &alpha_url,
            S1,
            Some(json!({ "title": "Alpha 2" })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");

    // A handler's Ctx sees the whole tenant, so a scoped grant doesn't
    // admit it; an unscoped one does.
    let peek_url = format!("/api/v1/opportunities/{alpha}/peek");
    let (status, refused, _) = crm.send("GET", &peek_url, S1, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(refused["code"], "permission_denied");
    let (status, _, _) = crm.send("GET", &peek_url, DIRECTOR, None, None).await;
    assert_eq!(status, StatusCode::OK);

    // The manager acts on team A's rows, not team B's.
    let (_, etag) = crm.row(&bravo).await;
    let (status, won, _) = crm
        .send("POST", &format!("{url}/win"), MANAGER, None, Some(&etag))
        .await;
    assert_eq!(status, StatusCode::OK, "{won}");
    assert_eq!(won["stage"], "won");
    let (_, etag) = crm.row(&charlie).await;
    let (status, _, _) = crm
        .send(
            "POST",
            &format!("/api/v1/opportunities/{charlie}/win"),
            MANAGER,
            None,
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Reassigning within the team keeps the row in `team`; moving it to
    // another team doesn't.
    let (_, etag) = crm.row(&alpha).await;
    let (status, moved, etag) = crm
        .send(
            "PATCH",
            &alpha_url,
            MANAGER,
            Some(json!({ "owner": m2 })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    assert_eq!(moved["owner"], m2.as_str());
    let (status, refused, _) = crm
        .send(
            "PATCH",
            &alpha_url,
            MANAGER,
            Some(json!({ "team": crm.team_b })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["code"], "out_of_scope");
    assert_eq!(crm.row(&alpha).await.0["team"], crm.team_a.as_str());
    // S1 no longer owns Alpha, so it's gone from their list.
    assert_eq!(crm.titles(S1, "").await, ["Delta"]);
    crm.db.drop().await;
}

#[tokio::test]
async fn a_scoped_write_rechecks_its_scope_in_the_transaction() {
    let crm = Crm::start().await;
    let pool = crm.db.pool().await;
    let bravo = crm.opportunities[1].clone();
    let (_, etag) = crm.row(&bravo).await;

    // Another transaction removes the manager from team A and hasn't
    // committed. The manager's action locks the team membership it relies
    // on FOR SHARE, so it waits, then finds it gone: the row is missing.
    let mut removing = pool.begin().await.unwrap();
    let removed = sqlx::query("DELETE FROM team_members WHERE actor = $1")
        .bind(uuid(MANAGER))
        .execute(&mut *removing)
        .await
        .unwrap();
    assert_eq!(removed.rows_affected(), 1);
    let request = crm.request(
        "POST",
        &format!("/api/v1/opportunities/{bravo}/win"),
        MANAGER,
        None,
        Some(&etag),
    );
    let app = crm.app.clone();
    let action = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    wait_for_lock(&pool, &action, "the team membership").await;
    removing.commit().await.unwrap();
    let res = action.await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(crm.row(&bravo).await.0["stage"], "open");

    crm.db.drop().await;
}

#[tokio::test]
async fn a_replay_rechecks_the_rows_scope() {
    let crm = Crm::start().await;
    let pool = crm.db.pool().await;
    let [m1, ..] = crm.members.clone();
    let bravo = crm.opportunities[1].clone();
    let create = json!({ "title": "Echo", "owner": m1, "team": crm.team_a });
    let (status, created, replayed) = crm
        .keyed(
            "POST",
            "/api/v1/opportunities",
            MANAGER,
            create.clone(),
            None,
            "create-1",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert!(!replayed);
    let (_, etag) = crm.row(&bravo).await;
    let patch = json!({ "title": "Bravo 2" });
    let bravo_url = format!("/api/v1/opportunities/{bravo}");
    let (status, updated, _) = crm
        .keyed(
            "PATCH",
            &bravo_url,
            MANAGER,
            patch.clone(),
            Some(&etag),
            "update-1",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    // While the manager is in team A, a replay returns the stored answer.
    let (status, again, replayed) = crm
        .keyed(
            "POST",
            "/api/v1/opportunities",
            MANAGER,
            create.clone(),
            None,
            "create-1",
        )
        .await;
    assert_eq!((status, replayed), (StatusCode::CREATED, true));
    assert_eq!(again, created);

    // Out of team A, a replay is the 404 of a fresh request, without the
    // stored row.
    sqlx::query("DELETE FROM team_members WHERE actor = $1")
        .bind(uuid(MANAGER))
        .execute(&pool)
        .await
        .unwrap();
    for (method, uri, body, if_match, key) in [
        ("POST", "/api/v1/opportunities", create, None, "create-1"),
        (
            "PATCH",
            bravo_url.as_str(),
            patch,
            Some(etag.as_str()),
            "update-1",
        ),
    ] {
        let (status, body, replayed) = crm.keyed(method, uri, MANAGER, body, if_match, key).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method}: {body}");
        assert!(!replayed, "{method}");
        assert_eq!(body["code"], "not_found");
        assert!(
            body.get("id").is_none() && body.get("owner").is_none(),
            "{method}: {body}"
        );
    }
    crm.db.drop().await;
}

#[test]
fn scope_conditions_are_checked_when_the_file_is_read() {
    let text = conditioned();
    let intent = trestle::intent::parse("trestle.toml", &text).unwrap_or_else(|e| panic!("{e}"));
    let model = intent
        .models
        .iter()
        .find(|m| m.id == "crm.opportunity")
        .unwrap();
    let through = model.scopes["team"].through.as_ref().unwrap();
    assert_eq!(
        through.conditions,
        [
            ("active".to_owned(), vec![json!(true)]),
            ("role".to_owned(), vec![json!("manager"), json!("lead")]),
        ]
    );
    let bad = text
        .replace(
            r#"where = { active = true, role = ["manager", "lead"] }"#,
            r#"where = { active = "yes", role = ["manager", "boss"], rank = 1, actor = "x" }"#,
        )
        .replace(
            "  { id = \"actor\", type = \"uuid\" },\n]\n\n[[models]]\nid = \"crm.team\"",
            "  { id = \"actor\", type = \"uuid\" },\n]\nscopes = { me = { field = \"actor\", \
             where = { name = \"M\" } } }\n\n[[models]]\nid = \"crm.team\"",
        );
    let problems = trestle::intent::parse("trestle.toml", &bad)
        .unwrap_err()
        .problems;
    for needle in [
        r#"scope team of crm.opportunity: where.active = "yes" does not fit crm.team_member.active"#,
        r#"scope team of crm.opportunity: where.role = ["manager", "boss"] does not fit"#,
        "scope team of crm.opportunity: where.rank is not a field of crm.team_member",
        "scope team of crm.opportunity: where.actor must be a bool, int, text or enum field",
        "scope me of crm.member: where is for a scope with through",
    ] {
        assert!(
            problems.iter().any(|p| p.contains(needle)),
            "no problem says {needle:?}: {problems:#?}"
        );
    }
    assert_eq!(problems.len(), 5, "{problems:#?}");
}

#[tokio::test]
async fn a_scope_condition_admits_only_matching_through_rows() {
    let crm = Crm::start_with(&conditioned()).await;
    let pool = crm.db.pool().await;
    let bravo = crm.opportunities[1].clone();
    let bravo_url = format!("/api/v1/opportunities/{bravo}");
    let membership = |set: &'static str| {
        let pool = pool.clone();
        async move {
            let sql = format!("UPDATE team_members SET {set} WHERE actor = $1");
            let updated = sqlx::query(&sql)
                .bind(uuid(MANAGER))
                .execute(&pool)
                .await
                .unwrap();
            assert_eq!(updated.rows_affected(), 1);
        }
    };
    // A plain member of team A: the membership exists but doesn't match.
    assert!(crm.titles(MANAGER, "").await.is_empty());
    let (status, _, _) = crm.send("GET", &bravo_url, MANAGER, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // A manager, or a lead (one of the listed values), reaches the team.
    membership("role = 'manager'").await;
    assert_eq!(crm.titles(MANAGER, "").await, ["Alpha", "Bravo"]);
    membership("role = 'lead'").await;
    assert_eq!(crm.titles(MANAGER, "").await, ["Alpha", "Bravo"]);
    let create = json!({ "title": "Echo", "owner": crm.members[0], "team": crm.team_a });
    let (status, created, _) = crm
        .keyed(
            "POST",
            "/api/v1/opportunities",
            MANAGER,
            create.clone(),
            None,
            "create-1",
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // Inactive, the same membership grants nothing on any path.
    membership("active = false").await;
    assert!(crm.titles(MANAGER, "").await.is_empty());
    let (status, _, _) = crm.send("GET", &bravo_url, MANAGER, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body, replayed) = crm
        .keyed(
            "POST",
            "/api/v1/opportunities",
            MANAGER,
            create,
            None,
            "create-1",
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!replayed && body.get("id").is_none(), "{body}");
    let (status, refused, _) = crm
        .send(
            "POST",
            "/api/v1/opportunities",
            MANAGER,
            Some(json!({ "title": "Foxtrot", "owner": crm.members[0], "team": crm.team_a })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["code"], "out_of_scope");
    // Salespeople's `own` scope has no conditions and is unaffected.
    assert_eq!(crm.titles(S1, "").await, ["Alpha", "Echo"]);
    crm.db.drop().await;
}

#[tokio::test]
async fn a_scope_condition_is_rechecked_in_the_transaction() {
    let crm = Crm::start_with(&conditioned()).await;
    let pool = crm.db.pool().await;
    sqlx::query("UPDATE team_members SET role = 'manager' WHERE actor = $1")
        .bind(uuid(MANAGER))
        .execute(&pool)
        .await
        .unwrap();
    let bravo = crm.opportunities[1].clone();
    let (_, etag) = crm.row(&bravo).await;

    // Another transaction deactivates the manager's team membership and
    // hasn't committed. The row still exists, but the update holds it, so
    // the action's FOR SHARE lock waits, then reads the committed row,
    // which no longer meets `active = true`: Bravo is missing.
    let mut deactivating = pool.begin().await.unwrap();
    let updated = sqlx::query("UPDATE team_members SET active = false WHERE actor = $1")
        .bind(uuid(MANAGER))
        .execute(&mut *deactivating)
        .await
        .unwrap();
    assert_eq!(updated.rows_affected(), 1);
    let request = crm.request(
        "POST",
        &format!("/api/v1/opportunities/{bravo}/win"),
        MANAGER,
        None,
        Some(&etag),
    );
    let app = crm.app.clone();
    let action = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    wait_for_lock(&pool, &action, "the team membership update").await;
    deactivating.commit().await.unwrap();
    let res = action.await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(crm.row(&bravo).await.0["stage"], "open");
    crm.db.drop().await;
}

#[tokio::test]
async fn a_rows_history_is_as_hidden_as_the_row() {
    let text = CRM
        .replace(
            "table = \"opportunities\"\n",
            "table = \"opportunities\"\nhistory = true\n",
        )
        .replace("idempotent = true\n", "idempotent = true\nhistory = true\n");
    assert_eq!(text.matches("history = true").count(), 2);
    let server = Crm::start_with(&text).await;
    let [alpha, bravo, charlie] = server.opportunities.clone();
    let crm = &server;
    let history = |id: &str, actor: &'static str| {
        let uri = format!("/api/v1/opportunities/{id}/history");
        async move { crm.send("GET", &uri, actor, None, None).await }
    };
    // S1 reads the log of their own row, which the director created.
    let (status, page, _) = history(&alpha, S1).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        (&items[0]["kind"], &items[0]["actor"]),
        (&json!("create"), &json!(DIRECTOR))
    );
    // Another salesperson's row, and another team's for the manager, are
    // missing, log included; a role without the grant is refused.
    for (id, actor) in [(&bravo, S1), (&charlie, MANAGER)] {
        let (status, body, _) = history(id, actor).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(body.get("items").is_none());
    }
    assert_eq!(history(&bravo, MANAGER).await.0, StatusCode::OK);
    assert_eq!(history(&alpha, ADMIN).await.0, StatusCode::FORBIDDEN);
    // S1's own change is logged with S1 as the actor.
    let (_, etag) = crm.row(&alpha).await;
    let (status, _, _) = crm
        .send(
            "PATCH",
            &format!("/api/v1/opportunities/{alpha}"),
            S1,
            Some(json!({ "title": "Alpha 2" })),
            Some(&etag),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, page, _) = history(&alpha, S1).await;
    assert_eq!(page["items"][0]["actor"], S1);
    assert_eq!(
        page["items"][0]["changes"],
        json!([{ "change": { "field": "title", "old": "Alpha", "new": "Alpha 2" } }])
    );
    // A deleted row is 404, log included, but its log is kept.
    let (_, etag) = crm.row(&charlie).await;
    let uri = format!("/api/v1/opportunities/{charlie}");
    let (status, _, _) = crm.send("DELETE", &uri, DIRECTOR, None, Some(&etag)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(history(&charlie, DIRECTOR).await.0, StatusCode::NOT_FOUND);
    let pool = server.db.pool().await;
    let (kind, version, title): (String, i64, Value) = sqlx::query_as(
        "SELECT kind, version, changes::jsonb->'title' FROM opportunities_history \
         WHERE row_id = $1::uuid ORDER BY version DESC, id DESC LIMIT 1",
    )
    .bind(&charlie)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (kind.as_str(), version, title),
        ("delete", 1, json!({ "old": "Charlie", "new": null }))
    );
    server.db.drop().await;
}
