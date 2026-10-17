//! M1 (design doc §18): the pilot composes as apps, assembly validates
//! dependencies and references, and operations are inspectable. No database.

use pilot::AppState;
use pilot::audit::AuditApp;
use pilot::lessons::LessonsApp;
use trestle_core::protocol::{Audience, FormPresentation, Widget};
use trestle_core::registry::{App, AppBuilder, Project};

fn issues(project: Project<AppState>) -> Vec<String> {
    project
        .assemble()
        .err()
        .map(|e| e.issues)
        .unwrap_or_default()
}

fn assert_issue(issues: &[String], needle: &str) {
    assert!(
        issues.iter().any(|i| i.contains(needle)),
        "expected an issue containing {needle:?}, got {issues:#?}"
    );
}

/// A second product app that reuses lessons' operation with its own form.
struct LabsApp {
    declare_dependency: bool,
    extra_field: Option<&'static str>,
}

impl App<AppState> for LabsApp {
    fn id(&self) -> &'static str {
        "labs"
    }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        if self.declare_dependency {
            app.requires("lessons");
        }
        let mut form = FormPresentation::for_operation("labs.quick_lesson.form", "lessons.create")
            .title("Quick lesson")
            .field("/title", Widget::Text, "Name")
            .field("/grade_level", Widget::Integer, "Grade");
        if let Some(pointer) = self.extra_field {
            form = form.field(pointer, Widget::Text, "Extra");
        }
        app.form(form);
    }
}

#[test]
fn pilot_assembles_in_dependency_order() {
    let registry = pilot::registry().unwrap();
    let order: Vec<&str> = registry.apps().iter().map(|a| &*a.id).collect();
    assert_eq!(order, ["identity", "schools", "audit", "units", "lessons"]);
    // Installed in reverse, dependencies still come first; install order only
    // breaks ties between apps that are ready at the same time.
    let reversed = Project::<AppState>::new("t", "0")
        .install(LessonsApp)
        .install(pilot::units::UnitsApp)
        .install(AuditApp)
        .install(pilot::schools::SchoolsApp)
        .assemble()
        .unwrap();
    let order: Vec<&str> = reversed.apps().iter().map(|a| &*a.id).collect();
    assert_eq!(order, ["audit", "schools", "units", "lessons"]);
}

#[test]
fn lessons_without_its_dependencies_fails_assembly() {
    let found = issues(Project::new("t", "0").install(LessonsApp));
    assert_issue(
        &found,
        "app lessons requires schools, which is not installed",
    );
    assert_issue(&found, "app lessons requires audit, which is not installed");
}

#[test]
fn a_second_app_is_checked_against_the_first() {
    let valid = pilot::project().install(LabsApp {
        declare_dependency: true,
        extra_field: None,
    });
    let registry = valid.assemble().unwrap();
    assert_eq!(&*registry.apps().last().unwrap().id, "labs");
    assert_eq!(registry.forms().len(), 3);

    let undeclared = pilot::project().install(LabsApp {
        declare_dependency: false,
        extra_field: None,
    });
    assert_issue(
        &issues(undeclared),
        "form labs.quick_lesson.form uses operation lessons.create owned by lessons, which app labs does not require",
    );

    let unknown_field = pilot::project().install(LabsApp {
        declare_dependency: true,
        extra_field: Some("/school_id"),
    });
    assert_issue(
        &issues(unknown_field),
        "/school_id is not a field of input contract lessons.create.input",
    );
}

#[test]
fn operations_are_inspectable() {
    let registry = pilot::registry().unwrap();
    let create = registry.inspect_operation("lessons.create").unwrap();
    assert_eq!(create["mode"], "write");
    assert_eq!(create["policy"], "lessons.create");
    assert_eq!(create["context"]["headers"][0], "Trestle-Tenant");
    assert_eq!(create["http"]["path"], "/api/v1/lessons");
    assert_eq!(create["http"]["location"], "/api/v1/lessons/{id}");
    assert_eq!(create["input"]["id"], "lessons.create.input");
    assert_eq!(create["forms"][0]["id"], "lessons.create.form");

    let read = registry.inspect_operation("lessons.get").unwrap();
    assert_eq!(read["mode"], "read");
    assert_eq!(read["http"]["body"], "path");
    assert!(registry.inspect_operation("lessons.archive").is_none());
}

#[test]
fn published_documents_do_not_leak_policy_or_ownership() {
    let registry = pilot::registry().unwrap();
    for document in [
        registry.openapi(Audience::Product),
        registry.contracts_export(Audience::Product),
        registry.interactions_export(Audience::Product),
    ] {
        let text = document.to_string();
        for secret in [
            "school_id",
            "created_by",
            "x-trestle-policy",
            "lessons.read",
        ] {
            assert!(
                !text.contains(secret),
                "{secret} leaked into a published document"
            );
        }
    }
}
