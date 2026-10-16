//! M3 (design doc §18): model metadata and historical evolution on the pilot.

mod support;

use pilot::AppState;
use pilot::dev::NORTHSIDE;
use pilot::lessons::repository::{
    self, GRADE_LEVEL, ID, Lessons, SCHOOL_ID, STATUS, SUBTITLE, TITLE,
};
use pilot::schools::SchoolId;
use serde_json::json;
use support::{TestEnv, post_json, send};
use trestle_core::protocol::{Audience, FormPresentation, Widget};
use trestle_core::query::Table;
use trestle_core::registry::{App, AppBuilder};
use trestle_historical::artifact;
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::{Check, ColumnType, FieldState};
use trestle_historical::testing::TestDatabase;
use trestle_historical::verify::verify;
use trestle_historical::{Migration, MigrationSet, migrate};

#[test]
fn models_match_committed_history() {
    let registry = pilot::registry().unwrap();
    let (_, plan) = pilot::migration_plan(&registry, PlanOptions::default()).unwrap();
    assert!(plan.is_empty(), "run `pilot makemigrations`: {plan:#?}");
}

/// Presentation only: a second form with different labels for lessons.create.
struct RelabelApp;

impl App<AppState> for RelabelApp {
    fn id(&self) -> &'static str {
        "relabel"
    }

    fn register(&self, app: &mut AppBuilder<AppState>) {
        app.requires("lessons");
        app.form(
            FormPresentation::for_operation("relabel.lesson.form", "lessons.create")
                .title("New unit")
                .field("/title", Widget::Textarea, "Unit name")
                .field("/grade_level", Widget::Integer, "Year"),
        );
    }
}

#[test]
fn presentation_changes_create_no_migration() {
    let base = pilot::registry().unwrap();
    let relabeled = pilot::project().install(RelabelApp).assemble().unwrap();
    assert_ne!(
        base.fingerprints(Audience::Product)["interactions"],
        relabeled.fingerprints(Audience::Product)["interactions"],
        "the presentation really changed"
    );
    assert_eq!(base.desired_schema(), relabeled.desired_schema());
    let (_, plan) = pilot::migration_plan(&relabeled, PlanOptions::default()).unwrap();
    assert!(plan.is_empty());
}

