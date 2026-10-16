//! F8: pilot's `units` app rewritten as an intent file (`intent/units.toml`)
//! matches the hand-written Rust app: the same database schema, verified
//! against pilot's own migrated database, and the same behavior for the
//! core cases, with both servers reading and writing one database.
//!
//! Known differences are listed in `docs/guide/intent-parity.md`.

mod support;

use std::sync::Arc;

use axum::Router;
use http::StatusCode;
use pilot::dev::{DEV_TOKENS, NORTHSIDE, SOUTHSIDE};
use serde_json::{Value, json};
use support::{Reply, TestEnv, get, post_json, send};
use trestle_core::identity::DevTokenIdentity;

const UNITS: &str = include_str!("../intent/units.toml");

fn intent_project() -> trestle::Project {
    trestle::project().intent_str("intent/units.toml", UNITS)
}

struct Both {
    env: TestEnv,
    rust: Router,
    intent: Router,
}

impl Both {
    async fn new() -> Self {
        let env = TestEnv::new().await;
        let registry = pilot::registry().unwrap();
        let rust = pilot::router(&registry, env.state.clone());
        let intent_registry = intent_project().assemble().unwrap();
        let state = trestle::State::new(
            env.pool.clone(),
            Arc::new(DevTokenIdentity::parse(DEV_TOKENS).unwrap()),
        );
        let intent = trestle::router_with(&intent_registry, state, Default::default());
        Self { env, rust, intent }
    }

    async fn create(&self, app: &Router, who: &str, school: uuid::Uuid, name: &str) -> Reply {
        send(
            app,
            post_json("/api/v1/units", who, school, &json!({ "name": name })),
        )
        .await
    }
}

fn names(list: &Value) -> Vec<String> {
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_intent_schema_matches_pilots_database() {
    let env = TestEnv::new().await;
    let registry = intent_project().assemble().unwrap();
    let desired = registry.desired_schema();
    let tables: Vec<&str> = desired.models.values().map(|m| m.table.as_str()).collect();
    assert_eq!(tables.len(), 3, "{tables:?}");
    for table in ["schools", "school_memberships", "units"] {
        assert!(tables.contains(&table), "{table} missing from {tables:?}");
    }
    let mut conn = env.pool.acquire().await.unwrap();
    let report = trestle_historical::verify::verify(&mut conn, desired)
        .await
        .unwrap();
    assert!(
        report.mismatches.is_empty(),
        "intent schema differs from pilot's migrated database: {:#?}",
        report.mismatches
    );
    drop(conn);
    env.finish().await;
}

#[tokio::test]
async fn both_apps_behave_the_same_on_one_database() {
    let b = Both::new().await;
    for (label, app) in [("rust", &b.rust), ("intent", &b.intent)] {
        let name = format!("Fractions ({label})");
        // A teacher creates; the name comes back.
        let created = b.create(app, "alice", NORTHSIDE, &name).await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{label}: {:#}",
            created.body
        );
        assert_eq!(created.body["name"], name.as_str());
        let id = created.body["id"].as_str().unwrap().to_owned();

        // A duplicate name in the same school is a 422 on /name.
        let duplicate = b.create(app, "olivia", NORTHSIDE, &name).await;
        assert_eq!(
            duplicate.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label}"
        );
        let error = &duplicate.body["errors"][0];
        assert_eq!(
            (error["pointer"].as_str(), error["code"].as_str()),
            (Some("/name"), Some("already_exists")),
            "{label}: {:#}",
            duplicate.body
        );
        // ...but another school may use it.
        assert_eq!(
            b.create(app, "bob", SOUTHSIDE, &name).await.status,
            StatusCode::CREATED,
            "{label}"
        );

        // Viewers read but cannot write.
        assert_eq!(
            b.create(app, "carol", NORTHSIDE, "Viewer's unit")
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{label}"
        );
        // Contract violations are rejected before the database.
        assert_eq!(
            b.create(app, "alice", NORTHSIDE, "").await.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{label}: empty name"
        );

        // Other schools' units are invisible: 404, not 403.
        let other = send(app, get(&format!("/api/v1/units/{id}"), "bob", SOUTHSIDE)).await;
        assert_eq!(other.status, StatusCode::NOT_FOUND, "{label}");
        // Non-members of the selected school are refused.
        let stranger = send(app, get("/api/v1/units", "bob", NORTHSIDE)).await;
        assert_eq!(stranger.status, StatusCode::FORBIDDEN, "{label}");
    }

    // One database: each app reads what the other wrote, in name order.
    for (label, app) in [("rust", &b.rust), ("intent", &b.intent)] {
        let list = send(app, get("/api/v1/units", "carol", NORTHSIDE)).await;
        assert_eq!(list.status, StatusCode::OK, "{label}");
        assert_eq!(
            names(&list.body),
            ["Fractions (intent)", "Fractions (rust)"],
            "{label}"
        );
    }
    b.env.finish().await;
}
