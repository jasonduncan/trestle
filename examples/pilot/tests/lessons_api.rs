//! M0 acceptance (design doc §18): an authorized user creates and reads a
//! lesson; another tenant cannot access it; malformed and unknown input fails
//! without writes; one rollback and one database constraint failure.
//!
//! Each test gets a fresh database, replays the committed history into it,
//! seeds the development fixtures and drives the real router in-process.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use pilot::AppState;
use pilot::dev::{ALICE, DEV_TOKENS, NORTHSIDE, SOUTHSIDE};
use pilot::lessons::contracts::CreateLessonInput;
use pilot::lessons::operations::CreateLesson;
use pilot::schools::{Role, SchoolId, TenantContext};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use trestle_core::identity::{ActorId, DevTokenIdentity};
use trestle_historical::testing::TestDatabase;
use uuid::Uuid;

struct Harness {
    db: TestDatabase,
    pool: PgPool,
    app: Router,
}

impl Harness {
    async fn new() -> Self {
        let db = TestDatabase::create().await;
        trestle_historical::migrate(db.options(), &pilot_migrations::history().unwrap())
            .await
            .unwrap();
        let pool = db.pool().await;
        pilot::dev::seed(&pool).await.unwrap();
        let identity = Arc::new(DevTokenIdentity::parse(DEV_TOKENS).unwrap());
        let registry = pilot::registry().expect("registry assembles");
        let app = pilot::router(&registry, AppState::new(pool.clone(), identity));
        Self { db, pool, app }
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("every response body is JSON")
        };
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn lesson_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM lessons")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Reply {
    fn content_type(&self) -> &str {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    /// Asserts a problem+json response and returns its code.
    fn problem(&self, status: StatusCode) -> &str {
        assert_eq!(self.status, status, "{:#}", self.body);
        assert_eq!(self.content_type(), "application/problem+json");
        assert_eq!(self.body["status"], status.as_u16());
        assert!(
            self.body["request_id"]
                .as_str()
                .unwrap()
                .starts_with("req-")
        );
        self.body["code"].as_str().unwrap()
    }

    fn field_errors(&self) -> Vec<(String, String)> {
        self.body["errors"]
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
}

/// Request builder: `call(POST, uri).token("alice").tenant(NORTHSIDE).json(..)`.
struct Call {
    builder: http::request::Builder,
}

fn call(method: Method, uri: &str) -> Call {
    Call {
        builder: Request::builder().method(method).uri(uri),
    }
}

impl Call {
    fn token(mut self, who: &str) -> Self {
        self.builder = self
            .builder
            .header(header::AUTHORIZATION, format!("Bearer {who}-dev-token"));
        self
    }

    fn tenant(self, school: Uuid) -> Self {
        self.header("trestle-tenant", &school.to_string())
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.builder = self.builder.header(name, value);
        self
    }

    fn json(self, body: Value) -> Request<Body> {
        self.raw("application/json", body.to_string())
    }

    fn raw(self, content_type: &str, body: impl Into<Body>) -> Request<Body> {
        self.builder
            .header(header::CONTENT_TYPE, content_type)
            .body(body.into())
            .unwrap()
    }

    fn empty(self) -> Request<Body> {
        self.builder.body(Body::empty()).unwrap()
    }
}

fn create_as(who: &str, school: Uuid, body: Value) -> Request<Body> {
    call(Method::POST, "/api/v1/lessons")
        .token(who)
        .tenant(school)
        .json(body)
}

fn fractions() -> Value {
    json!({ "title": "Fractions", "grade_level": 4 })
}

#[tokio::test]
async fn teacher_creates_and_reads_lesson_with_audit() {
    let h = Harness::new().await;

    let created = h.send(create_as("alice", NORTHSIDE, fractions())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:#}", created.body);
    assert_eq!(created.content_type(), "application/json");
    let id = created.body["id"].as_str().unwrap().to_owned();
    assert_eq!(
        created.headers[header::LOCATION],
        format!("/api/v1/lessons/{id}").as_str()
    );
    assert_eq!(
        created.body,
        json!({
            "id": id,
            "title": "Fractions",
            "grade_level": 4,
            "status": "draft",
            "version": "1",
            "subtitle": null,
            "unit_id": null,
            "duration_minutes": null,
        }),
        "the view exposes exactly lessons.view fields"
    );

    let read = h
        .send(
            call(Method::GET, &format!("/api/v1/lessons/{id}"))
                .token("alice")
                .tenant(NORTHSIDE)
                .empty(),
        )
        .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body, created.body);

    // Viewers may read.
    let viewer = h
        .send(
            call(Method::GET, &format!("/api/v1/lessons/{id}"))
                .token("carol")
                .tenant(NORTHSIDE)
                .empty(),
        )
        .await;
    assert_eq!(viewer.status, StatusCode::OK);

    let (actor, tenant, operation, request_id): (Uuid, Uuid, String, String) = sqlx::query_as(
        "SELECT actor_id, tenant_id, operation, request_id FROM audit_events WHERE target_id = $1",
    )
    .bind(&id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(
        (actor, tenant, operation.as_str()),
        (ALICE, NORTHSIDE, "lessons.create")
    );
    assert_eq!(
        request_id,
        created.headers["x-request-id"].to_str().unwrap()
    );
    h.finish().await;
}

#[tokio::test]
async fn other_tenant_cannot_access_lesson() {
    let h = Harness::new().await;
    let created = h.send(create_as("alice", NORTHSIDE, fractions())).await;
    let uri = format!("/api/v1/lessons/{}", created.body["id"].as_str().unwrap());

    // Bob selecting his own school: the lesson does not exist there.
    let reply = h
        .send(
            call(Method::GET, &uri)
                .token("bob")
                .tenant(SOUTHSIDE)
                .empty(),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::NOT_FOUND), "not_found");

    // Bob claiming Alice's school: the header is not authority.
    let reply = h
        .send(
            call(Method::GET, &uri)
                .token("bob")
                .tenant(NORTHSIDE)
                .empty(),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::FORBIDDEN), "tenant_not_permitted");

    // Nonexistent schools look exactly like schools without membership.
    let reply = h
        .send(
            call(Method::GET, &uri)
                .token("bob")
                .tenant(Uuid::now_v7())
                .empty(),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::FORBIDDEN), "tenant_not_permitted");

