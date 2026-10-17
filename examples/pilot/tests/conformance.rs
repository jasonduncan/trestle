//! M1 exit (design doc §18): the published contract matches actual decoding
//! and validation, and forbidden input is rejected identically through the
//! managed route and a hand-written Axum handler (I02, I04, I13).
//!
//! Every case in `fixtures/contracts/lessons.create.input.json` is checked
//! three ways:
//!
//! 1. the published JSON Schema, by an independent validator (`jsonschema`);
//! 2. the managed route generated from the registry;
//! 3. an ordinary Axum handler calling the same executor.

mod support;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use http::StatusCode;
use pilot::AppState;
use pilot::dev::NORTHSIDE;
use pilot::lessons::contracts::LessonView;
use pilot::lessons::operations::CreateLesson;
use pilot::schools::TenantContext;
use serde_json::{Value, json};
use support::{Reply, TestEnv, get, post_json, send};
use trestle_core::Problem;
use trestle_core::input::JsonBody;
use trestle_core::invocation::Resolved;
use trestle_core::operation::execute_json;

const FIXTURES: &str = include_str!("../../../fixtures/contracts/lessons.create.input.json");

/// A hand-written handler: no registry, just the executor.
async fn manual_create(
    State(state): State<AppState>,
    Resolved(ctx): Resolved<TenantContext>,
    JsonBody(body): JsonBody,
) -> Result<(StatusCode, Json<LessonView>), Problem> {
    let view = execute_json(&state.pool, &CreateLesson, &ctx, &body).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Status, code and (pointer, code) pairs; drops request IDs and new IDs.
fn outcome(reply: &Reply) -> Value {
    if reply.status.is_success() {
        return json!({ "status": reply.status.as_u16() });
    }
    let errors: Vec<Value> = reply.body["errors"]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .map(|e| json!([e["pointer"], e["code"]]))
                .collect()
        })
        .unwrap_or_default();
    json!({ "status": reply.status.as_u16(), "code": reply.body["code"], "errors": errors })
}

fn expected(expect: &Value) -> Value {
    if expect["status"] == 201 {
        return json!({ "status": 201 });
    }
    json!({
        "status": expect["status"],
        "code": expect["code"],
        "errors": expect.get("errors").cloned().unwrap_or_else(|| json!([])),
    })
}

#[tokio::test]
async fn create_contract_agrees_across_schema_managed_route_and_manual_handler() {
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    let registry = pilot::registry().unwrap();
    let op = registry.operation("lessons.create").unwrap();
    assert_eq!(fixtures["contract"], op.input.id.as_str());
    assert_eq!(fixtures["version"], op.input.version.as_str());

    let input_schema = jsonschema::options()
        .should_validate_formats(true)
        .build(&op.input.json_schema())
        .unwrap();
    let output_schema = jsonschema::options()
        .should_validate_formats(true)
        .build(&op.output.json_schema())
        .unwrap();

    let managed_env = TestEnv::new().await;
    let managed = pilot::router(&registry, managed_env.state.clone());
    let manual_env = TestEnv::new().await;
    let manual = trestle_core::http::finish(
        Router::new()
            .route("/manual/lessons", post(manual_create))
            .with_state(manual_env.state.clone()),
    );

    let mut accepted_count = 0;
    for case in fixtures["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let expect = &case["expect"];
        let accepted = expect["status"] == 201;

        assert_eq!(
            input_schema.is_valid(input),
            accepted,
            "published JSON Schema disagrees with fixture {name:?}"
        );

        let via_managed = send(
            &managed,
            post_json("/api/v1/lessons", "alice", NORTHSIDE, input),
        )
        .await;
        let via_manual = send(
            &manual,
            post_json("/manual/lessons", "alice", NORTHSIDE, input),
        )
        .await;
        assert_eq!(
            outcome(&via_managed),
            expected(expect),
            "managed route disagrees with fixture {name:?}: {:#}",
            via_managed.body
        );
        assert_eq!(
            outcome(&via_manual),
            outcome(&via_managed),
            "manual handler and managed route disagree on {name:?}"
        );

        if accepted {
            accepted_count += 1;
            for reply in [&via_managed, &via_manual] {
                assert!(
                    output_schema.is_valid(&reply.body),
                    "response for {name:?} violates lessons.view: {:#}",
                    reply.body
                );
            }
            let uri = format!(
                "/api/v1/lessons/{}",
                via_managed.body["id"].as_str().unwrap()
            );
            let read = send(&managed, get(&uri, "alice", NORTHSIDE)).await;
            assert_eq!(read.status, StatusCode::OK);
            assert!(output_schema.is_valid(&read.body));
            assert_eq!(read.body, via_managed.body);
        }
    }
    assert!(accepted_count > 0);
    assert_eq!(managed_env.lesson_count().await, accepted_count);
    assert_eq!(manual_env.lesson_count().await, accepted_count);
    managed_env.finish().await;
    manual_env.finish().await;
}
