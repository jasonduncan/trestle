//! Boots the server against a throwaway database (TRESTLE_TEST_DATABASE_URL,
//! default postgres://localhost/postgres) and makes one request.

use serde_json::json;
use trestle::testing::TestApp;

const ALICE: &str = "00000000-0000-4000-8000-00000000a11c";

#[tokio::test]
async fn a_member_creates_a_note() {
    let app = TestApp::start(trestle::project(), &format!("alice-dev-token={ALICE}")).await;
    let workspace = app.tenant("Test").await;
    app.grant(workspace, ALICE.parse().unwrap(), "member").await;

    let res = app
        .post("/api/v1/notes", json!({ "title": "Hello" }))
        .token("alice-dev-token")
        .tenant(workspace)
        .send()
        .await;
    assert_eq!(res.status, 201, "{}", res.body);
    assert_eq!(res.body["title"], "Hello");

    app.finish().await;
}