    // Bob cannot create in Alice's school either.
    let reply = h.send(create_as("bob", NORTHSIDE, fractions())).await;
    assert_eq!(reply.problem(StatusCode::FORBIDDEN), "tenant_not_permitted");
    assert_eq!(h.lesson_count().await, 1);
    h.finish().await;
}

#[tokio::test]
async fn authentication_and_tenant_selection_are_required() {
    let h = Harness::new().await;

    let reply = h
        .send(
            call(Method::POST, "/api/v1/lessons")
                .tenant(NORTHSIDE)
                .json(fractions()),
        )
        .await;
    assert_eq!(
        reply.problem(StatusCode::UNAUTHORIZED),
        "credentials_missing"
    );
    assert_eq!(reply.headers[header::WWW_AUTHENTICATE], "Bearer");

    let reply = h.send(create_as("mallory", NORTHSIDE, fractions())).await;
    assert_eq!(
        reply.problem(StatusCode::UNAUTHORIZED),
        "credentials_invalid"
    );

    let reply = h
        .send(
            call(Method::POST, "/api/v1/lessons")
                .token("alice")
                .json(fractions()),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::BAD_REQUEST), "tenant_required");

    let reply = h
        .send(
            call(Method::POST, "/api/v1/lessons")
                .token("alice")
                .header("trestle-tenant", "northside")
                .json(fractions()),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::BAD_REQUEST), "tenant_invalid");
    assert_eq!(h.lesson_count().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn viewer_cannot_create() {
    let h = Harness::new().await;
    let reply = h.send(create_as("carol", NORTHSIDE, fractions())).await;
    assert_eq!(reply.problem(StatusCode::FORBIDDEN), "permission_denied");
    assert_eq!(h.lesson_count().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn revoked_membership_is_enforced_on_the_next_write() {
    let h = Harness::new().await;
    sqlx::query("DELETE FROM school_memberships WHERE actor_id = $1")
        .bind(ALICE)
        .execute(&h.pool)
        .await
        .unwrap();
    let reply = h.send(create_as("alice", NORTHSIDE, fractions())).await;
    assert_eq!(reply.problem(StatusCode::FORBIDDEN), "tenant_not_permitted");
    assert_eq!(h.lesson_count().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn operation_rechecks_membership_inside_its_transaction() {
    // A context admitted while Alice was a teacher, used after revocation:
    // the "permission revoked while the form was open" case (§12.3).
    let h = Harness::new().await;
    let stale = TenantContext {
        actor: ActorId(ALICE),
        school: SchoolId(NORTHSIDE),
        role: Role::Teacher,
    };
    sqlx::query("UPDATE school_memberships SET role = 'viewer' WHERE actor_id = $1")
        .bind(ALICE)
        .execute(&h.pool)
        .await
        .unwrap();
    let input = CreateLessonInput {
        title: "Fractions".into(),
        grade_level: 4,
        subtitle: None,
        unit_id: None,
        duration_minutes: None,
    };
    let err = trestle_core::operation::execute(&h.pool, &CreateLesson, &stale, input)
        .await
        .unwrap_err();
    assert_eq!(
        (err.status(), err.code()),
        (StatusCode::FORBIDDEN, "permission_denied")
    );
    assert_eq!(h.lesson_count().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn malformed_and_misshapen_bodies_fail_without_writes() {
    let h = Harness::new().await;

    let reply = h
        .send(
            call(Method::POST, "/api/v1/lessons")
                .token("alice")
                .tenant(NORTHSIDE)
                .raw("application/json", "{\"title\": \"Fractions\","),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::BAD_REQUEST), "malformed_json");

    let reply = h
        .send(create_as("alice", NORTHSIDE, json!(["Fractions", 4])))
        .await;
    assert_eq!(reply.problem(StatusCode::BAD_REQUEST), "body_not_object");

    let reply = h
        .send(create_as(
            "alice",
            NORTHSIDE,
            json!({"title": "Fractions", "grade_level": "4"}),
        ))
        .await;
    assert_eq!(reply.problem(StatusCode::BAD_REQUEST), "invalid_body_shape");
    assert_eq!(
        reply.field_errors(),
        [("/grade_level".into(), "type".into())]
    );

    let reply = h
        .send(
            call(Method::POST, "/api/v1/lessons")
                .token("alice")
                .tenant(NORTHSIDE)
                .raw("text/plain", fractions().to_string()),
        )
        .await;
    assert_eq!(
        reply.problem(StatusCode::UNSUPPORTED_MEDIA_TYPE),
        "unsupported_media_type"
    );

    let huge = json!({ "title": "x".repeat(2 * 1024 * 1024), "grade_level": 4 });
    let reply = h.send(create_as("alice", NORTHSIDE, huge)).await;
    assert_eq!(
        reply.problem(StatusCode::PAYLOAD_TOO_LARGE),
        "payload_too_large"
    );

    assert_eq!(h.lesson_count().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn unknown_and_ownership_fields_are_rejected_without_writes() {
    let h = Harness::new().await;
    let reply = h
        .send(create_as(
            "alice",
            NORTHSIDE,
            json!({
                "title": "Fractions",
                "grade_level": 4,
                "school_id": SOUTHSIDE,
                "status": "published",
            }),
        ))
        .await;
    assert_eq!(
        reply.problem(StatusCode::UNPROCESSABLE_ENTITY),
        "validation_failed"
    );
    assert_eq!(
        reply.field_errors(),
        [
            ("/school_id".into(), "unknown_field".into()),
            ("/status".into(), "unknown_field".into()),
        ]
    );
    assert_eq!(h.lesson_count().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn contract_rules_count_characters_and_report_every_field() {
    let h = Harness::new().await;

    let reply = h
        .send(create_as(
            "alice",
            NORTHSIDE,
            json!({"title": "", "grade_level": 13}),
        ))
        .await;
    assert_eq!(
        reply.problem(StatusCode::UNPROCESSABLE_ENTITY),
        "validation_failed"
    );
    assert_eq!(
        reply.field_errors(),
        [
            ("/title".into(), "min_length".into()),
            ("/grade_level".into(), "maximum".into()),
        ]
    );

    // 200 astral characters are 800 UTF-8 bytes but within the limit, and
    // PostgreSQL's char_length agrees with the contract.
    let emoji_200 = "📐".repeat(200);
    let reply = h
        .send(create_as(
            "alice",
            NORTHSIDE,
            json!({"title": emoji_200, "grade_level": 1}),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:#}", reply.body);

    let reply = h
        .send(create_as(
            "alice",
            NORTHSIDE,
            json!({"title": "📐".repeat(201), "grade_level": 1}),
        ))
        .await;
    assert_eq!(
        reply.problem(StatusCode::UNPROCESSABLE_ENTITY),
        "validation_failed"
    );
    assert_eq!(reply.body["errors"][0]["params"]["max"], 200);
    assert_eq!(h.lesson_count().await, 1);
    h.finish().await;
}

#[tokio::test]
async fn database_constraint_failure_becomes_a_field_error() {
    let h = Harness::new().await;
    let first = h.send(create_as("alice", NORTHSIDE, fractions())).await;
    assert_eq!(first.status, StatusCode::CREATED);

    // Enforced by the lessons_school_title_key unique constraint.
    let duplicate = h.send(create_as("alice", NORTHSIDE, fractions())).await;
    assert_eq!(
        duplicate.problem(StatusCode::UNPROCESSABLE_ENTITY),
        "validation_failed"
    );
    assert_eq!(
        duplicate.field_errors(),
        [("/title".into(), "already_exists".into())]
    );

    // The same title in another school is a different lesson.
    let other = h.send(create_as("bob", SOUTHSIDE, fractions())).await;
    assert_eq!(other.status, StatusCode::CREATED);
    assert_eq!(h.lesson_count().await, 2);
    h.finish().await;
}

#[tokio::test]
async fn audit_failure_rolls_back_the_lesson() {
    let h = Harness::new().await;
    sqlx::raw_sql(
        "CREATE FUNCTION fail_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'injected audit failure'; END $$;
         CREATE TRIGGER fail_audit BEFORE INSERT ON audit_events
         FOR EACH ROW EXECUTE FUNCTION fail_audit();",
    )
    .execute(&h.pool)
    .await
    .unwrap();

    let reply = h.send(create_as("alice", NORTHSIDE, fractions())).await;
    assert_eq!(
        reply.problem(StatusCode::INTERNAL_SERVER_ERROR),
        "internal_error"
    );
    let text = reply.body.to_string();
    assert!(
        !text.contains("injected") && !text.contains("audit_events"),
        "{text}"
    );
    assert_eq!(
        h.lesson_count().await,
        0,
        "the lesson insert must roll back"
    );
    h.finish().await;
}

#[tokio::test]
async fn unknown_routes_ids_and_methods_are_problems() {
    let h = Harness::new().await;

    let reply = h.send(call(Method::GET, "/api/v1/nope").empty()).await;
    assert_eq!(reply.problem(StatusCode::NOT_FOUND), "not_found");

    let reply = h
        .send(
            call(Method::GET, "/api/v1/lessons/not-a-uuid")
                .token("alice")
                .tenant(NORTHSIDE)
                .empty(),
        )
        .await;
    assert_eq!(reply.problem(StatusCode::NOT_FOUND), "not_found");

    let reply = h
        .send(call(Method::DELETE, "/api/v1/lessons").empty())
        .await;
    assert_eq!(
        reply.problem(StatusCode::METHOD_NOT_ALLOWED),
        "method_not_allowed"
    );
    h.finish().await;
}

#[tokio::test]
async fn readiness_reports_schema_capabilities() {
    let h = Harness::new().await;
    let reply = h.send(call(Method::GET, "/readyz").empty()).await;
    assert_eq!(reply.status, StatusCode::OK);

    sqlx::query("DELETE FROM trestle_migrations WHERE app = 'lessons'")
        .execute(&h.pool)
        .await
        .unwrap();
    let reply = h.send(call(Method::GET, "/readyz").empty()).await;
    assert_eq!(
        reply.problem(StatusCode::SERVICE_UNAVAILABLE),
        "schema_not_ready"
    );
    h.finish().await;
}
