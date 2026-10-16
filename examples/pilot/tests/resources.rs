//! M4 (design doc §18): resources, lists and cursors, patch semantics,
//! preconditions, domain actions, hidden relation targets, the admin
//! profile, and a hand-written Axum route beside managed ones.

mod support;

use axum::Router;
use http::{Method, StatusCode, header};
use pilot::dev::{NORTHSIDE, SOUTHSIDE};
use serde_json::{Value, json};
use support::{Reply, TestEnv, get, post_json, request, send, send_raw};

struct Fixture {
    env: TestEnv,
    app: Router,
}

impl Fixture {
    async fn new() -> Self {
        let env = TestEnv::new().await;
        let registry = pilot::registry().unwrap();
        let app = pilot::router(&registry, env.state.clone());
        Self { env, app }
    }

    async fn create_lesson(&self, who: &str, school: uuid::Uuid, body: Value) -> Reply {
        send(&self.app, post_json("/api/v1/lessons", who, school, &body)).await
    }

    async fn create_unit(&self, who: &str, school: uuid::Uuid, name: &str) -> String {
        let reply = send(
            &self.app,
            post_json("/api/v1/units", who, school, &json!({"name": name})),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{:#}", reply.body);
        reply.body["id"].as_str().unwrap().to_owned()
    }

    async fn call(
        &self,
        method: Method,
        uri: &str,
        who: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Reply {
        send(
            &self.app,
            request(method, uri, who, NORTHSIDE, body.as_ref(), headers),
        )
        .await
    }

    async fn finish(self) {
        self.env.finish().await;
    }
}

fn etag(reply: &Reply) -> String {
    reply.headers[header::ETAG].to_str().unwrap().to_owned()
}

#[tokio::test]
async fn lists_page_with_signed_cursors_bound_to_the_query() {
    let f = Fixture::new().await;
    for name in ["Algebra", "Biology", "Chemistry", "Drama", "Economics"] {
        f.create_unit("alice", NORTHSIDE, name).await;
    }
    f.create_unit("bob", SOUTHSIDE, "Astronomy").await;

    let mut names = Vec::new();
    let mut uri = "/api/v1/units?limit=2".to_owned();
    let mut pages = 0;
    loop {
        let page = send(&f.app, get(&uri, "alice", NORTHSIDE)).await;
        assert_eq!(page.status, StatusCode::OK, "{:#}", page.body);
        pages += 1;
        names.extend(
            page.body["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|u| u["name"].as_str().unwrap().to_owned()),
        );
        match page.body["next_cursor"].as_str() {
            Some(cursor) => uri = format!("/api/v1/units?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(
        names,
        ["Algebra", "Biology", "Chemistry", "Drama", "Economics"],
        "scoped, ordered, no gaps"
    );

    let first = send(&f.app, get("/api/v1/units?limit=2", "alice", NORTHSIDE)).await;
    let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
    let reused = send(
        &f.app,
        get(
            &format!("/api/v1/units?limit=3&cursor={cursor}"),
            "alice",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(
        (reused.status, reused.body["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("cursor_mismatch"))
    );
    let mut forged = cursor.clone();
    forged.insert(3, 'x');
    let tampered = send(
        &f.app,
        get(
            &format!("/api/v1/units?limit=2&cursor={forged}"),
            "alice",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(tampered.body["code"], "invalid_cursor");

    // Only allowlisted inputs; bounded pages.
    let unknown = send(
        &f.app,
        get("/api/v1/lessons?order=created_by", "alice", NORTHSIDE),
    )
    .await;
    assert_eq!(
        (unknown.status, unknown.body["errors"][0]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("unknown_field"))
    );
    let huge = send(
        &f.app,
        get("/api/v1/lessons?limit=1000", "alice", NORTHSIDE),
    )
    .await;
    assert_eq!(huge.body["errors"][0]["code"], "maximum");
    let repeated = send(
        &f.app,
        get(
            "/api/v1/lessons?grade_level=1&grade_level=2",
            "alice",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(repeated.body["code"], "conflicting_input");
    f.finish().await;
}

#[tokio::test]
async fn lesson_filters_apply_inside_the_tenant_scope() {
    let f = Fixture::new().await;
    for (title, grade) in [("Ratios", 6), ("Area", 6), ("Fractions", 4)] {
        f.create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": title, "grade_level": grade}),
        )
        .await;
    }
    f.create_lesson(
        "bob",
        SOUTHSIDE,
        json!({"title": "Other school", "grade_level": 6}),
    )
    .await;
    let sixth = send(
        &f.app,
        get("/api/v1/lessons?grade_level=6", "alice", NORTHSIDE),
    )
    .await;
    let titles: Vec<&str> = sixth.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["Area", "Ratios"]);
    let drafts = send(
        &f.app,
        get("/api/v1/lessons?status=draft", "alice", NORTHSIDE),
    )
    .await;
    assert_eq!(drafts.body["items"].as_array().unwrap().len(), 3);
    f.finish().await;
}

#[tokio::test]
async fn patch_distinguishes_missing_null_and_value_and_requires_a_precondition() {
    let f = Fixture::new().await;
    let created = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Fractions", "grade_level": 4}),
        )
        .await;
    let id = created.body["id"].as_str().unwrap();
    let uri = format!("/api/v1/lessons/{id}");
    assert_eq!(etag(&created), "\"lessons.view:1\"");

    let read = send(&f.app, get(&uri, "alice", NORTHSIDE)).await;
    let v1 = etag(&read);

    let missing = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"subtitle": "Parts"})),
            &[],
        )
        .await;
    assert_eq!(
        (missing.status, missing.body["code"].as_str()),
        (
            StatusCode::PRECONDITION_REQUIRED,
            Some("precondition_required")
        )
    );
    let other_rep = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"subtitle": "Parts"})),
            &[("if-match", "\"units.view:1\"")],
        )
        .await;
    assert_eq!(other_rep.status, StatusCode::PRECONDITION_FAILED);

    let set = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"subtitle": "Parts"})),
            &[("if-match", &v1)],
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{:#}", set.body);
    assert_eq!(
        (set.body["subtitle"].as_str(), set.body["version"].as_str()),
        (Some("Parts"), Some("2"))
    );
    let v2 = etag(&set);
    assert_eq!(v2, "\"lessons.view:2\"");

    // Missing leaves the field alone.
    let grade = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"grade_level": 5})),
            &[("if-match", &v2)],
        )
        .await;
    assert_eq!(
        (
            grade.body["subtitle"].as_str(),
            grade.body["grade_level"].as_i64()
        ),
        (Some("Parts"), Some(5))
    );
    let v3 = etag(&grade);

    // Null clears a nullable field; null on a required one is refused.
    let cleared = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"subtitle": null})),
            &[("if-match", &v3)],
        )
        .await;
    assert_eq!(cleared.body["subtitle"], Value::Null);
    let v4 = etag(&cleared);
    let null_title = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"title": null})),
            &[("if-match", &v4)],
        )
        .await;
    assert_eq!(null_title.body["errors"][0]["code"], "not_nullable");

    // Stale edit: the old version no longer matches.
    let stale = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"title": "Overwrite"})),
            &[("if-match", &v1)],
        )
        .await;
    assert_eq!(
        (stale.status, stale.body["code"].as_str()),
        (StatusCode::PRECONDITION_FAILED, Some("precondition_failed"))
    );

    // An empty patch changes nothing but still checks the version.
    let noop = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({})),
            &[("if-match", &v4)],
        )
        .await;
    assert_eq!(
        (noop.status, noop.body["version"].as_str()),
        (StatusCode::OK, Some("4"))
    );

    let now = send(&f.app, get(&uri, "alice", NORTHSIDE)).await;
    assert_eq!(
        (now.body["title"].as_str(), now.body["version"].as_str()),
        (Some("Fractions"), Some("4"))
    );
    f.finish().await;
}

