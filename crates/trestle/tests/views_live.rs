//! Release fixtures exercise real registries, PostgreSQL and executor routes.
//! Set TRESTLE_VIEW_BROWSER=1 after `npm run build:views` for browser evidence.
#[path = "support/views_app.rs"]
mod views_app;
use std::sync::Arc;

use axum::{Router, body::Body};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use trestle::State;
use trestle::core::identity::{ActorId, DevTokenIdentity};
use trestle::core::invocation::Invocation;
use trestle::historical::{
    MigrationSet, artifact,
    planner::{PlanOptions, diff},
    schema::SchemaState,
    testing::TestDatabase,
};
use uuid::Uuid;

const ALICE: Uuid = Uuid::from_u128(0xa11ce);
const BOB: Uuid = Uuid::from_u128(0xb0b);
const TENANT: Uuid = Uuid::from_u128(0x7ea3);
const OTHER: Uuid = Uuid::from_u128(0x7ea4);
const TOKENS: &str = "alice-view-test=00000000-0000-0000-0000-0000000a11ce,bob-view-test=00000000-0000-0000-0000-000000000b0b";

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    who: &str,
    tenant: Uuid,
    input: Value,
    extra: Option<(&str, &str)>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {who}-view-test"))
        .header("Trestle-Tenant", tenant.to_string())
        .header("Content-Type", "application/json");
    if let Some((name, value)) = extra {
        request = request.header(name, value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(if input.is_null() {
                    Body::empty()
                } else {
                    Body::from(input.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn browser(app: &Router, name: &str, stage: &str, schema: &str) {
    if std::env::var_os("TRESTLE_VIEW_BROWSER").is_none() {
        return;
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.clone();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let result = std::process::Command::new("node")
        .arg("e2e/run-views-live.mjs")
        .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../frontend"))
        .env("VIEW_BACKEND", format!("http://{address}"))
        .env("VIEW_APP", name)
        .env("VIEW_STAGE", stage)
        .env("VIEW_SCHEMA", schema)
        .output()
        .expect("launch browser harness");
    task.abort();
    assert!(
        result.status.success(),
        "browser {name}/{stage}: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reference_apps_keep_scope_and_contracts_at_release_fixture_size() {
    for (name, source, relation, primary, title, reference, role) in [
        (
            "support",
            include_str!("../../../fixtures/views/support.toml"),
            "accounts",
            "cases",
            "subject",
            "account",
            "staff",
        ),
        (
            "maintenance",
            include_str!("../../../fixtures/views/maintenance.toml"),
            "assets",
            "work_orders",
            "title",
            "asset",
            "technician",
        ),
    ] {
        let registry = Arc::new(views_app::project(name, source).assemble().unwrap());
        let schema =
            trestle::protocol::digest(&serde_json::to_value(registry.desired_schema()).unwrap());
        let db = TestDatabase::create().await;
        let pool = db.pool().await;
        let plan = diff(
            &SchemaState::default(),
            registry.desired_schema(),
            &PlanOptions::default(),
        );
        for migration in
            artifact::generate(&MigrationSet::new(vec![]).unwrap(), &plan, "fixture").unwrap()
        {
            sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        }
        for tenant in [TENANT, OTHER] {
            sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'Test workspace')")
                .bind(tenant)
                .execute(&pool)
                .await
                .unwrap();
            for actor in [ALICE, BOB] {
                sqlx::query("INSERT INTO workspace_memberships (workspace_id, actor_id, role) VALUES ($1, $2, $3)")
                    .bind(tenant).bind(actor).bind(if actor == BOB { "viewer" } else { role }).execute(&pool).await.unwrap();
            }
        }
        let state = State::new(
            pool.clone(),
            Arc::new(DevTokenIdentity::parse(TOKENS).unwrap()),
        );
        let app = trestle::router_with(&registry, state.clone(), Default::default());
        let (status, manifest) = call(
            &app,
            "GET",
            "/_meta/v1/ui/manifest",
            "alice",
            TENANT,
            Value::Null,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{manifest}");
        assert!(!manifest["views"].as_array().unwrap().is_empty());
        browser(&app, name, "empty", &schema).await;
        // SQL is fixture setup only. All browser and assertion writes use the executor.
        sqlx::query(&format!("INSERT INTO {relation} (id, workspace_id, name) SELECT md5(i::text)::uuid, $1, 'Choice ' || lpad(i::text, 3, '0') FROM generate_series(1,125) i"))
            .bind(TENANT).execute(&pool).await.unwrap();
        sqlx::query(&format!("INSERT INTO {primary} (id, workspace_id, {title}, {reference}) SELECT md5(('row-' || i)::text)::uuid, $1, 'Record ' || lpad(i::text, 5, '0'), md5('125')::uuid FROM generate_series(1,10000) i"))
            .bind(TENANT).execute(&pool).await.unwrap();
        let people = if name == "support" {
            "users"
        } else {
            "technicians"
        };
        sqlx::query(&format!("INSERT INTO {people} (id, workspace_id, name) SELECT md5(('person-' || i)::text)::uuid, $1, 'Person ' || lpad(i::text, 3, '0') FROM generate_series(1,125) i"))
            .bind(TENANT).execute(&pool).await.unwrap();
        let (status, page) = call(
            &app,
            "GET",
            &format!("/api/v1/{primary}?limit=20"),
            "alice",
            TENANT,
            Value::Null,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"].as_array().unwrap().len(), 20);
        let cursor = page["next_cursor"].as_str().unwrap();
        let continued = format!("/api/v1/{primary}?limit=20&cursor={cursor}");
        for (who, tenant, url) in [
            ("bob", TENANT, continued.clone()),
            ("alice", OTHER, continued.clone()),
            ("alice", TENANT, continued.replace("limit=20", "limit=21")),
        ] {
            let (status, result) = call(&app, "GET", &url, who, tenant, Value::Null, None).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "cursor escaped its scope: {result}"
            );
        }
        let (status, found) = call(
            &app,
            "GET",
            &format!("/api/v1/{relation}?q=125&limit=100"),
            "alice",
            TENANT,
            Value::Null,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{found}");
        assert_eq!(found["items"].as_array().unwrap().len(), 1);
        assert_eq!(found["items"][0]["name"], "Choice 125");
        let path = format!(
            "/api/v1/{primary}/{}",
            page["items"][0]["id"].as_str().unwrap()
        );
        assert_eq!(
            call(&app, "GET", &path, "alice", OTHER, Value::Null, None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let mut body = json!({});
        body[title] = json!("Denied");
        body[reference] = found["items"][0]["id"].clone();
        assert_eq!(
            call(
                &app,
                "POST",
                &format!("/api/v1/{primary}"),
                "bob",
                TENANT,
                body.clone(),
                None
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let pinned = call(
            &app,
            "POST",
            &format!("/api/v1/{primary}"),
            "alice",
            TENANT,
            body.clone(),
            Some(("Trestle-Operation-Version", "wrong")),
        )
        .await;
        assert_eq!(pinned.1["code"], "operation_version_stale");
        let op = if name == "support" {
            "support.case.create"
        } else {
            "maintenance.work_order.create"
        };
        let error = registry
            .invoke(
                &state,
                op,
                &Invocation::for_actor(ActorId(ALICE))
                    .with_tenant(TENANT)
                    .with_operation_version("wrong"),
                &body,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "operation_version_stale");
        let worker_task = if name == "support" {
            let row = &page["items"][0];
            let hints_path = format!("/api/v1/cases/{}/availability", row["id"].as_str().unwrap());
            let (status, hints) =
                call(&app, "GET", &hints_path, "alice", TENANT, Value::Null, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(hints["record_key"], row["id"]);
            assert_eq!(hints["actions"][0]["available"], true);
            let (_, viewer_hints) =
                call(&app, "GET", &hints_path, "bob", TENANT, Value::Null, None).await;
            assert_eq!(viewer_hints["actions"][0]["available"], false);
            assert_eq!(
                call(&app, "GET", &hints_path, "alice", OTHER, Value::Null, None)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
            let export_path = format!("/api/v1/cases/{}/export", row["id"].as_str().unwrap());
            let tag = format!("\"support.case:{}\"", row["version"]);
            let (status, work) = call(
                &app,
                "POST",
                &export_path,
                "alice",
                TENANT,
                json!({}),
                Some(("If-Match", &tag)),
            )
            .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{work}");
            let path = format!("/api/v1/exports/{}", work["id"].as_str().unwrap());
            assert_eq!(
                call(&app, "GET", &path, "bob", TENANT, Value::Null, None)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
            assert_eq!(
                call(&app, "GET", &path, "alice", OTHER, Value::Null, None)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
            let worker = trestle::core::jobs::Worker::new(
                registry.clone(),
                state.clone(),
                Default::default(),
            );
            assert_eq!(worker.run_once(10).await.unwrap().done, 1);
            let (_, finished) = call(&app, "GET", &path, "alice", TENANT, Value::Null, None).await;
            assert_eq!(finished["state"], "succeeded", "{finished}");
            Some(tokio::spawn(async move {
                loop {
                    worker.run_once(10).await.unwrap();
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }))
        } else {
            None
        };
        browser(&app, name, "populated", &schema).await;
        if let Some(task) = worker_task {
            task.abort();
        }

        pool.close().await;
        db.drop().await;
    }
}
