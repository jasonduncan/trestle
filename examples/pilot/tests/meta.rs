//! M2 backend (design doc §8.3–§8.5, §11.4): metadata endpoints and the
//! interaction-revision check on managed routes.

mod support;

use axum::body::Body;
use http::{Request, StatusCode, header};
use pilot::dev::{NORTHSIDE, SOUTHSIDE};
use serde_json::json;
use support::{Reply, TestEnv, get, post_json, send};
use trestle_core::protocol::Audience;

const MANIFEST: &str = "/_meta/v1/manifest";
const CREATE_FORM: &str = "/_meta/v1/interactions/lessons.create.form";

fn assert_no_store(reply: &Reply) {
    assert_eq!(
        reply.headers[header::CACHE_CONTROL],
        "private, no-store",
        "metadata is per-caller and must never be cached"
    );
}

fn create_with_revision(revision: Option<&str>, title: &str) -> Request<Body> {
    let mut request = post_json(
        "/api/v1/lessons",
        "alice",
        NORTHSIDE,
        &json!({"title": title, "grade_level": 5}),
    );
    if let Some(revision) = revision {
        request
            .headers_mut()
            .insert("trestle-interaction-revision", revision.parse().unwrap());
    }
    request
}

#[tokio::test]
async fn manifest_lists_visible_interactions_with_advisory_availability() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());

    let teacher = send(&app, get(MANIFEST, "alice", NORTHSIDE)).await;
    assert_eq!(teacher.status, StatusCode::OK);
    assert_no_store(&teacher);
    let entry = &teacher.body["interactions"][0];
    assert_eq!(entry["interactionId"], "lessons.create.form");
    assert_eq!(
        entry["revision"],
        registry
            .form("lessons.create.form")
            .unwrap()
            .revision
            .as_str()
    );
    assert_eq!(entry["availability"], json!({"available": true}));

    let viewer = send(&app, get(MANIFEST, "carol", NORTHSIDE)).await;
    assert_eq!(
        viewer.body["interactions"][0]["availability"],
        json!({"available": false, "reason": "permission_denied"})
    );

    let anonymous = send(
        &app,
        Request::builder()
            .uri(MANIFEST)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_no_store(&anonymous);

    // Bob claiming Alice's school sees nothing, and learns why.
    let outsider = send(&app, get(MANIFEST, "bob", NORTHSIDE)).await;
    assert_eq!(outsider.status, StatusCode::FORBIDDEN);
    assert_eq!(outsider.body["code"], "tenant_not_permitted");
    env.finish().await;
}

#[tokio::test]
async fn served_descriptor_is_the_exported_descriptor_plus_availability() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());

    let reply = send(&app, get(CREATE_FORM, "alice", NORTHSIDE)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_no_store(&reply);
    let mut served = reply.body.clone();
    assert_eq!(served["availability"], json!({"available": true}));
    served.as_object_mut().unwrap().remove("availability");
    let exported = &registry.interactions_export(Audience::Product)["interactions"][0];
    assert_eq!(
        &served, exported,
        "served and exported descriptors must not drift"
    );

    let missing = send(
        &app,
        get(
            "/_meta/v1/interactions/lessons.nope.form",
            "alice",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_no_store(&missing);

    // Knowing the ID does not grant access (§8.3).
    let outsider = send(&app, get(CREATE_FORM, "bob", NORTHSIDE)).await;
    assert_eq!(outsider.status, StatusCode::FORBIDDEN);
    let other_school = send(&app, get(CREATE_FORM, "bob", SOUTHSIDE)).await;
    assert_eq!(other_school.status, StatusCode::OK);
    env.finish().await;
}

#[tokio::test]
async fn submissions_from_unknown_form_revisions_are_refused_without_effect() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());
    let current = registry
        .form("lessons.create.form")
        .unwrap()
        .revision
        .clone();

    let ok = send(&app, create_with_revision(Some(&current), "Current form")).await;
    assert_eq!(ok.status, StatusCode::CREATED, "{:#}", ok.body);

    let stale = send(
        &app,
        create_with_revision(Some("r-0000000000000000"), "Stale form"),
    )
    .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(stale.body["code"], "interaction_stale");

    let garbage = send(
        &app,
        create_with_revision(Some("not a revision"), "Garbage"),
    )
    .await;
    assert_eq!(garbage.body["code"], "interaction_stale");

    // Plain API clients do not use metadata and are unaffected.
    let api = send(&app, create_with_revision(None, "API client")).await;
    assert_eq!(api.status, StatusCode::CREATED);
    assert_eq!(env.lesson_count().await, 2);

    // Authentication still comes first.
    let mut anonymous = create_with_revision(Some("r-0000000000000000"), "Anon");
    anonymous.headers_mut().remove(header::AUTHORIZATION);
    assert_eq!(send(&app, anonymous).await.status, StatusCode::UNAUTHORIZED);
    env.finish().await;
}

#[tokio::test]
async fn availability_is_advisory_and_submission_is_reauthorized() {
    // Revoke permission between loading the form and submitting it (§18 M2).
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());
    let loaded = send(&app, get(CREATE_FORM, "alice", NORTHSIDE)).await;
    assert_eq!(loaded.body["availability"]["available"], true);
    let revision = loaded.body["revision"].as_str().unwrap().to_owned();

    sqlx::query("UPDATE school_memberships SET role = 'viewer' WHERE actor_id = $1")
        .bind(pilot::dev::ALICE)
        .execute(&env.pool)
        .await
        .unwrap();

    let submitted = send(
        &app,
        create_with_revision(Some(&revision), "After revocation"),
    )
    .await;
    assert_eq!(submitted.status, StatusCode::FORBIDDEN);
    assert_eq!(submitted.body["code"], "permission_denied");
    assert_eq!(env.lesson_count().await, 0);
    env.finish().await;
}
