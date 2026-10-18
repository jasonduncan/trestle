//! F2: migrations read at runtime (or embedded), data steps registered by
//! name, and `check` validating both.

use std::path::{Path, PathBuf};

use trestle::historical::data::DataFuture;
use trestle::historical::testing::TestDatabase;
use trestle::historical::{DataStep, HistoricalTx};
use trestle::prelude::*;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/migrations");

struct Notes {
    with_step: bool,
}

impl App for Notes {
    fn id(&self) -> &'static str {
        "notes"
    }

    fn register(&self, app: &mut AppBuilder) {
        app.model(
            Model::new("notes.note")
                .table("notes")
                .field(Field::uuid("id").primary_key())
                .field(Field::text("title").chars(1, 200)),
        );
        if self.with_step {
            app.data_step("seed_notes", seed_notes);
        }
    }
}

fn run_seed<'a>(_tx: &'a mut HistoricalTx<'_>) -> DataFuture<'a> {
    Box::pin(async { Ok(()) })
}

fn seed_notes() -> DataStep {
    DataStep {
        name: "seed_notes",
        source: include_str!("migrations.rs"),
        run: trestle::historical::DataRun::Once(run_seed),
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trestle-f2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn copy_fixtures(to: &Path) {
    let from = Path::new(FIXTURES).join("notes");
    std::fs::create_dir_all(to.join("notes")).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join("notes").join(entry.file_name())).unwrap();
    }
}

fn project(dir: &Path, with_step: bool) -> trestle::Project {
    trestle::project()
        .install(Notes { with_step })
        .named("Notes", "1.0.0")
        .migrations_dir(dir)
}

fn run(project: impl Fn() -> trestle::Project, args: &[&str]) -> Result<(), trestle::Failure> {
    let mut argv = vec!["notes"];
    argv.extend_from_slice(args);
    let matches = project().command().try_get_matches_from(argv).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(project().run_matches(&matches))
}

#[test]
fn check_requires_migrations_for_every_model() {
    let dir = scratch("check");
    let err = run(|| project(&dir, false), &["check"]).unwrap_err();
    assert!(err.contains("run `"), "{err}");
    assert!(err.contains("makemigrations"), "{err}");
    run(
        || project(&dir, false),
        &["makemigrations", "--name", "initial"],
    )
    .unwrap();
    run(|| project(&dir, false), &["check"]).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn data_steps_are_linked_by_name_and_checked() {
    let dir = scratch("steps");
    copy_fixtures(&dir);

    // Registered but unused.
    let err = run(|| project(&dir, true), &["check"]).unwrap_err();
    assert!(
        err.contains("\"seed_notes\" is registered but no migration uses it"),
        "{err}"
    );

    run(
        || project(&dir, true),
        &[
            "makemigrations",
            "--empty",
            "--app",
            "notes",
            "--data",
            "seed_notes",
            "--name",
            "seed",
        ],
    )
    .unwrap();
    run(|| project(&dir, true), &["check"]).unwrap();

    // Used but not registered.
    let err = run(|| project(&dir, false), &["check"]).unwrap_err();
    assert!(
        err.contains("needs data step \"seed_notes\"; register it with app.data_step"),
        "{err}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_step_must_be_registered_under_its_own_name() {
    struct Misnamed;
    impl App for Misnamed {
        fn id(&self) -> &'static str {
            "misnamed"
        }
        fn register(&self, app: &mut AppBuilder) {
            app.data_step("other_name", seed_notes);
        }
    }
    let err = trestle::project()
        .install(Misnamed)
        .assemble()
        .err()
        .unwrap();
    assert!(
        err.to_string().contains("calls itself \"seed_notes\""),
        "{err}"
    );
}

#[test]
fn embedded_migrations_match_the_directory() {
    let files = trestle::embed_migrations!("$CARGO_MANIFEST_DIR/tests/fixtures/migrations");
    assert_eq!(
        files,
        trestle::historical::load::read_dir(Path::new(FIXTURES)).unwrap()
    );
    let embedded = || project(Path::new(FIXTURES), false).embedded_migrations(files.clone());
    run(embedded, &["check"]).unwrap();
}

#[test]
fn a_stale_embedded_copy_fails_check() {
    let dir = scratch("stale");
    copy_fixtures(&dir);
    let files = trestle::embed_migrations!("$CARGO_MANIFEST_DIR/tests/fixtures/migrations");
    let sql = dir.join("notes/0001_initial.sql");
    let text = std::fs::read_to_string(&sql).unwrap();
    std::fs::write(&sql, format!("{text}-- edited\n")).unwrap();
    let err = run(
        || project(&dir, false).embedded_migrations(files.clone()),
        &["check"],
    )
    .unwrap_err();
    assert!(err.contains("embedded in this binary differ"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn runtime_migrations_apply_and_verify() {
    let db = TestDatabase::create().await;
    // The admin URL with the test database swapped in, so credentials
    // (a password, sslmode) carry over.
    let admin = std::env::var("TRESTLE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".to_owned());
    let (base, query) = admin.split_once('?').unwrap_or((&admin, ""));
    let (server, _) = base.rsplit_once('/').expect("a database URL has a path");
    let mut url = format!("{server}/{}", db.options().get_database().unwrap());
    if !query.is_empty() {
        url = format!("{url}?{query}");
    }
    let dir = PathBuf::from(FIXTURES);
    let project = || project(&dir, false);
    let args = |cmd: &[&'static str]| {
        let mut v = vec!["notes", "--database-url"];
        v.push(Box::leak(url.clone().into_boxed_str()));
        v.extend_from_slice(cmd);
        v
    };
    for cmd in [&["migrate"][..], &["migrate"], &["schema", "verify"]] {
        let matches = project().command().try_get_matches_from(args(cmd)).unwrap();
        project()
            .run_matches(&matches)
            .await
            .unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    }
    db.drop().await;
}
