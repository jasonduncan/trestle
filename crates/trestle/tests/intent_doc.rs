//! ADR-030: the intent as a value. An `IntentDoc` reads every key of the
//! file, and a project built from a document assembles, refuses and serves
//! exactly like one built from the file it renders to.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use trestle::intent::toml;
use trestle::intent::{EndpointDoc, FieldDoc, IntentDoc, ModelDoc, TenancyDoc, Write};
use trestle::prelude::*;
use trestle::protocol::Audience;
use trestle::testing::TestApp;

const EVERY_KEY: &str = include_str!("fixtures/intent_doc/every_key.toml");
const NOTES: &str = include_str!("../examples/notes_intent/trestle.toml");
const NOTES_MIGRATIONS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/notes_intent/migrations"
);

async fn passthrough(_ctx: Ctx, row: Row) -> Result<Row> {
    Ok(row)
}

async fn unchanged(_ctx: Ctx, write: Write) -> Result<Write> {
    Ok(write)
}

async fn flag(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.get("crm.deal", input.id()?).await
}

async fn pin(ctx: Ctx, input: Input) -> Result<Row> {
    ctx.update("notes.note", input.id()?, json!({ "pinned": true }))
        .await
}

fn crm(project: trestle::Project) -> trestle::Project {
    project
        .handler("crm::flag", flag)
        .hook("crm::tidy", passthrough)
        .write_hook("crm::deal_rules", unchanged)
        .write_hook("crm::deal_effects", unchanged)
}

fn notes(project: trestle::Project) -> trestle::Project {
    project
        .handler("notes::pin", pin)
        .hook("notes::tidy", passthrough)
        .migrations_dir(NOTES_MIGRATIONS)
}

/// Everything a client or the database can see of an assembled project.
fn surface(project: trestle::Project) -> Value {
    let registry = project.assemble().unwrap_or_else(|e| panic!("{e}"));
    let mut ops: Vec<Value> = registry
        .operations()
        .iter()
        .map(|o| json!([&*o.info.id, &*o.policy, format!("{:?}", o.binding)]))
        .collect();
    ops.sort_by_key(|o| o.to_string());
    json!({
        "operations": ops,
        "schema": serde_json::to_value(registry.desired_schema()).unwrap(),
        "contracts": [
            registry.contracts_export(Audience::Product),
            registry.contracts_export(Audience::Admin),
        ],
        "interactions": [
            registry.interactions_export(Audience::Product),
            registry.interactions_export(Audience::Admin),
        ],
        "openapi": [
            registry.openapi(Audience::Product),
            registry.openapi(Audience::Admin),
        ],
        "admin": registry.admin_export(),
        "views": [registry.views_export(Audience::Product), registry.views_export(Audience::Admin)],
    })
}

fn refusal(project: trestle::Project) -> Vec<String> {
    match project.assemble() {
        Ok(_) => panic!("the project assembled"),
        Err(e) => e.issues,
    }
}

