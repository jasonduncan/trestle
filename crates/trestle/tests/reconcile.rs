//! F6: the reconciler contract (design §7) end to end against Postgres:
//! plan JSON and classification, production gating, approval by digest,
//! verify, and the stable exit codes.

use std::path::{Path, PathBuf};

use sqlx::Connection;
use trestle::exit;
use trestle::historical::load;
use trestle::historical::testing::TestDatabase;
use trestle::reconcile;

const NOTES: &str = include_str!("../examples/notes_intent/trestle.toml");
const EXAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/examples/notes_intent/migrations"
);

async fn pin(
    ctx: trestle::intent::Ctx,
    input: trestle::intent::Input,
) -> trestle::Result<trestle::intent::Row> {
    ctx.update(
        "notes.note",
        input.id()?,
        serde_json::json!({ "pinned": true }),
    )
    .await
}
async fn tidy(
    _: trestle::intent::Ctx,
    row: trestle::intent::Row,
) -> trestle::Result<trestle::intent::Row> {
    Ok(row)
}

/// A copy of the example's migrations that tests can add to.
fn migrations() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trestle-reconcile-{}", uuid::Uuid::new_v4()));
    for app in ["notes", "workspace"] {
        std::fs::create_dir_all(dir.join(app)).unwrap();
        for f in std::fs::read_dir(Path::new(EXAMPLE).join(app)).unwrap() {
            let f = f.unwrap();
            std::fs::copy(f.path(), dir.join(app).join(f.file_name())).unwrap();
        }
    }
    dir
}

