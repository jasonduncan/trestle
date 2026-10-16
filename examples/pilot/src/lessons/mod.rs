//! Lessons.
//!
//! * `contracts`  — wire input/output types bound to their contracts
//! * `repository` — explicit, tenant-scoped SQL on an explicit connection
//! * `policy`     — who may do what, admitted and rechecked by the executor
//! * `operations` — the behavior
//! * `export`     — a hand-written Axum route that reuses the operations
//! * this module  — the app: what lessons registers and publishes

pub mod contracts;
mod export;
pub mod operations;
pub mod policy;
pub mod repository;

use axum::routing::get;
use http::Method;
use trestle_core::protocol::{Audience, FormPresentation, Widget};
use trestle_core::registry::{AdminView, App, AppBuilder};
use trestle_historical::schema::{Column, Model};

use crate::AppState;
use crate::schools::AdminAccess;
use operations::{CreateLesson, DeleteLesson, GetLesson, ListLessons, PatchLesson, PublishLesson};

pub struct LessonsApp;

impl App<AppState> for LessonsApp {
    fn id(&self) -> &'static str {
        "lessons"
    }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        app.requires("schools").requires("audit").requires("units");
        app.schema(pilot_migrations::LESSONS_0001)
            .schema(pilot_migrations::LESSONS_0002)
            .schema(pilot_migrations::LESSONS_0003)
            .schema(pilot_migrations::LESSONS_0004)
            .model(lesson_model());
        app.resource("lessons.lessons", "/api/v1/lessons")
            .list(ListLessons)
            .create(CreateLesson)
            .retrieve(GetLesson)
            .update(PatchLesson)
            .delete(DeleteLesson)
            .action("publish", PublishLesson)
            .admin(
                AdminView::new("Lessons", "title")
                    .columns(&["title", "grade_level", "status", "version"])
                    .filters(&["grade_level", "status"])
                    .edit_form("lessons.admin_edit.form")
                    .action_when("publish", "Publish", "status", serde_json::json!("draft")),
                AdminAccess,
            );
        app.form(create_form())
            .retain(
                "r-acbd164ff4e78da6",
                "M2 release A form (no subtitle). Later fields are optional, so its submissions mean the same thing.",
            )
            .retain(
                "r-cab979f086c703f7",
                "M2 release B form (subtitle, no unit). unit_id is optional, so its submissions mean the same thing.",
            )
            .retain(
                "r-b68fa979090594a2",
                "M3/M4 form (unit, no duration). duration_minutes is optional, so its submissions mean the same thing.",
            );
        app.form(admin_edit_form()).retain(
            "r-9adfca22d27dfc4e",
            "M4 admin edit form (no duration). A PATCH without duration_minutes leaves it unchanged.",
        );
        app.unmanaged(
            Method::GET,
            "/api/v1/lessons/export.csv",
            get(export::csv),
            "CSV download for spreadsheets. Hand-written, but every row comes from lessons.list through the executor. Reviewed 2026-09-26.",
        );
    }
}

/// Persistence for lessons. Separate from the wire contracts in
/// `contracts` (§5.1): a column's existence never makes it writable.
pub fn lesson_model() -> Model {
    Model::new("lessons.lesson")
        .table("lessons")
        .field(Column::uuid("id").primary_key())
        .field(Column::uuid("school_id").references("schools.school"))
        .field(Column::text("title").chars(1, 200))
        .field(Column::small_int("grade_level").range(1, 12))
        .field(
            Column::text("status")
                .one_of(&["draft", "published"])
                .default_text("draft"),
        )
        .field(Column::big_int("version").at_least(1).default_int(1))
        .field(Column::uuid("created_by"))
        .field(Column::timestamptz("created_at").default_now())
        .field(Column::timestamptz("updated_at").default_now())
        .field(Column::text("subtitle").nullable().chars(1, 200))
        .field(Column::uuid("unit_id").nullable().references("units.unit"))
        .field(
            Column::small_int("duration_minutes")
                .nullable()
                .range(1, 600),
        )
        .unique("lessons_school_title_key", &["school_id", "title"])
}

/// Hand-authored presentation for `lessons.create`. Labels and widgets only:
/// the input contract and policy stay with the operation.
pub fn create_form() -> FormPresentation {
    FormPresentation::for_operation("lessons.create.form", "lessons.create")
        .title("Create lesson")
        .field("/title", Widget::Text, "Title")
        .field("/grade_level", Widget::Integer, "Grade level")
        .help("1 to 12.")
        .field("/subtitle", Widget::Text, "Subtitle")
        .relation("/unit_id", "Unit", "units.list", "name")
        .field("/duration_minutes", Widget::Integer, "Duration (minutes)")
        .help("1 to 600.")
        .submit("Create lesson")
}

/// Admin edit form for `lessons.update`. Status and version are not here:
/// status changes only through the publish action.
pub fn admin_edit_form() -> FormPresentation {
    FormPresentation::for_operation("lessons.admin_edit.form", "lessons.update")
        .audience(Audience::Admin)
        .title("Edit lesson")
        .field("/title", Widget::Text, "Title")
        .field("/grade_level", Widget::Integer, "Grade level")
        .field("/subtitle", Widget::Text, "Subtitle")
        .relation("/unit_id", "Unit", "units.list", "name")
        .field("/duration_minutes", Widget::Integer, "Duration (minutes)")
        .submit("Save changes")
}