#[test]
fn the_every_key_fixture_is_a_valid_intent() {
    trestle::intent::parse("every_key.toml", EVERY_KEY).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn a_document_reads_every_key_and_assembles_like_its_file() {
    let doc = IntentDoc::from_toml("every_key.toml", EVERY_KEY).unwrap();
    // Spot checks that the keys landed where the file put them.
    assert!(!doc.auth.passwords);
    assert_eq!(doc.models[0].table.as_deref(), Some("client_accounts"));
    assert_eq!(doc.models[1].fields[1].currencies, ["EUR", "USD"]);
    assert_eq!(doc.models[1].fields[4].labels["won"], "Closed won");
    assert_eq!(doc.models[1].fields[4].writable.as_deref(), Some("actions"));
    assert_eq!(doc.models[1].hooks["before_write"], "crm::deal_rules");
    assert_eq!(doc.models[1].hooks["after_write"], "crm::deal_effects");
    assert_eq!(doc.models[0].scopes["mine"].field, "external");
    assert_eq!(
        doc.models[1].scopes["own"].through.as_deref(),
        Some("crm.account")
    );
    assert_eq!(doc.models[1].scopes["own"].key.as_deref(), Some("id"));
    assert_eq!(
        doc.models[1].scopes["own"].actor.as_deref(),
        Some("external")
    );
    assert_eq!(doc.roles["member"][1], "crm.write@own");
    assert_eq!(doc.endpoints[3].accept, ["reason"]);
    assert_eq!(
        doc.endpoints[3].check[0].gt.as_ref().unwrap()["currency"].as_str(),
        Some("USD")
    );
    assert_eq!(doc.endpoints[3].exists[0].matches["id"], "account");
    assert_eq!(doc.endpoints[3].repeat.as_deref(), Some("unchanged"));
    assert!(!doc.endpoints[0].idempotent);
    assert!(
        doc.endpoints[1].idempotent && doc.endpoints[2].idempotent && doc.endpoints[3].idempotent
    );
    assert_eq!(
        doc.admin[0].filters.as_deref(),
        Some(&["stage".to_owned()][..])
    );

    let rendered = doc.to_toml().unwrap();
    assert_eq!(
        IntentDoc::from_toml("rendered.toml", &rendered).unwrap(),
        doc,
        "rendering loses nothing:\n{rendered}"
    );
    assert_eq!(
        surface(crm(trestle::project().intent_doc("crm", doc))),
        surface(crm(trestle::project().intent_str("crm", EVERY_KEY))),
    );
}

#[test]
fn a_document_built_in_code_assembles_like_the_notes_file() {
    let field = |id: &str, ty: &str| FieldDoc {
        id: id.into(),
        ty: ty.into(),
        ..FieldDoc::default()
    };
    let mut doc = IntentDoc::new("notes");
    doc.project.trestle = Some("0.1".into());
    doc.tenancy = Some(TenancyDoc {
        model: "workspace".into(),
    });
    doc.roles = BTreeMap::from([
        (
            "member".into(),
            vec!["notes.read".into(), "notes.write".into()],
        ),
        ("viewer".into(), vec!["notes.read".into()]),
    ]);
    doc.models.push(ModelDoc {
        id: "notes.note".into(),
        fields: vec![
            FieldDoc {
                max: Some(200),
                ..field("title", "text")
            },
            FieldDoc {
                optional: true,
                ..field("body", "text")
            },
            FieldDoc {
                default: Some(toml::Value::Boolean(false)),
                ..field("pinned", "bool")
            },
        ],
        ..ModelDoc::default()
    });
    doc.endpoints.push(EndpointDoc {
        model: Some("notes.note".into()),
        crud: ["list", "get", "create", "update", "delete"]
            .map(String::from)
            .into(),
        read: Some("notes.read".into()),
        write: Some("notes.write".into()),
        filter: vec!["pinned".into()],
        order: Some("-created_at".into()),
        hooks: BTreeMap::from([("validate".into(), "notes::tidy".into())]),
        ..EndpointDoc::default()
    });
    doc.endpoints.push(EndpointDoc {
        id: Some("notes.pin".into()),
        path: Some("POST /api/v1/notes/{id}/pin".into()),
        permission: Some("notes.write".into()),
        handler: Some("notes::pin".into()),
        ..EndpointDoc::default()
    });

    assert_eq!(
        surface(notes(trestle::project().intent_doc("notes", doc))),
        surface(notes(trestle::project().intent_str("trestle.toml", NOTES))),
    );
}

#[test]
fn a_document_is_refused_like_its_file_with_lines_in_its_rendering() {
    let mut doc = IntentDoc::from_toml("every_key.toml", EVERY_KEY).unwrap();
    doc.models[1].fields[3].to = Some("crm.nothing".into());
    doc.endpoints[0].filter = vec!["shade".into()];
    doc.admin[0].columns = Some(vec!["colour".into()]);

    let from_doc = refusal(crm(trestle::project().intent_doc("crm", doc.clone())));
    let rendered = doc.to_toml().unwrap();
    let from_text = refusal(crm(trestle::project().intent_str("crm", &rendered)));
    assert_eq!(from_doc, from_text, "the same problems, in the same order");
    assert!(from_doc.len() >= 3, "every problem at once: {from_doc:?}");

    let lines: Vec<&str> = rendered.lines().collect();
    for (problem, needle) in [
        ("crm.nothing", "crm.nothing"),
        ("shade", "shade"),
        ("colour", "colour"),
    ] {
        let issue = from_doc
            .iter()
            .find(|i| i.contains(problem))
            .unwrap_or_else(|| panic!("no problem mentions {problem}: {from_doc:?}"));
        let line: usize = issue
            .strip_prefix("crm:")
            .and_then(|rest| rest.split(':').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{issue:?} names no line"));
        assert!(
            lines[line - 1].contains(needle),
            "{issue:?} points at line {line}: {:?}",
            lines[line - 1]
        );
    }
}

#[test]
fn reading_a_document_refuses_unknown_keys_with_a_line() {
    let err = IntentDoc::from_toml("x.toml", "[project]\nname = \"x\"\ncolour = 1\n").unwrap_err();
    assert_eq!(err.problems.len(), 1);
    assert!(err.problems[0].starts_with("x.toml:3"), "{err:?}");
    assert!(err.problems[0].contains("colour"), "{err:?}");
}

#[test]
fn a_named_provider_and_the_defaults_render_as_written() {
    let doc = IntentDoc::from_toml(
        "x.toml",
        "[project]\nname = \"x\"\n\n[auth]\noidc = \"google\"\n",
    )
    .unwrap();
    let rendered = doc.to_toml().unwrap();
    assert!(rendered.contains("oidc = \"google\""), "{rendered}");
    // Defaults are left out, so a minimal document renders minimally.
    assert_eq!(
        IntentDoc::new("x").to_toml().unwrap(),
        "[project]\nname = \"x\"\n"
    );
    let intent = doc.validate("x.toml").unwrap();
    assert!(intent.auth.passwords);
    assert_eq!(
        intent.auth.oidc,
        Some(trestle::intent::Oidc::Named("google".into()))
    );
}

/// Project-level intent behavior comes with a document too: tenancy for the
/// test harness, committed migrations, handlers, roles and hooks.
#[tokio::test]
async fn a_project_from_a_document_serves_like_its_file() {
    const MEMBER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const VIEWER: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let doc = IntentDoc::from_toml("trestle.toml", NOTES).unwrap();
    let app = TestApp::start(
        notes(trestle::project().intent_doc("notes", doc)),
        &format!("member-token={MEMBER},viewer-token={VIEWER}"),
    )
    .await;
    let tenant = app.tenant("North").await;
    app.grant(tenant, MEMBER.parse().unwrap(), "member").await;
    app.grant(tenant, VIEWER.parse().unwrap(), "viewer").await;

    let created = app
        .post("/api/v1/notes", json!({"title": "Hello"}))
        .token("member-token")
        .tenant(tenant)
        .send()
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let id = created.body["id"].as_str().unwrap().to_owned();

    let pinned = app
        .post(&format!("/api/v1/notes/{id}/pin"), json!({}))
        .token("member-token")
        .tenant(tenant)
        .send()
        .await;
    assert_eq!(pinned.status, 200, "{}", pinned.body);
    assert_eq!(pinned.body["pinned"], true);

    let denied = app
        .post("/api/v1/notes", json!({"title": "No"}))
        .token("viewer-token")
        .tenant(tenant)
        .send()
        .await;
    assert_eq!(denied.status, 403, "{}", denied.body);

    let list = app
        .get("/api/v1/notes")
        .token("viewer-token")
        .tenant(tenant)
        .send()
        .await;
    assert_eq!(list.body["items"].as_array().map(Vec::len), Some(1));
    app.finish().await;
}