#[tokio::test]
async fn forbidden_fields_cannot_be_patched_or_created() {
    let f = Fixture::new().await;
    let created = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Fractions", "grade_level": 4}),
        )
        .await;
    let id = created.body["id"].as_str().unwrap();
    let uri = format!("/api/v1/lessons/{id}");
    let tag = etag(&created);
    let forbidden = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"status": "published", "version": "99", "school_id": SOUTHSIDE, "created_by": pilot::dev::BOB})),
            &[("if-match", &tag)],
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = forbidden.body["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["pointer"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["/created_by", "/school_id", "/status", "/version"]);
    let id_in_body = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"id": id})),
            &[("if-match", &tag)],
        )
        .await;
    assert_eq!(id_in_body.body["code"], "conflicting_input");
    let viewer = f
        .call(
            Method::PATCH,
            &uri,
            "carol",
            Some(json!({"title": "x"})),
            &[("if-match", &tag)],
        )
        .await;
    assert_eq!(
        (viewer.status, viewer.body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("permission_denied"))
    );
    let unchanged = send(&f.app, get(&uri, "alice", NORTHSIDE)).await;
    assert_eq!(
        (
            unchanged.body["status"].as_str(),
            unchanged.body["version"].as_str()
        ),
        (Some("draft"), Some("1"))
    );
    f.finish().await;
}