#[test]
fn destructive_changes_block_generation() {
    let registry = pilot::registry().unwrap();
    let set = pilot_migrations::history().unwrap();
    let mut desired = registry.desired_schema().clone();
    desired
        .models
        .get_mut("lessons.lesson")
        .unwrap()
        .fields
        .retain(|f| f.id != "lessons.lesson.subtitle");
    let plan = diff(&set.history().unwrap(), &desired, &PlanOptions::default());
    assert!(plan.changes.is_empty());
    assert!(
        plan.blocked[0].contains("lessons.lesson.subtitle was removed"),
        "{:?}",
        plan.blocked
    );
    let err = artifact::generate(&set, &plan, "drop_subtitle").unwrap_err();
    assert!(err.to_string().contains("the plan is blocked"));
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

#[tokio::test]
async fn generated_migration_applies_on_top_of_committed_history_and_verifies() {
    let registry = pilot::registry().unwrap();
    let set = pilot_migrations::history().unwrap();
    let history = set.history().unwrap();

    // Release N+1 adds an optional summary to lessons.
    let mut desired = registry.desired_schema().clone();
    desired
        .models
        .get_mut("lessons.lesson")
        .unwrap()
        .fields
        .push(FieldState {
            id: "lessons.lesson.summary".into(),
            column: "summary".into(),
            ty: ColumnType::Text,
            nullable: true,
            primary_key: false,
            check: Some(Check::Chars { min: 1, max: 500 }),
            check_name: None,
            references: None,
            default: None,
        });
    let plan = diff(&history, &desired, &PlanOptions::default());
    let artifacts = artifact::generate(&set, &plan, "lesson_summary").unwrap();
    assert_eq!(artifacts.len(), 1);
    let generated = &artifacts[0];
    assert_eq!(generated.manifest.id, "0005_lesson_summary");
    assert_eq!(generated.manifest.depends_on, ["lessons.0004_duration"]);
    assert!(generated.sql.contains(
        "ALTER TABLE lessons ADD COLUMN summary text CONSTRAINT lessons_summary_check CHECK (char_length(summary) BETWEEN 1 AND 500);"
    ));

    // Written artifacts are what a developer reviews and commits.
    let dir = std::env::temp_dir().join(format!("trestle-artifacts-{}", std::process::id()));
    let written = artifact::write(&dir, &artifacts).unwrap();
    assert_eq!(written.len(), 2);
    assert!(
        artifact::write(&dir, &artifacts).is_err(),
        "never overwrites"
    );
    let _ = std::fs::remove_dir_all(&dir);

    let mut all: Vec<Migration> = set.iter().cloned().collect();
    all.push(Migration {
        app: "lessons",
        id: "0005_lesson_summary",
        depends_on: &[pilot_migrations::LESSONS_0004],
        sql: leak(generated.sql.clone()),
        manifest: Some(leak(generated.manifest_json())),
        data: None,
    });
    let next = MigrationSet::ordered(all).unwrap();
    assert!(diff(&next.history().unwrap(), &desired, &PlanOptions::default()).is_empty());

    let db = TestDatabase::create().await;
    migrate(db.options(), &next).await.unwrap();
    let mut conn = db.connect().await;
    let report = verify(&mut conn, &next.history().unwrap()).await.unwrap();
    assert!(report.ok(), "{:#?}", report.mismatches);
    drop(conn);
    db.drop().await;
}

#[test]
fn typed_columns_match_the_model() {
    let registry = pilot::registry().unwrap();
    let model = registry.desired_schema().model(Lessons::MODEL).unwrap();
    assert_eq!(model.table, Lessons::TABLE);
    let columns: Vec<&str> = model.fields.iter().map(|f| f.column.as_str()).collect();
    for column in Lessons::COLUMNS.iter().copied().chain([
        ID.name(),
        SCHOOL_ID.name(),
        TITLE.name(),
        GRADE_LEVEL.name(),
        STATUS.name(),
        SUBTITLE.name(),
    ]) {
        assert!(
            columns.contains(&column),
            "{column} is not a column of {}",
            Lessons::MODEL
        );
    }
    assert_eq!(model.primary_key()[0].column, Lessons::PRIMARY_KEY);
}

#[tokio::test]
async fn scoped_list_query_filters_orders_and_bounds() {
    let env = TestEnv::new().await;
    let registry = pilot::registry().unwrap();
    let app = pilot::router(&registry, env.state.clone());
    for (title, grade) in [("Ratios", 6), ("Area", 6), ("Fractions", 4)] {
        let reply = send(
            &app,
            post_json(
                "/api/v1/lessons",
                "alice",
                NORTHSIDE,
                &json!({"title": title, "grade_level": grade}),
            ),
        )
        .await;
        assert_eq!(reply.status, 201);
    }
    let bob = send(
        &app,
        post_json(
            "/api/v1/lessons",
            "bob",
            pilot::dev::SOUTHSIDE,
            &json!({"title": "Other school", "grade_level": 6}),
        ),
    )
    .await;
    assert_eq!(bob.status, 201);

    let mut conn = env.pool.acquire().await.unwrap();
    let sixth_grade = repository::ListFilters {
        grade_level: Some(6),
        ..Default::default()
    };
    let sixth = repository::list(&mut conn, SchoolId(NORTHSIDE), &sixth_grade, None, 10)
        .await
        .unwrap();
    let titles: Vec<&str> = sixth.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Area", "Ratios"],
        "scoped to Northside, filtered, ordered"
    );
    let first = repository::list(&mut conn, SchoolId(NORTHSIDE), &Default::default(), None, 1)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    drop(conn);
    env.finish().await;
}
