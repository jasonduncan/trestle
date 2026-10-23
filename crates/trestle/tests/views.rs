#[path = "support/views_app.rs"]
mod views_app;
// Cross-language fixtures are exported from actual registrations, never
// authored JSON Schemas. UPDATE_VIEW_FIXTURES=1 deliberately refreshes them.
use std::path::PathBuf;
use trestle::protocol::{Audience, to_pretty};

const SUPPORT: &str = include_str!("../../../fixtures/views/support.toml");
const MAINTENANCE: &str = include_str!("../../../fixtures/views/maintenance.toml");

#[test]
fn registered_views_match_shared_fixtures_and_round_trip() {
    for (name, intent) in [("support", SUPPORT), ("maintenance", MAINTENANCE)] {
        let registry = views_app::project(name, intent)
            .assemble()
            .unwrap_or_else(|e| panic!("{e}"));
        let doc = serde_json::json!({
            "admin": registry.views_export(Audience::Admin),
            "product": registry.views_export(Audience::Product),
            "adminForms": registry.interactions_export(Audience::Admin),
            "productForms": registry.interactions_export(Audience::Product),
        });
        for view in registry.views() {
            let wire = serde_json::to_vec(&view.document).unwrap();
            let decoded: trestle::protocol::view::ViewDoc = serde_json::from_slice(&wire).unwrap();
            decoded.validate().unwrap();
            assert_eq!(view.document, decoded);
            assert!(!String::from_utf8(wire).unwrap().contains("policy"));
        }
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../fixtures/views/{name}.json"));
        if std::env::var_os("UPDATE_VIEW_FIXTURES").is_some() {
            std::fs::write(&path, to_pretty(&doc)).unwrap();
        }
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(doc, expected);
    }
}

