//! Shared helpers for pilot integration tests: a migrated and seeded
//! throwaway database, application state, and in-process requests.
#![allow(dead_code)]

pub mod idp;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use pilot::AppState;
use pilot::dev::DEV_TOKENS;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use trestle_core::identity::DevTokenIdentity;
use trestle_historical::testing::TestDatabase;
use uuid::Uuid;

pub struct TestEnv {
    db: TestDatabase,
    pub pool: PgPool,
    pub state: AppState,
}

impl TestEnv {
    pub async fn new() -> Self {
        let db = TestDatabase::create().await;
        trestle_historical::migrate(db.options(), &pilot_migrations::history().unwrap())
            .await
            .unwrap();
        let pool = db.pool().await;
        pilot::dev::seed(&pool).await.unwrap();
        let state = AppState::new(
            pool.clone(),
            Arc::new(DevTokenIdentity::parse(DEV_TOKENS).unwrap()),
        );
        Self { db, pool, state }
    }

    pub async fn lesson_count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM lessons")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    pub async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

pub async fn send(app: &Router, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.unwrap();
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

pub fn post_json(uri: &str, who: &str, school: Uuid, body: &Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {who}-dev-token"))
        .header("trestle-tenant", school.to_string())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Any request, with extra headers and an optional JSON body.
pub fn request(
    method: Method,
    uri: &str,
    who: &str,
    school: Uuid,
    body: Option<&Value>,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {who}-dev-token"))
        .header("trestle-tenant", school.to_string());
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

/// Raw response body (for non-JSON responses such as CSV).
pub async fn send_raw(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

pub fn get(uri: &str, who: &str, school: Uuid) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {who}-dev-token"))
        .header("trestle-tenant", school.to_string())
        .body(Body::empty())
        .unwrap()
}