#[tokio::test]
async fn hidden_relation_targets_are_refused_and_invisible() {
    let f = Fixture::new().await;
    let mine = f.create_unit("alice", NORTHSIDE, "Number sense").await;
    let theirs = f.create_unit("bob", SOUTHSIDE, "Their unit").await;

    let picker = send(&f.app, get("/api/v1/units", "alice", NORTHSIDE)).await;
    let ids: Vec<&str> = picker.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [mine.as_str()],
        "the relation picker only offers this school's units"
    );
    let peek = send(
        &f.app,
        get(&format!("/api/v1/units/{theirs}"), "alice", NORTHSIDE),
    )
    .await;
    assert_eq!(peek.status, StatusCode::NOT_FOUND);

    let smuggled = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Smuggled", "grade_level": 3, "unit_id": theirs}),
        )
        .await;
    assert_eq!(smuggled.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        (
            smuggled.body["errors"][0]["pointer"].as_str(),
            smuggled.body["errors"][0]["code"].as_str()
        ),
        (Some("/unit_id"), Some("not_found"))
    );
    let nonexistent = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Ghost", "grade_level": 3, "unit_id": uuid::Uuid::now_v7()}),
        )
        .await;
    assert_eq!(
        nonexistent.body["errors"], smuggled.body["errors"],
        "another school's unit looks exactly like no unit"
    );

    let placed = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Placed", "grade_level": 3, "unit_id": mine}),
        )
        .await;
    assert_eq!(placed.body["unit_id"], mine.as_str());
    let uri = format!("/api/v1/lessons/{}", placed.body["id"].as_str().unwrap());
    let moved = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"unit_id": theirs})),
            &[("if-match", &etag(&placed))],
        )
        .await;
    assert_eq!(moved.body["errors"][0]["code"], "not_found");
    let unset = f
        .call(
            Method::PATCH,
            &uri,
            "alice",
            Some(json!({"unit_id": null})),
            &[("if-match", &etag(&placed))],
        )
        .await;
    assert_eq!(unset.body["unit_id"], Value::Null);
    assert_eq!(f.env.lesson_count().await, 1);
    f.finish().await;
}

