//! Boot a project against a throwaway database, for a project's own tests.
//!
//! Enable with `trestle = { ..., features = ["testing"] }` under
//! `[dev-dependencies]`. Needs a PostgreSQL role that may create databases
//! (`TRESTLE_TEST_DATABASE_URL`, default `postgres://localhost/postgres`).
//!
//! ```no_run
//! # async fn demo() {
//! use trestle::testing::TestApp;
//! let app = TestApp::start(trestle::project(), "alice-token=0190a1b2-0000-7000-8000-000000000001").await;
//! let res = app.get("/api/v1/notes").token("alice-token").send().await;
//! assert_eq!(res.status, 200);
//! app.finish().await;
//! # }
//! ```

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, header};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::intent::spec::Tenancy;
use crate::{Project, State};
use trestle_core::Problem;
use trestle_core::identity::DevTokenIdentity;
use trestle_core::invocation::Invocation;
use trestle_core::registry::Registry;
use trestle_historical::testing::TestDatabase;

/// A migrated database and the project's router, answering in-process.
pub struct TestApp {
    db: TestDatabase,
    pool: PgPool,
    router: Router,
    registry: Registry<State>,
    state: State,
    tenancy: Option<Tenancy>,
}

impl TestApp {
    /// Creates a database, applies every committed migration and builds
    /// the router. `dev_tokens` is `token=actor-uuid,...` as for
    /// `TRESTLE_DEV_TOKENS`. Panics on any failure: this is for tests.
    pub async fn start(project: Project, dev_tokens: &str) -> Self {
        let (registry, history, tenancy) = project.prepare().unwrap_or_else(|e| panic!("{e}"));
        let db = TestDatabase::create().await;
        trestle_historical::migrate(db.options(), &history)
            .await
            .unwrap_or_else(|e| panic!("migrate: {e}"));
        let pool = db.pool().await;
        let identity = Arc::new(DevTokenIdentity::parse(dev_tokens).expect("dev tokens"));
        let state = State::new(pool.clone(), identity);
        let router = crate::router_with(&registry, state.clone(), Default::default());
        Self {
            db,
            pool,
            router,
            registry,
            state,
            tenancy,
        }
    }

    /// Runs operation `id` without HTTP, as a job or command would
    /// (`Registry::invoke`, ADR-013).
    pub async fn invoke(
        &self,
        id: &str,
        invocation: &Invocation,
        input: &Value,
    ) -> Result<Value, Problem> {
        self.registry
            .invoke(&self.state, id, invocation, input)
            .await
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Creates a tenant of the `[tenancy]` model and returns its ID.
    pub async fn tenant(&self, name: &str) -> Uuid {
        let tenancy = self
            .tenancy
            .as_ref()
            .expect("trestle.toml declares no [tenancy]");
        let id = Uuid::now_v7();
        sqlx::query(&format!(
            "INSERT INTO {} (id, name) VALUES ($1, $2)",
            tenancy.table()
        ))
        .bind(id)
        .bind(name)
        .execute(&self.pool)
        .await
        .expect("create tenant");
        id
    }

    /// Gives `actor` a role from `[roles]` in `tenant`.
    pub async fn grant(&self, tenant: Uuid, actor: Uuid, role: &str) {
        let tenancy = self
            .tenancy
            .as_ref()
            .expect("trestle.toml declares no [tenancy]");
        sqlx::query(&format!(
            "INSERT INTO {} ({}, actor_id, role) VALUES ($1, $2, $3)",
            tenancy.membership_table(),
            tenancy.column()
        ))
        .bind(tenant)
        .bind(actor)
        .bind(role)
        .execute(&self.pool)
        .await
        .expect("grant role");
    }

    pub fn request(&self, method: Method, uri: &str) -> TestRequest<'_> {
        TestRequest {
            app: self,
            builder: Request::builder().method(method).uri(uri),
            body: None,
        }
    }
    pub fn get(&self, uri: &str) -> TestRequest<'_> {
        self.request(Method::GET, uri)
    }
    pub fn post(&self, uri: &str, body: Value) -> TestRequest<'_> {
        self.request(Method::POST, uri).json(body)
    }
    pub fn patch(&self, uri: &str, body: Value) -> TestRequest<'_> {
        self.request(Method::PATCH, uri).json(body)
    }
    pub fn delete(&self, uri: &str) -> TestRequest<'_> {
        self.request(Method::DELETE, uri)
    }

    /// Drops the database. A test that panics first leaves it behind for
    /// inspection.
    pub async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

pub struct TestRequest<'a> {
    app: &'a TestApp,
    builder: http::request::Builder,
    body: Option<Value>,
}

impl TestRequest<'_> {
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.builder = self.builder.header(name, value);
        self
    }
    /// `Authorization: Bearer <token>`.
    pub fn token(self, token: &str) -> Self {
        self.header(header::AUTHORIZATION.as_str(), &format!("Bearer {token}"))
    }
    /// `Trestle-Tenant: <id>`.
    pub fn tenant(self, tenant: Uuid) -> Self {
        self.header("Trestle-Tenant", &tenant.to_string())
    }
    pub fn if_match(self, etag: &str) -> Self {
        self.header(header::IF_MATCH.as_str(), etag)
    }
    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }
    pub async fn send(self) -> TestResponse {
        let req = match self.body {
            Some(b) => self
                .builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => self.builder.body(Body::empty()),
        }
        .expect("valid request");
        let res = self
            .app
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("infallible");
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        TestResponse {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }
}

pub struct TestResponse {
    pub status: u16,
    pub headers: HeaderMap,
    /// The JSON body, or `null` when empty or not JSON.
    pub body: Value,
}

impl TestResponse {
    pub fn etag(&self) -> Option<&str> {
        self.headers.get(header::ETAG)?.to_str().ok()
    }
}
