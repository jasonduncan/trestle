//! Committed snapshots of everything the pilot publishes (design doc §11.1:
//! "snapshot exports, and review schema diffs"), plus the append-only
//! history of interaction revisions that backs open-form compatibility
//! (§11.4).
//!
//! After an intended change: `make snapshots`, then review `git diff`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use trestle_core::protocol::{Audience, ContractDoc, to_pretty};

fn published_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("published")
}

fn updating() -> bool {
    std::env::var_os("UPDATE_SNAPSHOTS").is_some()
}

/// The generated TypeScript client lives where the product app imports it.
#[test]
fn generated_client_matches_the_registry() {
    let registry = pilot::registry().unwrap();
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/product/src/generated/api.ts");
    let fresh = registry.typescript_sdk(Audience::Product);
    if updating() {
        std::fs::write(&path, fresh).unwrap();
    } else {
        assert!(
            std::fs::read_to_string(&path).ok().as_deref() == Some(fresh.as_str()),
            "frontend/product/src/generated/api.ts is stale; run `make snapshots`"
        );
    }
}

#[test]
fn published_documents_match_committed_snapshots() {
    let registry = pilot::registry().unwrap();
    let mut stale = Vec::new();
    for (file, document) in [
        (
            "contracts.json",
            registry.contracts_export(Audience::Product),
        ),
        (
            "interactions.json",
            registry.interactions_export(Audience::Product),
        ),
        ("openapi.json", registry.openapi(Audience::Product)),
        (
            "admin.json",
            serde_json::json!({
                "resources": registry.admin_export(),
                "interactions": registry.interactions_export(Audience::Admin),
                "openapi": registry.openapi(Audience::Admin),
            }),
        ),
    ] {
        let path = published_dir().join(file);
        let fresh = to_pretty(&document);
        if updating() {
            std::fs::write(&path, fresh).unwrap();
        } else if std::fs::read_to_string(&path).ok().as_deref() != Some(fresh.as_str()) {
            stale.push(file);
        }
    }
    assert!(
        stale.is_empty(),
        "published documents changed: {stale:?}. If intended, run `make snapshots` and review the diff."
    );
}

/// `revisions.json` records every interaction revision ever published, with
/// the input contract it submitted against. Entries are only ever added.
#[test]
fn retained_revisions_are_recorded_and_still_compatible() {
    let registry = pilot::registry().unwrap();
    let path = published_dir().join("revisions.json");
    let mut history: Value = std::fs::read_to_string(&path)
        .ok()
        .map(|text| serde_json::from_str(&text).expect("revisions.json is JSON"))
        .unwrap_or_else(|| json!({ "format": "trestle.revisions/1", "interactions": {} }));

    if updating() {
        for published in registry.forms() {
            let op = registry.operation(&published.form.operation).unwrap();
            let entry = history["interactions"]
                .as_object_mut()
                .unwrap()
                .entry(published.form.id.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            entry
                .as_object_mut()
                .unwrap()
                .entry(published.revision.clone())
                .or_insert_with(|| json!({ "operation": op.info.id, "input": op.input.to_ir() }));
        }
        std::fs::write(&path, to_pretty(&history)).unwrap();
    }

    let mut problems = Vec::new();
    for published in registry.forms() {
        let recorded = &history["interactions"][&published.form.id];
        if recorded.get(&published.revision).is_none() {
            problems.push(format!(
                "{} revision {} is not in revisions.json; run `make snapshots`",
                published.form.id, published.revision
            ));
        }
        let current = &registry.operation(&published.form.operation).unwrap().input;
        for retained in &published.retained {
            let Some(old) = recorded.get(retained.revision) else {
                problems.push(format!(
                    "{} retains {}, which was never published",
                    published.form.id, retained.revision
                ));
                continue;
            };
            let old_input = ContractDoc::from_ir(&old["input"]).unwrap();
            for reason in current.rejects_instances_of(&old_input) {
                problems.push(format!(
                    "{} retains {} but the current input contract breaks it: {reason}",
                    published.form.id, retained.revision
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