#[tokio::test]
async fn publish_is_a_domain_action_with_version_and_state_conflicts() {
    let f = Fixture::new().await;
    let created = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Fractions", "grade_level": 4}),
        )
        .await;
    let id = created.body["id"].as_str().unwrap();
    let publish = format!("/api/v1/lessons/{id}/actions/publish");

    let required = f
        .call(Method::POST, &publish, "alice", Some(json!({})), &[])
        .await;
    assert_eq!(required.body["errors"][0]["pointer"], "/expected_version");
    let viewer = f
        .call(
            Method::POST,
            &publish,
            "carol",
            Some(json!({"expected_version": "1"})),
            &[],
        )
        .await;
    assert_eq!(viewer.status, StatusCode::FORBIDDEN);
    let stale = f
        .call(
            Method::POST,
            &publish,
            "alice",
            Some(json!({"expected_version": "7"})),
            &[],
        )
        .await;
    assert_eq!(
        (stale.status, stale.body["code"].as_str()),
        (StatusCode::CONFLICT, Some("stale_version"))
    );

    let published = f
        .call(
            Method::POST,
            &publish,
            "alice",
            Some(json!({"expected_version": "1"})),
            &[],
        )
        .await;
    assert_eq!(published.status, StatusCode::OK, "{:#}", published.body);
    assert_eq!(
        (
            published.body["status"].as_str(),
            published.body["version"].as_str()
        ),
        (Some("published"), Some("2"))
    );
    let again = f
        .call(
            Method::POST,
            &publish,
            "alice",
            Some(json!({"expected_version": "2"})),
            &[],
        )
        .await;
    assert_eq!(
        (again.status, again.body["code"].as_str()),
        (StatusCode::CONFLICT, Some("invalid_transition"))
    );

    let elsewhere = send(
        &f.app,
        request(
            Method::POST,
            &publish,
            "bob",
            SOUTHSIDE,
            Some(&json!({"expected_version": "2"})),
            &[],
        ),
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
    f.finish().await;
}

#[tokio::test]
async fn delete_requires_the_current_version() {
    let f = Fixture::new().await;
    let created = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Fractions", "grade_level": 4}),
        )
        .await;
    let uri = format!("/api/v1/lessons/{}", created.body["id"].as_str().unwrap());
    assert_eq!(
        f.call(Method::DELETE, &uri, "alice", None, &[])
            .await
            .status,
        StatusCode::PRECONDITION_REQUIRED
    );
    let stale = f
        .call(
            Method::DELETE,
            &uri,
            "alice",
            None,
            &[("if-match", "\"lessons.view:9\"")],
        )
        .await;
    assert_eq!(stale.status, StatusCode::PRECONDITION_FAILED);
    let deleted = f
        .call(
            Method::DELETE,
            &uri,
            "alice",
            None,
            &[("if-match", &etag(&created))],
        )
        .await;
    assert_eq!(
        (deleted.status, &deleted.body),
        (StatusCode::NO_CONTENT, &Value::Null)
    );
    assert_eq!(
        send(&f.app, get(&uri, "alice", NORTHSIDE)).await.status,
        StatusCode::NOT_FOUND
    );
    let gone = f
        .call(
            Method::DELETE,
            &uri,
            "alice",
            None,
            &[("if-match", &etag(&created))],
        )
        .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    f.finish().await;
}

