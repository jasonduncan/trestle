//! ADR-023: `trestle.toml` fields of type decimal, date, instant and
//! duration, end to end: columns, contracts, and a CRUD server whose JSON
//! keeps every value in its contract form. Needs PostgreSQL.

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

const ORDERS: &str = r#"
[project]
name = "orders"
trestle = "0.1"

[tenancy]
model = "workspace"

[roles]
member = ["orders.read", "orders.write"]

[[models]]
id = "orders.order"
fields = [
  { id = "title",     type = "text", max = 100 },
  { id = "amount",    type = "decimal", precision = 10, scale = 2 },
  { id = "rate",      type = "decimal", precision = 3, scale = 3, optional = true },
  { id = "due",       type = "date", optional = true },
  { id = "placed_at", type = "instant", optional = true },
  { id = "length",    type = "duration", default = "PT30M" },
]

[[endpoints]]
model = "orders.order"
crud = ["list", "get", "create", "update"]
read = "orders.read"
write = "orders.write"
filter = ["due"]
"#;

const TOKEN: &str = "member-token";
const ACTOR: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

struct Server {
    app: Router,
    db: TestDatabase,
    tenant: uuid::Uuid,
}

impl Server {
    async fn start() -> Self {
        let registry = trestle::project()
            .intent_str("trestle.toml", ORDERS)
            .assemble()
            .unwrap();
        let db = TestDatabase::create().await;
        // A session zone half an hour off UTC (sqlx would start in UTC):
        // instants must still read back in UTC.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::Executor::execute(conn, "SET TIME ZONE 'Asia/Kolkata'").await?;
                    Ok(())
                })
            })
            .connect_with(db.options().clone())
            .await
            .unwrap();
        let plan = diff(
            &SchemaState::default(),
            registry.desired_schema(),
            &PlanOptions::default(),
        );
        // Generated as `makemigrations` would: referenced apps first.
        let none = trestle::historical::MigrationSet::new(Vec::new()).unwrap();
        for artifact in trestle::historical::artifact::generate(&none, &plan, "initial").unwrap() {
            sqlx::raw_sql(&artifact.sql).execute(&pool).await.unwrap();
        }
        let tenant = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'North')")
            .bind(tenant)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspace_memberships (workspace_id, actor_id, role) VALUES ($1, $2, 'member')",
        )
        .bind(tenant)
        .bind(uuid::Uuid::parse_str(ACTOR).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        let identity = Arc::new(DevTokenIdentity::parse(&format!("{TOKEN}={ACTOR}")).unwrap());
        let app = trestle::router_with(&registry, State::new(pool, identity), Default::default());
        Server { app, db, tenant }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        if_match: Option<&str>,
    ) -> (StatusCode, Value, Option<String>) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("Trestle-Tenant", self.tenant.to_string());
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
}

#[test]
fn the_intent_declares_columns_and_contracts_of_each_type() {
    let registry = trestle::project()
        .intent_str("trestle.toml", ORDERS)
        .assemble()
        .unwrap();
    let model = registry.desired_schema().model("orders.order").unwrap();
    let ty = |id: &str| {
        model
            .field(&format!("orders.order.{id}"))
            .map(|f| f.ty.sql().into_owned())
    };
    assert_eq!(ty("amount").as_deref(), Some("numeric(10,2)"));
    assert_eq!(ty("due").as_deref(), Some("date"));
    assert_eq!(ty("placed_at").as_deref(), Some("timestamptz"));
    assert_eq!(ty("length").as_deref(), Some("interval"));
    let create = registry.inspect_operation("orders.order.create").unwrap();
    let schema = &create["input"]["schema"]["properties"];
    assert_eq!(
        schema["amount"]["pattern"],
        "^-?(0|[1-9][0-9]{0,7})(\\.[0-9]{1,2})?$"
    );
    assert_eq!(schema["placed_at"]["format"], "date-time");
}

#[test]
fn decimal_settings_and_defaults_are_checked() {
    let problems = |fields: &str| {
        let text = ORDERS.replace(
            r#"  { id = "title",     type = "text", max = 100 },"#,
            &format!("  {fields},\n  {{ id = \"title\", type = \"text\", max = 100 }},"),
        );
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems)
            .unwrap_or_default()
    };
    let has = |p: Vec<String>, text: &str| p.iter().any(|m| m.contains(text));
    assert!(has(
        problems(r#"{ id = "x", type = "decimal" }"#),
        "needs precision"
    ));
    assert!(has(
        problems(r#"{ id = "x", type = "decimal", precision = 2, scale = 3 }"#),
        "needs precision"
    ));
    assert!(has(
        problems(r#"{ id = "x", type = "text", precision = 2 }"#),
        "for decimal fields"
    ));
    assert!(has(
        problems(r#"{ id = "x", type = "date", default = "2026-02-30" }"#),
        "does not fit"
    ));
    assert!(has(
        problems(r#"{ id = "x", type = "duration", default = "P1Y" }"#),
        "does not fit"
    ));
    assert!(
        problems(r#"{ id = "x", type = "decimal", precision = 5, default = "123" }"#).is_empty()
    );
}

#[tokio::test]
async fn crud_keeps_every_value_in_its_contract_form() {
    let server = Server::start().await;
    let (status, created, etag) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({
                "title": "First",
                "amount": "1234.50",
                "rate": "0.125",
                "due": "2026-12-31",
                "placed_at": "2026-10-17T09:30:00.250+02:00",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["amount"], "1234.50", "a string, not a number");
    assert_eq!(created["rate"], "0.125");
    assert_eq!(created["due"], "2026-12-31");
    assert_eq!(created["placed_at"], "2026-10-17T07:30:00.25Z", "in UTC");
    assert_eq!(
        created["length"], "PT30M",
        "the column default, as ISO 8601"
    );

    let id = created["id"].as_str().unwrap();
    let (status, updated, _) = server
        .call(
            "PATCH",
            &format!("/api/v1/orders/{id}"),
            Some(json!({ "length": "P1DT2H", "placed_at": null })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["length"], "P1DT2H");
    assert_eq!(updated["placed_at"], Value::Null);

    let (_, list, _) = server
        .call("GET", "/api/v1/orders?due=2026-12-31", None, None)
        .await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1), "{list}");
    let (_, none, _) = server
        .call("GET", "/api/v1/orders?due=2027-01-01", None, None)
        .await;
    assert_eq!(none["items"].as_array().map(Vec::len), Some(0), "{none}");

    let (status, refused, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "x", "amount": 12.5 })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    let (status, refused, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "x", "amount": "1.234" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(refused["errors"][0]["code"], "decimal_digits");
    server.db.drop().await;
}
