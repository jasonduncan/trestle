//! Runner behavior against real PostgreSQL (design doc §19.1, Migrations row).

use sqlx::PgConnection;
use trestle_historical::testing::TestDatabase;
use trestle_historical::{
    HistoryError, Migration, MigrationRef, MigrationSet, PlanState, migrate, missing, plan,
};

const WIDGETS: Migration = Migration {
    app: "widgets",
    id: "0001_initial",
    depends_on: &[],
    sql: "CREATE TABLE widgets (id bigint PRIMARY KEY, name text NOT NULL);",
    manifest: None,
    data: None,
};

const GADGETS: Migration = Migration {
    app: "gadgets",
    id: "0001_initial",
    depends_on: &[MigrationRef::new("widgets", "0001_initial")],
    sql: "CREATE TABLE gadgets (id bigint PRIMARY KEY, widget_id bigint REFERENCES widgets (id));
          INSERT INTO widgets (id, name) VALUES (1, 'seed');",
    manifest: None,
    data: None,
};

fn set(migrations: &[Migration]) -> MigrationSet {
    MigrationSet::new(migrations.to_vec()).unwrap()
}

async fn table_exists(conn: &mut PgConnection, table: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
        .bind(table)
        .fetch_one(conn)
        .await
        .unwrap()
}

async fn attempts(conn: &mut PgConnection, app: &str) -> (String, i32) {
    sqlx::query_as("SELECT status, attempts FROM trestle_migrations WHERE app = $1")
        .bind(app)
        .fetch_one(conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn fresh_database_replays_then_second_run_is_a_no_op() {
    let db = TestDatabase::create().await;
    let history = set(&[WIDGETS, GADGETS]);

    let mut conn = db.connect().await;
    let before = plan(&mut conn, &history).await.unwrap();
    assert!(before.entries.iter().all(|e| e.state == PlanState::Pending));
    assert_eq!(
        missing(&mut conn, &[WIDGETS.key()]).await.unwrap(),
        [WIDGETS.key()]
    );

    let applied = migrate(db.options(), &history).await.unwrap();
    assert_eq!(applied, [WIDGETS.key(), GADGETS.key()]);
    assert!(table_exists(&mut conn, "gadgets").await);
    assert!(
        missing(&mut conn, &[WIDGETS.key(), GADGETS.key()])
            .await
            .unwrap()
            .is_empty()
    );

    assert!(migrate(db.options(), &history).await.unwrap().is_empty());
    assert_eq!(attempts(&mut conn, "widgets").await, ("applied".into(), 1));
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn changed_applied_migration_is_refused() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &set(&[WIDGETS])).await.unwrap();

    let mut edited = WIDGETS;
    edited.sql = "CREATE TABLE widgets (id bigint PRIMARY KEY);";
    let err = migrate(db.options(), &set(&[edited, GADGETS]))
        .await
        .unwrap_err();
    assert!(matches!(err, HistoryError::DigestMismatch { .. }), "{err}");

    // Nothing after the mismatch ran.
    let mut conn = db.connect().await;
    assert!(!table_exists(&mut conn, "gadgets").await);
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn failed_migration_rolls_back_completely_and_can_be_retried() {
    let db = TestDatabase::create().await;
    let broken = Migration {
        sql: "CREATE TABLE widgets (id bigint PRIMARY KEY); SELECT 1 / 0;",
        ..WIDGETS
    };
    let err = migrate(db.options(), &set(&[broken])).await.unwrap_err();
    assert!(matches!(err, HistoryError::Failed { .. }), "{err}");

    let mut conn = db.connect().await;
    assert!(
        !table_exists(&mut conn, "widgets").await,
        "DDL must roll back"
    );
    assert_eq!(attempts(&mut conn, "widgets").await, ("failed".into(), 1));
    let retry = plan(&mut conn, &set(&[WIDGETS])).await.unwrap();
    assert!(matches!(
        retry.entries[0].state,
        PlanState::Retry { attempts: 1, .. }
    ));

    // A never-applied migration may be fixed before retrying.
    migrate(db.options(), &set(&[WIDGETS])).await.unwrap();
    assert!(table_exists(&mut conn, "widgets").await);
    assert_eq!(attempts(&mut conn, "widgets").await, ("applied".into(), 2));
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn concurrent_runners_apply_each_migration_once() {
    let db = TestDatabase::create().await;
    let history = set(&[WIDGETS, GADGETS]);
    let runs = (0..4).map(|_| {
        let options = db.options().clone();
        let history = history.clone();
        tokio::spawn(async move { migrate(&options, &history).await })
    });
    let mut total_applied = 0;
    for run in runs {
        total_applied += run.await.unwrap().unwrap().len();
    }
    assert_eq!(
        total_applied, 2,
        "each migration applied by exactly one runner"
    );

    let mut conn = db.connect().await;
    assert_eq!(attempts(&mut conn, "widgets").await, ("applied".into(), 1));
    assert_eq!(attempts(&mut conn, "gadgets").await, ("applied".into(), 1));
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn applied_migration_unknown_to_this_build_is_refused() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &set(&[WIDGETS, GADGETS]))
        .await
        .unwrap();

    // An older build that lacks `gadgets` must not run against this database.
    let err = migrate(db.options(), &set(&[WIDGETS])).await.unwrap_err();
    assert!(matches!(err, HistoryError::UnknownApplied(ref m) if m == "gadgets.0001_initial"));
    db.drop().await;
}