fn add(dir: &Path, id: &str, extra: &str, sql: &str) {
    std::fs::write(
        dir.join(format!("notes/{id}.json")),
        format!(
            r#"{{"format": 2, "app": "notes", "id": "{id}", "depends_on": ["notes.0001_initial"], "operations": []{extra}}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join(format!("notes/{id}.sql")), sql).unwrap();
}

struct Env {
    db: TestDatabase,
    dir: PathBuf,
    url: String,
}

impl Env {
    async fn new() -> Self {
        let db = TestDatabase::create().await;
        let base = std::env::var("TRESTLE_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://localhost/postgres".into());
        let name = db.options().get_database().unwrap().to_owned();
        let url = match base.rsplit_once('/') {
            Some((prefix, _)) => format!("{prefix}/{name}"),
            None => unreachable!(),
        };
        Env {
            db,
            dir: migrations(),
            url,
        }
    }

    fn project(&self) -> trestle::Project {
        trestle::project()
            .intent_str("trestle.toml", NOTES)
            .handler("notes::pin", pin)
            .hook("notes::tidy", tidy)
            .migrations_dir(&self.dir)
    }

    async fn run(&self, args: &[&str]) -> Result<(), trestle::Failure> {
        let mut argv = vec!["notes", "--database-url", &self.url];
        argv.extend_from_slice(args);
        let matches = self.project().command().try_get_matches_from(argv).unwrap();
        self.project().run_matches(&matches).await
    }

    async fn code(&self, args: &[&str]) -> u8 {
        match self.run(args).await {
            Ok(()) => exit::OK,
            Err(f) => f.code,
        }
    }

    async fn plan(&self) -> reconcile::PlanReport {
        let set = load::history_from_dir(&self.dir, &[]).unwrap();
        let mut conn = self.db.connect().await;
        reconcile::plan_report(&mut conn, &set, "notes")
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn generated_migrations_auto_apply_in_production() {
    let env = Env::new().await;
    let plan = env.plan().await;
    assert_eq!(plan.schema, "trestle.plan/1");
    assert_eq!(plan.database["ledger_present"], false);
    assert_eq!(plan.steps.len(), 2);
    assert!(
        plan.steps
            .iter()
            .all(|s| s.class == "auto" && s.risk == "additive")
    );
    assert_eq!(plan.class, "auto");
    assert!(plan.plan_digest.starts_with("sha256:"));
    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(json["steps"][0]["reasons"], serde_json::json!([]));

    assert_eq!(
        env.code(&["--env", "production", "migrate", "--auto"])
            .await,
        exit::OK
    );
    assert_eq!(env.code(&["verify"]).await, exit::OK);
    assert!(env.plan().await.steps.is_empty());
    env.db.drop().await;
}

#[tokio::test]
async fn approval_is_by_exact_plan_digest() {
    let env = Env::new().await;
    env.run(&["migrate"]).await.unwrap();
    add(
        &env.dir,
        "0002_title_idx",
        r#", "atomic": false"#,
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS notes_title_idx ON notes (title);\n",
    );
    let plan = env.plan().await;
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].reasons, ["non_atomic", "hand_sql"]);
    assert!(plan.requires_approval);

    // Production refuses without approval; so does --auto anywhere.
    let refused = env
        .run(&["--env", "production", "migrate"])
        .await
        .unwrap_err();
    assert_eq!(refused.code, exit::APPROVAL);
    assert!(refused.contains(&plan.plan_digest), "{refused}");
    assert_eq!(env.code(&["migrate", "--auto"]).await, exit::APPROVAL);
    assert_eq!(
        env.code(&["--env", "production", "migrate", "--approve", "sha256:0000"])
            .await,
        exit::APPROVAL
    );

    let approve = plan.plan_digest.as_str();
    env.run(&[
        "--env",
        "production",
        "migrate",
        "--approve",
        approve,
        "--backup-ref",
        "snap-42",
        "--json",
    ])
    .await
    .unwrap();
    let mut conn = env.db.connect().await;
    let backup: Option<String> =
        sqlx::query_scalar("SELECT backup_ref FROM trestle_migrations WHERE id = '0002_title_idx'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(backup.as_deref(), Some("snap-42"));
    let _ = conn.close().await;
    env.db.drop().await;
}

#[tokio::test]
async fn exit_codes_are_the_contract() {
    let env = Env::new().await;
    env.run(&["migrate"]).await.unwrap();

    // 4: an atomic migration failed and rolled back.
    add(&env.dir, "0002_broken", "", "SELECT 1 / 0;\n");
    assert_eq!(env.code(&["migrate"]).await, exit::FAILED);
    std::fs::remove_file(env.dir.join("notes/0002_broken.json")).unwrap();
    std::fs::remove_file(env.dir.join("notes/0002_broken.sql")).unwrap();

    // 5: a non-atomic migration stopped part-way; verify reports it (6).
    let mut conn = env.db.connect().await;
    sqlx::query("DELETE FROM trestle_migrations WHERE id = '0002_broken'")
        .execute(&mut conn)
        .await
        .unwrap();
    add(
        &env.dir,
        "0002_unique",
        r#", "atomic": false"#,
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS notes_body_idx ON notes (body);\n-- trestle:step\n\
         CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS notes_pinned_key ON notes (pinned);\n",
    );
    let ws = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
        .bind(ws)
        .execute(&mut conn)
        .await
        .unwrap();
    for _ in 0..2 {
        sqlx::query("INSERT INTO notes (id, workspace_id, title) VALUES ($1, $2, 't')")
            .bind(uuid::Uuid::now_v7())
            .bind(ws)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    assert_eq!(env.code(&["migrate"]).await, exit::PARTIAL);
    let verify = env.run(&["verify"]).await.unwrap_err();
    assert_eq!(verify.code, exit::DRIFT);
    assert!(
        verify.contains("notes.0002_unique: stopped at step 2"),
        "{verify}"
    );
    let plan = env.plan().await;
    assert_eq!(
        (plan.steps[0].status, plan.steps[0].resume_step),
        ("partial", Some(2))
    );

    // Fixed data: the rerun resumes and verify passes again.
    sqlx::query("DELETE FROM notes WHERE ctid <> (SELECT min(ctid) FROM notes)")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(env.code(&["migrate"]).await, exit::OK);
    assert_eq!(env.code(&["verify"]).await, exit::OK);

    // 6: catalog drift.
    sqlx::query("ALTER TABLE notes DROP COLUMN body")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(env.code(&["verify"]).await, exit::DRIFT);
    sqlx::query("ALTER TABLE notes ADD COLUMN body text")
        .execute(&mut conn)
        .await
        .unwrap();

    // 7: another runner holds the lock.
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(0x7472_6573_746c_6501_i64)
        .execute(&mut conn)
        .await
        .unwrap();
    add(&env.dir, "0003_noop", "", "-- nothing\n");
    assert_eq!(
        env.code(&["migrate", "--lock-timeout", "200ms"]).await,
        exit::LOCK
    );
    let _ = conn.close().await;

    // 1: blocked plan (an applied migration changed).
    std::fs::write(env.dir.join("notes/0001_initial.sql"), "-- edited\n").unwrap();
    assert_eq!(env.code(&["migrate", "--plan"]).await, exit::CHECK);
    env.db.drop().await;
}

#[tokio::test]
async fn usage_and_unreachable_database() {
    let env = Env::new().await;
    let matches = env
        .project()
        .command()
        .try_get_matches_from(["notes", "migrate"])
        .unwrap();
    // Without DATABASE_URL (from the environment) this is a usage error.
    if std::env::var_os("DATABASE_URL").is_none() {
        let err = env.project().run_matches(&matches).await.unwrap_err();
        assert_eq!(err.code, exit::USAGE);
    }
    let unreachable = Env {
        url: "postgres://127.0.0.1:1/nowhere".into(),
        ..env
    };
    assert_eq!(
        unreachable.code(&["migrate", "--plan"]).await,
        exit::UNREACHABLE
    );
    assert_eq!(unreachable.code(&["migrate"]).await, exit::UNREACHABLE);
    assert_eq!(unreachable.code(&["verify"]).await, exit::UNREACHABLE);
    unreachable.db.drop().await;
}

#[tokio::test]
async fn foreign_apps_share_the_database() {
    let env = Env::new().await;
    env.run(&["migrate"]).await.unwrap();
    // Another project (or this project's former self) applied `school`.
    let mut conn = env.db.connect().await;
    sqlx::query(
        "INSERT INTO trestle_migrations (app, id, digest, status) \
         VALUES ('school', '0001_initial', 'abc', 'applied')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    // Unknown to this build: refused, as if a newer release had run.
    assert_ne!(env.code(&["migrate"]).await, exit::OK);
    assert_eq!(env.code(&["verify"]).await, exit::DRIFT);

    let foreign = |args: &'static [&'static str]| {
        let mut argv = vec!["notes", "--database-url", env.url.as_str()];
        argv.extend_from_slice(args);
        let project = || env.project().foreign_apps(["school"]);
        let matches = project().command().try_get_matches_from(argv).unwrap();
        async move { project().run_matches(&matches).await }
    };
    foreign(&["migrate"]).await.unwrap();
    foreign(&["verify"]).await.unwrap();

    let set = load::history_from_dir(&env.dir, &[])
        .unwrap()
        .with_foreign_apps(["school"])
        .unwrap();
    let report = reconcile::verify(&mut conn, &set).await.unwrap();
    assert!(report.ok, "{report:?}");
    assert_eq!(report.foreign, vec!["school.0001_initial".to_owned()]);
    // An app with migrations here cannot also be foreign.
    let err = load::history_from_dir(&env.dir, &[])
        .unwrap()
        .with_foreign_apps(["notes"])
        .unwrap_err();
    assert!(err.to_string().contains("cannot be foreign"), "{err}");
    drop(conn);
    env.db.drop().await;
}

#[tokio::test]
async fn serve_refuses_a_build_the_schema_fence_shuts_out() {
    let env = Env::new().await;
    env.run(&["migrate"]).await.unwrap();
    // The old build keeps its own copy of history.
    let old = Env {
        dir: migrations(),
        url: env.url.clone(),
        db: TestDatabase::create().await,
    };

    // A newer release changes what titles mean and fences older builds.
    add(
        &env.dir,
        "0002_titles",
        r#", "fence": true"#,
        "UPDATE notes SET title = upper(title);\n",
    );
    let plan = env.plan().await;
    assert!(plan.steps[0].fence);
    assert!(
        plan.steps[0].reasons.contains(&"fence"),
        "{:?}",
        plan.steps[0].reasons
    );
    env.run(&["migrate"]).await.unwrap();
    assert_eq!(env.code(&["verify"]).await, exit::OK);

    // A configuration `serve` accepts, so only the schema can refuse: the
    // configuration is checked first, before anything connects.
    let key = "k".repeat(32);
    let serve = [
        "serve",
        "--session-key",
        &key,
        "--public-origin",
        "http://localhost:5173",
    ];
    let refused = old.run(&serve).await.unwrap_err();
    assert_eq!(refused.code, exit::DRIFT, "{refused}");
    assert!(
        refused.contains("fenced out this build") && refused.contains("notes.0002_titles"),
        "{refused}"
    );
    let verify = old.run(&["verify"]).await.unwrap_err();
    assert!(
        verify.contains("fenced: this build may not write until it contains notes.0002_titles"),
        "{verify}"
    );

    // A database migrated by an older Trestle has no fence: run migrate.
    let mut conn = env.db.connect().await;
    sqlx::query("DROP FUNCTION trestle_fence_v1(text[], text[])")
        .execute(&mut conn)
        .await
        .unwrap();
    let refused = env.run(&serve).await.unwrap_err();
    assert_eq!(refused.code, exit::CHECK, "{refused}");
    assert!(
        refused.contains("no schema fence yet") && refused.contains(" migrate`"),
        "{refused}"
    );
    let _ = conn.close().await;

    old.db.drop().await;
    env.db.drop().await;
}

#[tokio::test]
async fn the_plan_asserts_its_source_state_and_preconditions() {
    let env = Env::new().await;
    env.run(&["migrate"]).await.unwrap();
    let mut conn = env.db.connect().await;
    let ws = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
        .bind(ws)
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO notes (id, workspace_id, title, pinned) VALUES ($1, $2, 't', true)")
        .bind(uuid::Uuid::now_v7())
        .bind(ws)
        .execute(&mut conn)
        .await
        .unwrap();
    add(
        &env.dir,
        "0002_checked",
        r#", "preconditions": [
            {"name": "titles are set", "violations": "SELECT count(*) FROM notes WHERE title = ''"},
            {"name": "nothing pinned", "violations": "SELECT count(*) FROM notes WHERE pinned"}
        ]"#,
        "ALTER TABLE notes ADD COLUMN extra text;\n",
    );

    let plan = env.plan().await;
    let found: Vec<(&str, Option<i64>)> = plan.steps[0]
        .preconditions
        .iter()
        .map(|p| (p.name.as_str(), p.violations))
        .collect();
    assert_eq!(
        found,
        [("titles are set", Some(0)), ("nothing pinned", Some(1))]
    );
    assert!(
        plan.blocked
            .iter()
            .any(|b| b == "notes.0002_checked: precondition \"nothing pinned\" has 1 violation(s)"),
        "{:?}",
        plan.blocked
    );
    assert_eq!(env.code(&["migrate", "--plan"]).await, exit::CHECK);
    let refused = env.run(&["migrate"]).await.unwrap_err();
    assert_eq!(refused.code, exit::FAILED, "{refused}");
    assert!(refused.contains("nothing pinned"), "{refused}");

    // Drift blocks the plan and the run; --allow-drift overrides the run.
    sqlx::query("UPDATE notes SET pinned = false")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE notes DROP COLUMN body")
        .execute(&mut conn)
        .await
        .unwrap();
    let plan = env.plan().await;
    assert!(plan.database["drift"].is_array(), "{}", plan.database);
    assert!(
        plan.blocked
            .iter()
            .any(|b| b.starts_with("the database does not match its applied history")),
        "{:?}",
        plan.blocked
    );
    assert_eq!(env.code(&["migrate"]).await, exit::DRIFT);
    env.run(&["migrate", "--allow-drift"]).await.unwrap();
    let _ = conn.close().await;
    env.db.drop().await;
}
