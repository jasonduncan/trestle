//! ADR-023: `trestle.toml` fields of type decimal, date, instant and
//! duration, end to end: columns, contracts, and a CRUD server whose JSON
//! keeps every value in its contract form. ADR-035: enum labels are
//! presentation only. ADR-043: `default = "now"` on an instant is the
//! create's transaction time. Needs PostgreSQL.

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
  { id = "total",     type = "money", precision = 12, scale = 2, currencies = ["EUR", "USD"], optional = true },
  { id = "status",    type = "enum", values = ["new", "contract"], labels = { contract = "Agreement" }, default = "new" },
  { id = "opened_at", type = "instant", default = "now" },
  { id = "stamped_at", type = "instant", default = "now", writable = "actions" },
]

[[endpoints]]
model = "orders.order"
crud = ["list", "get", "create", "update"]
read = "orders.read"
write = "orders.write"
filter = ["due", "status"]

[[endpoints]]
id = "orders.stamp"
model = "orders.order"
path = "POST /api/v1/orders/{id}/stamp"
permission = "orders.write"
set = { stamped_at = "now" }

[[endpoints]]
id = "orders.copy"
model = "orders.order"
path = "POST /api/v1/orders/{id}/copy"
permission = "orders.write"
handler = "orders::copy"
"#;

/// The intent with its one handler.
fn orders(text: &str) -> trestle::Project {
    trestle::project()
        .intent_str("trestle.toml", text)
        .handler("orders::copy", copy)
}

/// A new order through `Ctx::create`, titled like the given one.
async fn copy(ctx: Ctx, input: Input) -> Result<Row> {
    let original = ctx.get("orders.order", input.id()?).await?;
    let title: String = original.get("title")?;
    ctx.create(
        "orders.order",
        json!({ "title": format!("{title} (copy)"), "amount": "1" }),
    )
    .await
}

const TOKEN: &str = "member-token";
const ACTOR: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

struct Server {
    app: Router,
    db: TestDatabase,
    pool: sqlx::PgPool,
    tenant: uuid::Uuid,
}

