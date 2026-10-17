//! ADR-013: an operation invoked without HTTP (`Registry::invoke` with an
//! `Invocation`) resolves its context through the same lookup and reaches the
//! same decision as a managed route, for every kind of caller.

mod support;

use axum::body::Body;
use http::{Method, Request, StatusCode, header};
use pilot::dev::{ALICE, BOB, CAROL, NORTHSIDE};
use serde_json::{Value, json};
use support::{TestEnv, get, send};
use trestle_core::identity::ActorId;
use trestle_core::invocation::Invocation;
use uuid::Uuid;

/// `POST /api/v1/lessons` with the given credentials and tenant header.
fn create(token: Option<&str>, tenant: Option<&str>, body: &Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/lessons")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(tenant) = tenant {
        builder = builder.header("trestle-tenant", tenant);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn as_actor(actor: Uuid, tenant: Option<Uuid>) -> Invocation {
    let invocation = Invocation::for_actor(ActorId(actor));
    match tenant {
        Some(t) => invocation.with_tenant(t),
        None => invocation,
    }
}

/// One caller, as HTTP sees it (credentials, tenant header) and as a direct
/// invocation, and the decision both must reach.
struct Case<'a> {
    name: &'a str,
    token: Option<&'a str>,
    tenant: Option<&'a str>,
    invocation: Invocation,
    expected: Result<(), (StatusCode, &'a str)>,
}

#[tokio::test]
async fn http_and_direct_invocation_reach_the_same_decision() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());
    let northside = NORTHSIDE.to_string();
    // Titles are unique per school, so each path creates its own lesson.
    let over_http = json!({ "title": "Fractions", "grade_level": 4 });
    let direct_body = json!({ "title": "Fractions, invoked", "grade_level": 4 });

    let cases = [
        Case {
            name: "teacher",
            token: Some("alice-dev-token"),
            tenant: Some(&northside),
            invocation: as_actor(ALICE, Some(NORTHSIDE)),
            expected: Ok(()),
        },
        Case {
            name: "viewer",
            token: Some("carol-dev-token"),
            tenant: Some(&northside),
            invocation: as_actor(CAROL, Some(NORTHSIDE)),
            expected: Err((StatusCode::FORBIDDEN, "permission_denied")),
        },
        Case {
            name: "other school's teacher",
            token: Some("bob-dev-token"),
            tenant: Some(&northside),
            invocation: as_actor(BOB, Some(NORTHSIDE)),
            expected: Err((StatusCode::FORBIDDEN, "tenant_not_permitted")),
        },
        Case {
            name: "no tenant",
            token: Some("alice-dev-token"),
            tenant: None,
            invocation: as_actor(ALICE, None),
            expected: Err((StatusCode::BAD_REQUEST, "tenant_required")),
        },
        Case {
            name: "no caller",
            token: None,
            tenant: Some(&northside),
            invocation: Invocation::anonymous().with_tenant(NORTHSIDE),
            expected: Err((StatusCode::UNAUTHORIZED, "credentials_missing")),
        },
    ];

    for Case {
        name: case,
        token,
        tenant,
        invocation,
        expected,
    } in cases
    {
        let http = send(&app, create(token, tenant, &over_http)).await;
        let direct = registry
            .invoke(&env.state, "lessons.create", &invocation, &direct_body)
            .await;
        match expected {
            Ok(()) => {
                assert_eq!(
                    http.status,
                    StatusCode::CREATED,
                    "{case} over HTTP: {}",
                    http.body
                );
                let direct = direct.unwrap_or_else(|p| panic!("{case} directly: {}", p.code()));
                assert_eq!(direct["title"], "Fractions, invoked", "{case}");
                assert_eq!(direct["status"], http.body["status"], "{case}");
            }
            Err((status, code)) => {
                assert_eq!(
                    (http.status, http.body["code"].as_str()),
                    (status, Some(code)),
                    "{case} over HTTP"
                );
                let problem = direct.expect_err(case);
                assert_eq!(
                    (problem.status(), problem.code()),
                    (status, code),
                    "{case} directly"
                );
            }
        }
    }
    assert_eq!(
        env.lesson_count().await,
        2,
        "only the teacher's two creates were written"
    );
    let owners: Vec<Uuid> = sqlx::query_scalar("SELECT created_by FROM lessons")
        .fetch_all(&env.pool)
        .await
        .unwrap();
    assert_eq!(
        owners,
        vec![ALICE, ALICE],
        "ownership comes from the invocation's actor"
    );
    env.finish().await;
}

#[tokio::test]
async fn direct_reads_are_tenant_scoped_like_http() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());
    let created = registry
        .invoke(
            &env.state,
            "lessons.create",
            &as_actor(ALICE, Some(NORTHSIDE)),
            &json!({ "title": "Decimals", "grade_level": 5 }),
        )
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let over_http = send(
        &app,
        get(&format!("/api/v1/lessons/{id}"), "alice", NORTHSIDE),
    )
    .await;
    assert_eq!(over_http.status, StatusCode::OK, "{}", over_http.body);
    assert_eq!(over_http.body["title"], "Decimals");

    // Bob teaches at Southside: the lesson does not exist for him either way.
    let bob = registry
        .invoke(
            &env.state,
            "lessons.get",
            &as_actor(BOB, Some(pilot::dev::SOUTHSIDE)),
            &json!({ "id": id }),
        )
        .await
        .expect_err("another tenant's lesson");
    let bob_http = send(
        &app,
        get(
            &format!("/api/v1/lessons/{id}"),
            "bob",
            pilot::dev::SOUTHSIDE,
        ),
    )
    .await;
    assert_eq!(bob.status(), StatusCode::NOT_FOUND);
    assert_eq!(bob_http.status, StatusCode::NOT_FOUND);
    env.finish().await;
}

#[tokio::test]
async fn http_reads_only_what_the_context_declares() {
    let env = TestEnv::new().await;
    let app = pilot::router(&pilot::registry().unwrap(), env.state.clone());
    let with_tenant = |uri: &str, tenant: &str| {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer alice-dev-token")
            .header("trestle-tenant", tenant)
            .body(Body::empty())
            .unwrap()
    };
    // `schools.mine` declares no tenant header: a malformed one is never read.
    let mine = send(&app, with_tenant("/api/v1/me/schools", "not-a-uuid")).await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.body);
    // `lessons.list` declares it: the same value is refused before any lookup.
    let lessons = send(&app, with_tenant("/api/v1/lessons", "not-a-uuid")).await;
    assert_eq!(lessons.status, StatusCode::BAD_REQUEST);
    assert_eq!(lessons.body["code"], "tenant_invalid");
    env.finish().await;
}

#[tokio::test]
async fn unknown_operations_are_not_found() {
    let env = TestEnv::new().await;
    let problem = pilot::registry()
        .unwrap()
        .invoke(
            &env.state,
            "lessons.nope",
            &as_actor(ALICE, Some(NORTHSIDE)),
            &json!({}),
        )
        .await
        .expect_err("not registered");
    assert_eq!(problem.status(), StatusCode::NOT_FOUND);
    env.finish().await;
}
