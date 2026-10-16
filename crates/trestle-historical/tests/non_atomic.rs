//! F5: non-atomic migrations (design §6): one committed step at a time,
//! resumable, with invalid-index recovery and per-migration timeouts.

use sqlx::PgConnection;
use trestle_historical::testing::TestDatabase;
use trestle_historical::{
    HistoryError, LedgerStatus, Migration, MigrationRef, MigrationSet, PlanState,
    concurrent_index_name, migrate, plan, split_steps,
};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn manifest(app: &str, id: &str, deps: &[&str], extra: &str) -> &'static str {
    let deps: Vec<String> = deps.iter().map(|d| format!("{d:?}")).collect();
    leak(format!(
        r#"{{"format": 2, "app": "{app}", "id": "{id}", "depends_on": [{}], "operations": []{extra}}}"#,
        deps.join(", ")
    ))
}

const BASE_SQL: &str = "CREATE TABLE items (id bigint PRIMARY KEY, name text NOT NULL, sku text);
INSERT INTO items VALUES (1, 'a', 'x'), (2, 'b', 'x');";

fn base() -> Migration {
    Migration {
        app: "items",
        id: "0001_initial",
        depends_on: &[],
        sql: BASE_SQL,
        manifest: Some(manifest("items", "0001_initial", &[], "")),
        data: None,
    }
}

const DEP: &[MigrationRef] = &[MigrationRef::new("items", "0001_initial")];

fn non_atomic(sql: &'static str) -> Migration {
    Migration {
        app: "items",
        id: "0002_indexes",
        depends_on: DEP,
        sql,
        manifest: Some(manifest(
            "items",
            "0002_indexes",
            &["items.0001_initial"],
            r#", "atomic": false"#,
        )),
        data: None,
    }
}

const INDEXES: &str = "-- items.0002_indexes
CREATE INDEX CONCURRENTLY IF NOT EXISTS items_name_idx ON items (name);
-- trestle:step
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS items_sku_key ON items (sku);
";

#[test]
fn steps_split_on_marker_lines() {
    let steps = split_steps(INDEXES);
    assert_eq!(steps.len(), 2);
    assert!(steps[0].contains("items_name_idx"));
    assert_eq!(
        concurrent_index_name(&steps[1]).as_deref(),
        Some("items_sku_key")
    );
    assert_eq!(concurrent_index_name("CREATE INDEX x ON t (a)"), None);
}

#[test]
fn check_rules_for_non_atomic_migrations() {
    assert!(non_atomic(INDEXES).problems().is_empty());
    let problems = |sql: &'static str| non_atomic(sql).problems().join("\n");
    assert!(problems("CREATE INDEX CONCURRENTLY IF NOT EXISTS a ON t (x); CREATE INDEX CONCURRENTLY IF NOT EXISTS b ON t (y);").contains("one statement per step"));
    assert!(
        problems("ALTER TABLE t ADD COLUMN c text;")
            .contains("put other DDL in an atomic migration")
    );
    assert!(problems("CREATE INDEX CONCURRENTLY a ON t (x);").contains("IF NOT EXISTS"));
    assert!(problems("DROP INDEX CONCURRENTLY a;").contains("IF EXISTS"));
    assert!(problems("-- nothing\n").contains("at least one step"));
    assert!(
        non_atomic("ALTER TABLE t VALIDATE CONSTRAINT c;")
            .problems()
            .is_empty()
    );

    let mut with_data = non_atomic(INDEXES);
    with_data.data = Some(trestle_historical::DataStep {
        name: "fill",
        source: "",
        run: |_| Box::pin(async { Ok(()) }),
    });
    assert!(
        with_data
            .problems()
            .join("")
            .contains("cannot have a data step")
    );

    let mut atomic = base();
    atomic.sql = "SELECT 1;\n-- trestle:step\nSELECT 2;";
    assert!(atomic.problems().join("").contains("only for migrations"));
    atomic.sql = "SELECT 1;";
    atomic.manifest = Some(manifest(
        "items",
        "0001_initial",
        &[],
        r#", "lock_timeout": "soon""#,
    ));
    assert!(atomic.problems().join("").contains("lock_timeout \"soon\""));
}

async fn ledger(conn: &mut PgConnection, id: &str) -> (String, i32, i32) {
    sqlx::query_as("SELECT status, step, attempts FROM trestle_migrations WHERE id = $1")
        .bind(id)
        .fetch_one(conn)
        .await
        .unwrap()
}