impl Server {
    async fn start() -> Self {
        let registry = orders(ORDERS).assemble().unwrap();
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
        let app = trestle::router_with(
            &registry,
            State::new(pool.clone(), identity),
            Default::default(),
        );
        Server {
            app,
            db,
            pool,
            tenant,
        }
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
    let registry = orders(ORDERS).assemble().unwrap();
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
    // `default = "now"` is the column's database default (ADR-043).
    let default = |id: &str| {
        model
            .field(&format!("orders.order.{id}"))
            .unwrap()
            .default
            .clone()
    };
    use trestle::historical::schema::DbDefault;
    assert_eq!(default("opened_at"), Some(DbDefault::Now));
    assert_eq!(default("stamped_at"), Some(DbDefault::Now));
    // Money is two columns (ADR-027).
    assert_eq!(ty("total_amount").as_deref(), Some("numeric(12,2)"));
    assert_eq!(ty("total_currency").as_deref(), Some("text"));
    assert!(model.field("orders.order.total").is_none());
    let create = registry.inspect_operation("orders.order.create").unwrap();
    let schema = &create["input"]["schema"]["properties"];
    assert_eq!(
        schema["amount"]["pattern"],
        "^-?(0|[1-9][0-9]{0,7})(\\.[0-9]{1,2})?$"
    );
    assert_eq!(schema["placed_at"]["format"], "date-time");
    assert_eq!(schema["total"]["type"], json!(["object", "null"]));
    assert_eq!(
        schema["total"]["properties"]["currency"]["enum"],
        json!(["EUR", "USD"])
    );
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
    // "now" is an instant's default only; for text it is just text.
    for ty in ["date", "int", "duration"] {
        let p = problems(&format!(
            r#"{{ id = "x", type = "{ty}", default = "now" }}"#
        ));
        assert!(
            has(
                p.clone(),
                &format!(
                    r#"default = "now" is the time of the create, for instant fields; "x" is a {ty} field"#
                )
            ),
            "{p:?}"
        );
    }
    assert!(has(
        problems(r#"{ id = "x", type = "instant", default = "2026-01-01T00:00:00Z" }"#),
        r#"an instant's default is "now""#
    ));
    assert!(problems(r#"{ id = "x", type = "text", default = "now" }"#).is_empty());
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
    // `default = "now"`: an omitted instant, managed or not, is the
    // create's transaction time; the create's own `created_at` is the
    // same `now()` (ADR-043).
    let same: bool = sqlx::query_scalar(
        "SELECT $1::timestamptz = created_at AND $2::timestamptz = created_at FROM orders WHERE id = $3::uuid",
    )
    .bind(created["opened_at"].as_str().unwrap())
    .bind(created["stamped_at"].as_str().unwrap())
    .bind(created["id"].as_str().unwrap())
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert!(same, "{created}");
    let recent: bool =
        sqlx::query_scalar("SELECT $1::timestamptz BETWEEN now() - interval '1 minute' AND now()")
            .bind(created["opened_at"].as_str().unwrap())
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert!(recent, "{created}");
    // A writable field still takes a value; a managed one is refused on
    // create like any managed field (ADR-037).
    let (status, given, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "Given", "amount": "1", "opened_at": "2026-01-02T03:04:05+01:00" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{given}");
    assert_eq!(given["opened_at"], "2026-01-02T02:04:05Z");
    let (status, refused, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "x", "amount": "1", "stamped_at": "2026-01-02T03:04:05Z" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["errors"][0]["code"], "unknown_field");
    // Ctx::create from a handler takes the same default.
    let (status, copied, _) = server
        .call(
            "POST",
            &format!("/api/v1/orders/{}/copy", created["id"].as_str().unwrap()),
            Some(json!({})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{copied}");
    let (created_at, opened_at, stamped_at) = (
        copied["created_at"].as_str().unwrap(),
        copied["opened_at"].as_str().unwrap(),
        copied["stamped_at"].as_str().unwrap(),
    );
    let same: bool = sqlx::query_scalar(
        "SELECT $1::timestamptz = $2::timestamptz AND $1::timestamptz = $3::timestamptz",
    )
    .bind(created_at)
    .bind(opened_at)
    .bind(stamped_at)
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert!(same, "{copied}");

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

    // Money round-trips as one object over two columns.
    let (status, priced, etag) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "Priced", "amount": "1", "total": { "amount": "10.50", "currency": "EUR" } })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{priced}");
    assert_eq!(
        priced["total"],
        json!({ "amount": "10.50", "currency": "EUR" })
    );
    let priced_id = priced["id"].as_str().unwrap();
    let (status, cleared, _) = server
        .call(
            "PATCH",
            &format!("/api/v1/orders/{priced_id}"),
            Some(json!({ "total": null })),
            etag.as_deref(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["total"], Value::Null);
    let (status, refused, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "x", "amount": "1", "total": { "amount": "1", "currency": "GBP" } })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["errors"][0]["pointer"], "/total/currency");
    assert_eq!(refused["errors"][0]["code"], "choice");

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

    // A labelled enum stores, returns and filters by its value; the label
    // is not a value (ADR-035).
    let (status, signed, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "Signed", "amount": "1", "status": "contract" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{signed}");
    assert_eq!(signed["status"], "contract");
    let stored: String = sqlx::query_scalar("SELECT status FROM orders WHERE id = $1")
        .bind(uuid::Uuid::parse_str(signed["id"].as_str().unwrap()).unwrap())
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored, "contract");
    let (_, list, _) = server
        .call("GET", "/api/v1/orders?status=contract", None, None)
        .await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1), "{list}");
    let (status, refused, _) = server
        .call(
            "POST",
            "/api/v1/orders",
            Some(json!({ "title": "x", "amount": "1", "status": "Agreement" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["errors"][0]["pointer"], "/status");
    assert_eq!(refused["errors"][0]["code"], "choice");
    server.db.drop().await;
}

/// Labels reach every contract the browser reads, and never the schema:
/// changing, adding or removing them plans no migration; changing the
/// values still does (ADR-035).
#[test]
fn enum_labels_are_presentation_only() {
    let registry = |text: &str| orders(text).assemble().unwrap();
    let labelled = registry(ORDERS);
    for (op, contract) in [
        ("orders.order.create", "input"),
        ("orders.order.list", "input"),
        ("orders.order.get", "output"),
    ] {
        let doc = labelled.inspect_operation(op).unwrap();
        let status = &doc[contract]["schema"]["properties"]["status"];
        assert_eq!(status["enum"], json!(["new", "contract"]), "{op}");
        assert_eq!(
            status["x-trestle-enum-labels"],
            json!({ "contract": "Agreement" }),
            "{op}"
        );
    }

    let line = r#"labels = { contract = "Agreement" }, "#;
    for changed in [
        ORDERS.replace(line, ""),
        ORDERS.replace(
            line,
            r#"labels = { contract = "Signed", new = "New lead" }, "#,
        ),
    ] {
        let other = registry(&changed);
        assert_eq!(other.desired_schema(), labelled.desired_schema());
        let plan = diff(
            labelled.desired_schema(),
            other.desired_schema(),
            &PlanOptions::default(),
        );
        assert!(plan.is_empty(), "{plan:?}");
    }
    let unlabelled = registry(&ORDERS.replace(line, ""));
    let status = &unlabelled.inspect_operation("orders.order.create").unwrap()["input"]["schema"]["properties"]
        ["status"];
    assert_eq!(
        status,
        &json!({ "type": "string", "enum": ["new", "contract"] }),
        "no labels, no keyword"
    );

    let more =
        registry(&ORDERS.replace(r#"["new", "contract"]"#, r#"["new", "contract", "lost"]"#));
    let plan = diff(
        labelled.desired_schema(),
        more.desired_schema(),
        &PlanOptions::default(),
    );
    assert!(!plan.is_empty(), "values are schema");
}

#[test]
fn enum_labels_are_checked() {
    let problems = |field: &str| {
        let text = ORDERS.replace(
            r#"  { id = "title",     type = "text", max = 100 },"#,
            &format!("  {field},\n  {{ id = \"title\", type = \"text\", max = 100 }},"),
        );
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems)
            .unwrap_or_default()
    };
    assert_eq!(
        problems(r#"{ id = "x", type = "enum", values = ["a"], labels = { b = "B" } }"#),
        [r#"trestle.toml:15: labels.b: "b" is not one of the field's values"#]
    );
    assert_eq!(
        problems(r#"{ id = "x", type = "enum", values = ["a"], labels = { a = "" } }"#),
        ["trestle.toml:15: labels.a must be 1 to 200 characters"]
    );
    assert_eq!(
        problems(r#"{ id = "x", type = "text", labels = { a = "A" } }"#),
        ["trestle.toml:15: labels = {...} is for enum fields"]
    );
}

#[test]
fn money_fields_are_checked() {
    let problems = |field: &str, endpoint: &str| {
        let text = ORDERS
            .replace(
                r#"  { id = "title",     type = "text", max = 100 },"#,
                &format!("  {field},\n  {{ id = \"title\", type = \"text\", max = 100 }},"),
            )
            .replace(r#"filter = ["due", "status"]"#, endpoint);
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems)
            .unwrap_or_default()
    };
    let has = |p: Vec<String>, text: &str| p.iter().any(|m| m.contains(text));
    let money = r#"{ id = "fee", type = "money", precision = 6, scale = 2 }"#;
    assert!(problems(money, r#"filter = ["due"]"#).is_empty());
    assert!(has(
        problems(money, r#"filter = ["fee"]"#),
        "cannot be a filter"
    ));
    assert!(has(problems(money, r#"order = "fee""#), "is money"));
    assert!(has(
        problems(
            r#"{ id = "fee", type = "money", precision = 6, currencies = ["usd"] }"#,
            r#"filter = ["due"]"#
        ),
        "three-letter codes"
    ));
    assert!(has(
        problems(
            r#"{ id = "fee", type = "text", currencies = ["USD"] }"#,
            r#"filter = ["due"]"#
        ),
        "for money fields"
    ));
    assert!(has(
        problems(
            r#"{ id = "fee", type = "money", precision = 6, default = "1" }"#,
            r#"filter = ["due"]"#
        ),
        "take no default"
    ));
    assert!(has(
        problems(
            r#"{ id = "fee", type = "money", precision = 6 },
  { id = "fee_amount", type = "int" }"#,
            r#"filter = ["due"]"#
        ),
        "collides with the storage of money field"
    ));
}
