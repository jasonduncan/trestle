//! Disposable PostgreSQL databases for integration tests.
//!
//! `TRESTLE_TEST_DATABASE_URL` (default `postgres://localhost/postgres`) must
//! name a role that may create and drop databases. A test that fails before
//! calling [`TestDatabase::drop`] leaves its `trestle_test_*` database behind
//! for inspection.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;

pub struct TestDatabase {
    admin: PgConnectOptions,
    options: PgConnectOptions,
    name: String,
}

impl TestDatabase {
    pub async fn create() -> Self {
        let url = std::env::var("TRESTLE_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://localhost/postgres".to_owned());
        let admin: PgConnectOptions = url
            .parse()
            .expect("TRESTLE_TEST_DATABASE_URL must be a PostgreSQL URL");
        let name = format!("trestle_test_{}", Uuid::new_v4().simple());
        let mut conn = PgConnection::connect_with(&admin)
            .await
            .expect("connect to PostgreSQL (is it running? see TRESTLE_TEST_DATABASE_URL)");
        sqlx::raw_sql(&format!("CREATE DATABASE {name}"))
            .execute(&mut conn)
            .await
            .expect("create test database");
        let _ = conn.close().await;
        let options = admin.clone().database(&name);
        Self {
            admin,
            options,
            name,
        }
    }

    pub fn options(&self) -> &PgConnectOptions {
        &self.options
    }

    pub async fn pool(&self) -> PgPool {
        PgPoolOptions::new()
            .max_connections(8)
            .connect_with(self.options.clone())
            .await
            .expect("connect to test database")
    }

    pub async fn connect(&self) -> PgConnection {
        PgConnection::connect_with(&self.options)
            .await
            .expect("connect to test database")
    }

    pub async fn drop(self) {
        let mut conn = PgConnection::connect_with(&self.admin)
            .await
            .expect("connect to PostgreSQL");
        sqlx::raw_sql(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .execute(&mut conn)
        .await
        .expect("drop test database");
        let _ = conn.close().await;
    }
}