async fn index_valid(conn: &mut PgConnection, name: &str) -> Option<bool> {
    sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = $1",
    )
    .bind(name)
    .fetch_optional(conn)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_failed_step_is_partial_and_the_rerun_resumes_and_repairs_the_index() {
    let db = TestDatabase::create().await;
    let set = MigrationSet::new(vec![base(), non_atomic(INDEXES)]).unwrap();

    // Step 2 fails: duplicate SKUs. CONCURRENTLY leaves an INVALID index.
    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Partial { step: 2, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("resumes at step 2"), "{err}");
    let mut conn = db.connect().await;
    assert_eq!(
        ledger(&mut conn, "0002_indexes").await,
        ("partial".into(), 1, 1)
    );
    assert_eq!(index_valid(&mut conn, "items_name_idx").await, Some(true));
    assert_eq!(index_valid(&mut conn, "items_sku_key").await, Some(false));
    let p = plan(&mut conn, &set).await.unwrap();
    assert_eq!(
        p.entries[1].state,
        PlanState::Retry {
            previous: LedgerStatus::Partial,
            attempts: 1,
            step: 1
        }
    );

    // A partly applied migration may not change before it finishes.
    let mut edited = non_atomic(leak(format!("{INDEXES}\n-- edited\n")));
    edited.id = "0002_indexes";
    let changed = MigrationSet::new(vec![base(), edited]).unwrap();
    assert!(matches!(
        plan(&mut conn, &changed).await,
        Err(HistoryError::DigestMismatch { .. })
    ));

    // Fix the data and rerun: resumes at step 2, rebuilds the index.
    sqlx::query("UPDATE items SET sku = 'y' WHERE id = 2")
        .execute(&mut conn)
        .await
        .unwrap();
    let applied = migrate(db.options(), &set).await.unwrap();
    assert_eq!(applied, [MigrationRef::new("items", "0002_indexes")]);
    assert_eq!(
        ledger(&mut conn, "0002_indexes").await,
        ("applied".into(), 2, 2)
    );
    assert_eq!(index_valid(&mut conn, "items_sku_key").await, Some(true));
    assert!(migrate(db.options(), &set).await.unwrap().is_empty());

    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn timeouts_come_from_the_manifest() {
    let db = TestDatabase::create().await;
    let mut m = base();
    m.sql = "DO $$ BEGIN
  IF current_setting('lock_timeout') <> '1500ms' OR current_setting('statement_timeout') <> '2min' THEN
    RAISE EXCEPTION 'timeouts not applied: % %', current_setting('lock_timeout'), current_setting('statement_timeout');
  END IF;
END $$;";
    m.manifest = Some(manifest(
        "items",
        "0001_initial",
        &[],
        r#", "lock_timeout": "1500ms", "statement_timeout": "2min""#,
    ));
    let set = MigrationSet::new(vec![m]).unwrap();
    migrate(db.options(), &set).await.unwrap();
    db.drop().await;
}

#[tokio::test]
async fn an_old_ledger_is_upgraded_in_place() {
    let db = TestDatabase::create().await;
    let mut conn = db.connect().await;
    sqlx::raw_sql(
        "CREATE TABLE trestle_migrations (
            app text NOT NULL, id text NOT NULL, digest text NOT NULL,
            status text NOT NULL CHECK (status IN ('running', 'applied', 'failed')),
            attempts integer NOT NULL DEFAULT 0, started_at timestamptz,
            finished_at timestamptz, last_error text, PRIMARY KEY (app, id));",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    let set = MigrationSet::new(vec![base()]).unwrap();
    // The old ledger is read as-is before the runner upgrades it.
    assert_eq!(
        plan(&mut conn, &set).await.unwrap().entries[0].state,
        PlanState::Pending
    );
    sqlx::raw_sql(BASE_SQL).execute(&mut conn).await.unwrap();
    sqlx::query(
        "INSERT INTO trestle_migrations (app, id, digest, status, attempts) VALUES ('items', '0001_initial', $1, 'applied', 1)",
    )
    .bind(base().digest())
    .execute(&mut conn)
    .await
    .unwrap();

    let set = MigrationSet::new(vec![base(), non_atomic(INDEXES)]).unwrap();
    sqlx::query("UPDATE items SET sku = id::text")
        .execute(&mut conn)
        .await
        .unwrap();
    let applied = migrate(db.options(), &set).await.unwrap();
    assert_eq!(applied, [MigrationRef::new("items", "0002_indexes")]);
    assert_eq!(
        ledger(&mut conn, "0001_initial").await,
        ("applied".into(), 0, 1)
    );
    assert_eq!(
        ledger(&mut conn, "0002_indexes").await,
        ("applied".into(), 2, 1)
    );
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn a_first_step_that_was_attempted_keeps_its_digest() {
    let db = TestDatabase::create().await;
    // Step 1 fails on duplicate SKUs and leaves an INVALID index behind,
    // although the ledger still says step 0.
    const FIRST: &str = "-- items.0002_indexes
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS items_sku_key ON items (sku);
-- trestle:step
CREATE INDEX CONCURRENTLY IF NOT EXISTS items_name_idx ON items (name);
";
    let set = MigrationSet::new(vec![base(), non_atomic(FIRST)]).unwrap();
    assert!(migrate(db.options(), &set).await.is_err());
    let mut conn = db.connect().await;
    assert_eq!(index_valid(&mut conn, "items_sku_key").await, Some(false));

    // Editing it now would resume against a database the new text never
    // described.
    let edited = non_atomic(leak(format!("{FIRST}\n-- edited\n")));
    let changed = MigrationSet::new(vec![base(), edited]).unwrap();
    assert!(
        matches!(
            plan(&mut conn, &changed).await,
            Err(HistoryError::DigestMismatch { .. })
        ),
        "an attempted non-atomic migration may not change"
    );
    drop(conn);
    db.drop().await;
}