#[tokio::test]
async fn admin_profile_is_owner_only_and_uses_the_same_operations() {
    let f = Fixture::new().await;
    let manifest = send(&f.app, get("/_meta/v1/admin/manifest", "olivia", NORTHSIDE)).await;
    assert_eq!(manifest.status, StatusCode::OK, "{:#}", manifest.body);
    assert_eq!(manifest.headers[header::CACHE_CONTROL], "private, no-store");
    let lessons = &manifest.body["resources"][0];
    assert_eq!(
        manifest.body["resources"].as_array().unwrap().len(),
        1,
        "units has no admin view"
    );
    assert_eq!(
        lessons["columns"],
        json!(["title", "grade_level", "status", "version"])
    );
    let action = &lessons["actions"][0];
    assert_eq!(
        (action["operation"].as_str(), action["path"].as_str()),
        (
            Some("lessons.publish"),
            Some("/api/v1/lessons/{id}/actions/publish")
        )
    );
    assert_eq!(action["bind"], json!({"expected_version": "/version"}));
    assert_eq!(
        action["visibleWhen"],
        json!({"pointer": "/status", "equals": "draft"})
    );
    assert!(
        !manifest.body.to_string().contains("schools.admin"),
        "policy IDs are never published"
    );

    let teacher = send(&f.app, get("/_meta/v1/admin/manifest", "alice", NORTHSIDE)).await;
    assert_eq!(
        (teacher.status, teacher.body["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("admin_forbidden"))
    );
    let form = send(
        &f.app,
        get(
            "/_meta/v1/admin/interactions/lessons.admin_edit.form",
            "olivia",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(
        form.body["operation"]["binding"]["precondition"]["header"],
        "If-Match"
    );
    assert_eq!(
        form.body["requiredFeatures"],
        json!([
            "forms.core@1",
            "http.json@1",
            "forms.relation@1",
            "http.precondition@1"
        ])
    );
    let form_as_teacher = send(
        &f.app,
        get(
            "/_meta/v1/admin/interactions/lessons.admin_edit.form",
            "alice",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(form_as_teacher.status, StatusCode::FORBIDDEN);
    let not_product = send(
        &f.app,
        get(
            "/_meta/v1/interactions/lessons.admin_edit.form",
            "olivia",
            NORTHSIDE,
        ),
    )
    .await;
    assert_eq!(
        not_product.status,
        StatusCode::NOT_FOUND,
        "admin forms are not in the product profile"
    );

    // Product and admin publish through the same operation and route.
    let one = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Product publish", "grade_level": 4}),
        )
        .await;
    let two = f
        .create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": "Admin publish", "grade_level": 4}),
        )
        .await;
    let product = f
        .call(
            Method::POST,
            &format!(
                "/api/v1/lessons/{}/actions/publish",
                one.body["id"].as_str().unwrap()
            ),
            "alice",
            Some(json!({"expected_version": "1"})),
            &[],
        )
        .await;
    let admin_path = action["path"]
        .as_str()
        .unwrap()
        .replace("{id}", two.body["id"].as_str().unwrap());
    let admin = f
        .call(
            Method::POST,
            &admin_path,
            "olivia",
            Some(json!({"expected_version": "1"})),
            &[],
        )
        .await;
    assert_eq!(
        (product.status, admin.status),
        (StatusCode::OK, StatusCode::OK)
    );
    let actors: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT actor_id, operation FROM audit_events WHERE operation = 'lessons.publish' ORDER BY occurred_at")
            .fetch_all(&f.env.pool)
            .await
            .unwrap();
    assert_eq!(
        actors,
        [
            (pilot::dev::ALICE, "lessons.publish".into()),
            (pilot::dev::OLIVIA, "lessons.publish".into())
        ]
    );
    f.finish().await;
}

#[tokio::test]
async fn a_hand_written_axum_route_reuses_the_operations() {
    let f = Fixture::new().await;
    for (title, grade) in [
        ("Fractions", 4),
        ("=HYPERLINK(\"http://x\")", 5),
        ("Ratios, part 1", 6),
    ] {
        f.create_lesson(
            "alice",
            NORTHSIDE,
            json!({"title": title, "grade_level": grade}),
        )
        .await;
    }
    f.create_lesson(
        "bob",
        SOUTHSIDE,
        json!({"title": "Southside only", "grade_level": 6}),
    )
    .await;

    let (status, headers, csv) = send_raw(
        &f.app,
        request(
            Method::GET,
            "/api/v1/lessons/export.csv",
            "alice",
            NORTHSIDE,
            None,
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 4, "{csv}");
    assert!(
        csv.contains("\"'=HYPERLINK(\"\"http://x\"\")\""),
        "formulas are neutralized: {csv}"
    );
    assert!(csv.contains("\"Ratios, part 1\""));
    assert!(
        !csv.contains("Southside only"),
        "tenant scope comes from the operation"
    );

    let anonymous = send(
        &f.app,
        http::Request::builder()
            .uri("/api/v1/lessons/export.csv")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    f.finish().await;
}