#[test]
fn invalid_view_bindings_fail_before_any_database_is_needed() {
    for (from, to, expected) in [
        (
            "columns = [\"subject\", \"state\", \"assignee\"]",
            "columns = [\"private_field\"]",
            "column",
        ),
        (
            "query = \"support.case.list\"",
            "query = \"support.case.create\"",
            "published read",
        ),
        (
            "filters = [\"state\", \"q\"]\nrecord_view",
            "filters = [\"private_field\"]\nrecord_view",
            "filter",
        ),
        (
            "record_view = \"support.case.detail\"",
            "record_view = \"support.missing\"",
            "missing record",
        ),
    ] {
        let changed = SUPPORT.replace(from, to);
        assert_ne!(changed, SUPPORT);
        let err = trestle::project()
            .intent_str("invalid", &changed)
            .assemble()
            .err()
            .expect("assembly must fail");
        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn installing_only_models_exposes_no_views() {
    let source =
        "[project]\nname='notes'\n[[models]]\nid='notes.note'\nfields=[{id='title',type='text'}]";
    let registry = trestle::project()
        .intent_str("only-models", source)
        .assemble()
        .unwrap();
    assert!(registry.views().is_empty());
    assert!(registry.forms().is_empty());
    assert!(registry.operations().is_empty());
}

#[test]
fn presentation_overrides_bind_stable_ids_and_refuse_removed_targets() {
    use trestle::core::registry::{App, AppBuilder, PresentationOverride};
    struct Customization(&'static str);
    impl App<trestle::State> for Customization {
        fn id(&self) -> &str {
            "customization"
        }
        fn register(&self, app: &mut AppBuilder<trestle::State>) {
            app.presentation_override(PresentationOverride {
                view: "support.case.collection".into(),
                title: Some("Our case queue".into()),
                columns: Some(vec![self.0.into()]),
                labels: [(self.0.into(), "Case title".into())].into(),
            });
        }
    }
    for source in [
        SUPPORT.to_owned(),
        SUPPORT.replace("title = \"Cases\"", "title = \"Cases v2\""),
    ] {
        let registry = views_app::project("support", &source)
            .install_app(Customization("subject"))
            .assemble()
            .unwrap();
        let view = &registry
            .views()
            .iter()
            .find(|v| v.document.view_id == "support.case.collection")
            .unwrap()
            .document;
        assert_eq!(view.title, "Our case queue");
        assert_eq!(view.columns.len(), 1);
        assert_eq!(view.columns[0].label, "Case title");
        assert_eq!(view.columns[0].pointer, "/subject");
    }
    let failure = views_app::project("support", SUPPORT)
        .install_app(Customization("removed"))
        .assemble()
        .err()
        .unwrap();
    assert!(failure.to_string().contains("missing"));
}

#[test]
fn renderer_pairing_checks_features_required_components_and_input_contracts() {
    use trestle::protocol::view::RendererManifest;
    let registry = views_app::project("support", SUPPORT).assemble().unwrap();
    let mut renderer = RendererManifest {
        protocol: "trestle.renderer".into(),
        protocol_version: "1.0".into(),
        view_versions: vec!["1.0".into()],
        interaction_versions: vec!["1.0".into(), "1.1".into()],
        features: registry
            .views()
            .iter()
            .flat_map(|v| v.document.required_features.clone())
            .chain(registry.forms().iter().flat_map(|f| {
                f.descriptor["requiredFeatures"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_owned())
            }))
            .collect(),
        components: [
            ("fixture.account_summary".into(), "support.account@1".into()),
            ("fixture.export_panel".into(), "support.case@1".into()),
        ]
        .into(),
        digest: "0".repeat(64),
    };
    assert!(registry.check_renderer(&renderer).is_empty());
    renderer.components.remove("fixture.export_panel");
    assert!(
        registry
            .check_renderer(&renderer)
            .iter()
            .any(|e| e.contains("missing component"))
    );
    renderer
        .components
        .insert("fixture.export_panel".into(), "wrong.input@1".into());
    assert!(
        registry
            .check_renderer(&renderer)
            .iter()
            .any(|e| e.contains("contract mismatch"))
    );
    renderer.features.clear();
    assert!(
        registry
            .check_renderer(&renderer)
            .iter()
            .any(|e| e.contains("requires"))
    );
}

#[test]
fn a_completeness_flag_is_a_required_boolean_of_a_collection() {
    let registry = views_app::project("support", SUPPORT).assemble().unwrap();
    let recent = &registry
        .views()
        .iter()
        .find(|v| v.document.view_id == "support_work.recent")
        .unwrap()
        .document;
    assert_eq!(recent.source.complete_pointer.as_deref(), Some("/complete"));
    assert!(
        recent
            .required_features
            .contains(&"views.completeness@1".to_owned())
    );
    let queue = &registry
        .views()
        .iter()
        .find(|v| v.document.view_id == "support_work.queue")
        .unwrap()
        .document;
    assert_eq!(queue.source.complete_pointer, None);
    assert!(
        !serde_json::to_string(queue)
            .unwrap()
            .contains("completePointer"),
        "a view without a flag keeps its bytes"
    );

    // Not a boolean of the page, or not a collection: refused at assembly.
    use std::sync::Arc;
    use trestle::core::registry::{App, AppBuilder, ViewDefinition};
    use trestle::intent::{Permission, Resolved};
    struct Flag(Arc<Resolved>, &'static str, bool);
    impl App<trestle::State> for Flag {
        fn id(&self) -> &str {
            "flag"
        }
        fn register(&self, app: &mut AppBuilder<trestle::State>) {
            app.requires("support_work");
            let view = if self.2 {
                ViewDefinition::collection("flag.recent", "support_work.recent")
            } else {
                ViewDefinition::record("flag.recent", "support_work.status").label("state")
            };
            let permission = Permission::new(&self.0, "support.read").unwrap();
            app.view_dyn(view.complete(self.1), Arc::new(permission));
        }
    }
    for (flag, collection) in [("items", true), ("missing", true), ("state", false)] {
        let err = views_app::project("support", SUPPORT)
            .intent_app(move |resolved| Flag(resolved.clone(), flag, collection))
            .assemble()
            .err()
            .expect("assembly must fail");
        assert!(err.to_string().contains("completeness flag"), "{err}");
    }
}
