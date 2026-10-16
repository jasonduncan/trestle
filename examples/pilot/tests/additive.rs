//! The M5 additive change (`duration_minutes`, lessons.0004_duration) and
//! the caller's own schools (`schools.mine`).
//!
//! An additive field is safe across a rolling deploy only if clients built
//! before it keep working unchanged: their open forms are still accepted,
//! and their partial updates leave the new field alone.

mod support;

use http::{Method, StatusCode};
use pilot::dev::{NORTHSIDE, SOUTHSIDE};
use serde_json::json;
use support::{TestEnv, get, post_json, request, send};

const M4_CREATE_FORM: &str = "r-b68fa979090594a2";
const M4_ADMIN_EDIT_FORM: &str = "r-9adfca22d27dfc4e";

#[tokio::test]
async fn clients_built_before_the_field_keep_working_and_never_clear_it() {
    let env = TestEnv::new().await;
    let app = pilot::router(&pilot::registry().unwrap(), env.state.clone());

    // A new client sets the field.
    let created = send(
        &app,
        post_json(
            "/api/v1/lessons",
            "alice",
            NORTHSIDE,
            &json!({ "title": "Timed", "grade_level": 5, "duration_minutes": 45 }),
        ),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["duration_minutes"], 45);
    let id = created.body["id"].as_str().unwrap().to_owned();

    // An M4 create form (open before the deploy) is still accepted; its
    // lesson simply has no duration.
    let old_create = send(
        &app,
        request(
            Method::POST,
            "/api/v1/lessons",
            "alice",
            NORTHSIDE,
            Some(&json!({ "title": "Untimed", "grade_level": 5 })),
            &[("trestle-interaction-revision", M4_CREATE_FORM)],
        ),
    )
    .await;
    assert_eq!(
        old_create.status,
        StatusCode::CREATED,
        "{}",
        old_create.body
    );
    assert_eq!(old_create.body["duration_minutes"], json!(null));

    // An M4 admin edit (no duration field) changes the title only.
    let etag = format!(
        "\"lessons.view:{}\"",
        created.body["version"].as_str().unwrap()
    );
    let old_edit = send(
        &app,
        request(
            Method::PATCH,
            &format!("/api/v1/lessons/{id}"),
            "olivia",
            NORTHSIDE,
            Some(&json!({ "title": "Timed (renamed)" })),
            &[
                ("trestle-interaction-revision", M4_ADMIN_EDIT_FORM),
                ("if-match", &etag),
            ],
        ),
    )
    .await;
    assert_eq!(old_edit.status, StatusCode::OK, "{}", old_edit.body);
    assert_eq!(old_edit.body["title"], "Timed (renamed)");
    assert_eq!(
        old_edit.body["duration_minutes"], 45,
        "untouched by an old client"
    );

    // New clients can clear it; the database constraint matches the contract.
    let etag = format!(
        "\"lessons.view:{}\"",
        old_edit.body["version"].as_str().unwrap()
    );
    let cleared = send(
        &app,
        request(
            Method::PATCH,
            &format!("/api/v1/lessons/{id}"),
            "olivia",
            NORTHSIDE,
            Some(&json!({ "duration_minutes": null })),
            &[("if-match", &etag)],
        ),
    )
    .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert_eq!(cleared.body["duration_minutes"], json!(null));
    let too_long = send(
        &app,
        post_json(
            "/api/v1/lessons",
            "alice",
            NORTHSIDE,
            &json!({ "title": "Too long", "grade_level": 5, "duration_minutes": 601 }),
        ),
    )
    .await;
    assert_eq!(too_long.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(too_long.body["errors"][0]["pointer"], "/duration_minutes");
    let constraint = sqlx::query("INSERT INTO lessons (id, school_id, title, grade_level, created_by, duration_minutes) VALUES (gen_random_uuid(), $1, 'Direct', 3, gen_random_uuid(), 0)")
        .bind(NORTHSIDE)
        .execute(&env.pool)
        .await;
    assert!(
        constraint.is_err(),
        "the column check rejects what the contract rejects"
    );
    env.finish().await;
}

#[tokio::test]
async fn callers_see_only_their_own_schools() {
    let env = TestEnv::new().await;
    let app = pilot::router(&pilot::registry().unwrap(), env.state.clone());
    let mine = |who: &'static str, tenant| {
        let app = app.clone();
        async move { send(&app, get("/api/v1/me/schools", who, tenant)).await }
    };

    let alice = mine("alice", NORTHSIDE).await;
    assert_eq!(alice.status, StatusCode::OK, "{}", alice.body);
    assert_eq!(
        alice.body,
        json!({ "items": [{ "school": NORTHSIDE, "name": "Northside Elementary", "role": "teacher" }], "next_cursor": null })
    );
    // The tenant header is irrelevant here and grants nothing.
    let spoofed = mine("alice", SOUTHSIDE).await;
    assert_eq!(spoofed.body, alice.body);
    let olivia = mine("olivia", NORTHSIDE).await;
    assert_eq!(olivia.body["items"][0]["role"], "owner");
    let bob = mine("bob", NORTHSIDE).await;
    assert_eq!(bob.body["items"][0]["school"], SOUTHSIDE.to_string());

    let anonymous = mine("nobody", NORTHSIDE).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    env.finish().await;
}
