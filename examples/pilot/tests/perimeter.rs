//! The serving perimeter (design doc §12.3, §15.3): admission control,
//! per-credential rate limits, default response headers and metrics with
//! bounded labels.

mod support;

use http::{Method, StatusCode, header};
use http_body_util::BodyExt;
use pilot::dev::NORTHSIDE;
use serde_json::json;
use support::{TestEnv, get, post_json, send, send_raw};
use tower::ServiceExt;
use trestle_core::admission::{Admission, AdmissionConfig};
use trestle_core::http::Perimeter;

fn admission(max_in_flight: usize, rate_per_second: f64, burst: f64) -> Perimeter {
    Perimeter {
        admission: Some(Admission::new(AdmissionConfig {
            max_in_flight,
            rate_per_second,
            burst,
            client_address_header: None,
            ..Default::default()
        })),
    }
}

#[tokio::test]
async fn responses_are_private_unsniffable_and_referrer_free() {
    let env = TestEnv::new().await;
    let app = pilot::router(&pilot::registry().unwrap(), env.state.clone());
    for (request, status) in [
        (get("/api/v1/lessons", "alice", NORTHSIDE), StatusCode::OK),
        (
            get("/api/v1/nowhere", "alice", NORTHSIDE),
            StatusCode::NOT_FOUND,
        ),
        (
            get("/api/v1/lessons", "nobody", NORTHSIDE),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        let reply = send(&app, request).await;
        assert_eq!(reply.status, status);
        assert_eq!(reply.headers[header::CACHE_CONTROL], "private, no-store");
        assert_eq!(reply.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(reply.headers[header::REFERRER_POLICY], "no-referrer");
        assert!(reply.headers.contains_key("x-request-id"));
    }
    env.finish().await;
}

#[tokio::test]
async fn each_credential_has_its_own_rate_limit() {
    let env = TestEnv::new().await;
    let app = pilot::router_with(
        &pilot::registry().unwrap(),
        env.state.clone(),
        admission(64, 0.1, 2.0),
    );
    for _ in 0..2 {
        let ok = send(&app, get("/api/v1/lessons", "alice", NORTHSIDE)).await;
        assert_eq!(ok.status, StatusCode::OK);
    }
    let limited = send(&app, get("/api/v1/lessons", "alice", NORTHSIDE)).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.body["code"], "rate_limited");
    assert_eq!(limited.headers[header::RETRY_AFTER], "10");
    assert_eq!(
        limited.headers[header::CONTENT_TYPE],
        "application/problem+json"
    );
    assert!(limited.body["request_id"].is_string());

    // Another credential is unaffected; health checks are never limited.
    let carol = send(&app, get("/api/v1/lessons", "carol", NORTHSIDE)).await;
    assert_eq!(carol.status, StatusCode::OK);
    let health = send(&app, get("/healthz", "alice", NORTHSIDE)).await;
    assert_eq!(health.status, StatusCode::OK);

    // A limited write never reached the executor.
    let write = send(
        &app,
        post_json(
            "/api/v1/lessons",
            "alice",
            NORTHSIDE,
            &json!({ "title": "Limited", "grade_level": 3 }),
        ),
    )
    .await;
    assert_eq!(write.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(env.lesson_count().await, 0);
    env.finish().await;
}

#[tokio::test]
async fn rotating_bogus_credentials_do_not_escape_the_limit() {
    let env = TestEnv::new().await;
    let app = pilot::router_with(
        &pilot::registry().unwrap(),
        env.state.clone(),
        admission(64, 0.1, 2.0),
    );
    // One client, a fresh made-up bearer token on every request: each must
    // not get a fresh bucket of its own.
    let mut limited = false;
    for i in 0..100 {
        let reply = send(
            &app,
            get("/api/v1/lessons", &format!("bogus{i}"), NORTHSIDE),
        )
        .await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    assert!(
        limited,
        "unverified credentials are limited by client address"
    );
    env.finish().await;
}

#[tokio::test]
async fn saturation_sheds_load_before_work_starts() {
    let env = TestEnv::new().await;
    let app = pilot::router_with(
        &pilot::registry().unwrap(),
        env.state.clone(),
        admission(0, 100.0, 100.0),
    );
    let shed = send(&app, get("/api/v1/lessons", "alice", NORTHSIDE)).await;
    assert_eq!(shed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(shed.body["code"], "overloaded");
    assert_eq!(shed.headers[header::RETRY_AFTER], "1");
    let (live, _, _) = send_raw(&app, get("/healthz", "alice", NORTHSIDE)).await;
    assert_eq!(live, StatusCode::OK, "liveness answers under saturation");
    env.finish().await;
}

#[tokio::test]
async fn metrics_use_route_templates_and_operation_ids_never_record_ids() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());
    let created = send(
        &app,
        post_json(
            "/api/v1/lessons",
            "alice",
            NORTHSIDE,
            &json!({ "title": "Metered", "grade_level": 2 }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.body["id"].as_str().unwrap().to_owned();
    let read = send(
        &app,
        get(&format!("/api/v1/lessons/{id}"), "alice", NORTHSIDE),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    let denied = send(
        &app,
        post_json(
            "/api/v1/lessons",
            "carol",
            NORTHSIDE,
            &json!({ "title": "No", "grade_level": 2 }),
        ),
    )
    .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let unknown = send(&app, get("/api/v1/lessons", "nobody", NORTHSIDE)).await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);

    let internal =
        pilot::server::metrics_router(env.pool.clone(), Admission::new(AdmissionConfig::default()));
    let response = internal
        .oneshot(
            http::Request::builder()
                .method(Method::GET)
                .uri("/metrics")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    for expected in [
        r#"trestle_http_requests_total{method="GET",route="/api/v1/lessons/{id}",status="2xx"}"#,
        r#"trestle_http_requests_total{method="POST",route="/api/v1/lessons",status="4xx"}"#,
        r#"trestle_operation_total{operation="lessons.create",outcome="ok"}"#,
        r#"trestle_operation_total{operation="lessons.create",outcome="permission_denied"}"#,
        r#"trestle_operation_duration_seconds_bucket{operation="lessons.get",le="+Inf"}"#,
        "trestle_db_acquire_seconds_count",
        r#"trestle_db_pool_connections{state="open"}"#,
        "trestle_http_in_flight 0",
        r#"trestle_auth_failures_total{reason="invalid"}"#,
    ] {
        assert!(text.contains(expected), "missing {expected}\n{text}");
    }
    for leaked in [
        id.as_str(),
        &NORTHSIDE.to_string(),
        &pilot::dev::ALICE.to_string(),
    ] {
        assert!(
            !text.contains(leaked),
            "metrics contain an identifier: {leaked}"
        );
    }
    env.finish().await;
}
