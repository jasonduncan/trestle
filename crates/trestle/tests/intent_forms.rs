//! ADR-026: generated forms for `trestle.toml` writes. `forms = true`
//! publishes create, update and action forms whose widgets follow the field
//! types, whose refs pick from the target's list by its label field, and
//! whose descriptors name the renderer features they need. Enum labels
//! (ADR-035) travel in the schemas the select reads. Offline.

use serde_json::{Value, json};

const CRM: &str = r#"
[project]
name = "crm"
trestle = "0.1"

[tenancy]
model = "workspace"

[roles]
member = ["crm.read", "crm.write"]

[[models]]
id = "crm.account"
fields = [{ id = "name", type = "text", max = 100 }]

[[models]]
id = "crm.deal"
fields = [
  { id = "title",     type = "text", max = 100 },
  { id = "amount",    type = "decimal", precision = 10, scale = 2, label = "Value" },
  { id = "account",   type = "ref", to = "crm.account" },
  { id = "stage",     type = "enum", values = ["open", "won"], labels = { won = "Closed won" }, default = "open" },
  { id = "closed_at", type = "instant", optional = true },
  { id = "notes",     type = "text", optional = true },
]

[[endpoints]]
model = "crm.account"
crud = ["list", "get", "create"]
read = "crm.read"
write = "crm.write"

[[endpoints]]
model = "crm.deal"
crud = ["get", "create", "update"]
read = "crm.read"
write = "crm.write"
forms = true

[[endpoints]]
id = "crm.win"
model = "crm.deal"
path = "POST /api/v1/deals/{id}/win"
permission = "crm.write"
set = { stage = "won", closed_at = "now" }
accept = ["notes"]
forms = true
"#;

fn registry(text: &str) -> trestle::core::registry::Registry<trestle::State> {
    trestle::project()
        .intent_str("trestle.toml", text)
        .assemble()
        .unwrap()
}

fn children(descriptor: &Value) -> Vec<(String, String, String)> {
    descriptor["presentation"]["layout"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["pointer"].as_str().unwrap().to_owned(),
                c["widget"].as_str().unwrap().to_owned(),
                c["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn features(descriptor: &Value) -> Vec<String> {
    descriptor["requiredFeatures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn create_update_and_action_forms_follow_the_field_types() {
    let registry = registry(CRM);
    let create = &registry
        .form("crm.deal.create.form")
        .expect("published")
        .descriptor;
    assert_eq!(create["presentation"]["title"], "New deal");
    assert_eq!(create["presentation"]["submit"]["label"], "Create");
    let row = |p: &str, w: &str, l: &str| (p.to_owned(), w.to_owned(), l.to_owned());
    assert_eq!(
        children(create),
        [
            row("/title", "text", "Title"),
            row("/amount", "decimal", "Value"),
            row("/account", "relation", "Account"),
            row("/stage", "select", "Stage"),
            row("/closed_at", "datetime", "Closed at"),
            row("/notes", "textarea", "Notes"),
        ]
    );
    let account = &create["presentation"]["layout"]["children"][2]["options"];
    assert_eq!(
        account,
        &json!({ "operation": "crm.account.list", "path": "/api/v1/accounts", "value": "id", "label": "name" })
    );
    // The select shows the label and submits the value; the representation
    // (what views read) carries the same labels.
    for schema in [&create["input"]["schema"], &create["output"]["schema"]] {
        let stage = &schema["properties"]["stage"];
        assert_eq!(stage["enum"], json!(["open", "won"]));
        assert_eq!(
            stage["x-trestle-enum-labels"],
            json!({ "won": "Closed won" })
        );
    }
    let needed = features(create);
    for f in ["forms.relation@1", "forms.decimal@1", "forms.temporal@1"] {
        assert!(needed.iter().any(|n| n == f), "{f} in {needed:?}");
    }

    let update = &registry.form("crm.deal.update.form").unwrap().descriptor;
    assert_eq!(update["presentation"]["title"], "Edit deal");
    assert_eq!(children(update).len(), 6);
    assert!(
        features(update).iter().any(|f| f == "http.precondition@1"),
        "edits send If-Match"
    );

    let win = &registry.form("crm.win.form").unwrap().descriptor;
    assert_eq!(win["presentation"]["title"], "Win deal");
    assert_eq!(win["presentation"]["submit"]["label"], "Win");
    assert_eq!(children(win), [row("/notes", "textarea", "Notes")]);

    assert!(
        registry.form("crm.account.create.form").is_none(),
        "forms are opt-in per endpoint"
    );
}

#[test]
fn forms_need_a_widget_for_every_field() {
    let problems = |from: &str, to: &str| {
        let text = CRM.replace(from, to);
        assert_ne!(text, CRM, "replacement applies: {from}");
        trestle::intent::parse("trestle.toml", &text)
            .err()
            .map(|e| e.problems)
            .unwrap_or_default()
    };
    let has = |p: Vec<String>, text: &str| {
        let found = p.iter().any(|m| m.contains(text));
        if !found {
            eprintln!("{p:#?}");
        }
        found
    };
    assert!(has(
        problems(
            r#"{ id = "title",     type = "text", max = 100 },"#,
            r#"{ id = "title", type = "text", max = 100 },
  { id = "owner", type = "uuid" },"#
        ),
        "cannot show uuid field owner"
    ));
    assert!(has(
        problems(
            r#"crud = ["list", "get", "create"]"#,
            r#"crud = ["get", "create"]"#
        ),
        "picks account from crm.account, which needs a list endpoint"
    ));
    assert!(has(
        problems(
            r#"fields = [{ id = "name", type = "text", max = 100 }]"#,
            r#"label = "size"
fields = [{ id = "name", type = "text", max = 100 }, { id = "size", type = "int" }]"#
        ),
        "label \"size\" must name a text field"
    ));
}

/// The browser renderer decodes this exact descriptor
/// (`frontend/interaction-react/test/intent-forms.test.ts`). Regenerate with
/// `TRESTLE_BLESS=1 cargo test -p trestle-framework --test intent_forms`.
#[test]
fn the_create_form_matches_the_fixture_the_renderer_reads() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/interactions/intent-crm-deal-create.json"
    );
    let descriptor = registry(CRM)
        .form("crm.deal.create.form")
        .unwrap()
        .descriptor
        .clone();
    let text = serde_json::to_string_pretty(&descriptor).unwrap() + "\n";
    if std::env::var_os("TRESTLE_BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &text).unwrap();
    }
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(
        committed, text,
        "the fixture is stale; rerun with TRESTLE_BLESS=1 and review the diff"
    );
}
