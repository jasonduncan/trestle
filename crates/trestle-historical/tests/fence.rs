//! ADR-017: the schema fence. A fence migration records itself; a build
//! without its app's newest fence migration is refused before `BEGIN`; the
//! lock order leaves no window in which such a build's write commits after
//! the fence. Needs PostgreSQL (`TRESTLE_TEST_DATABASE_URL`).

use std::time::{Duration, Instant};

use sqlx::{Connection, PgConnection};
use trestle_historical::classify::{Reason, classify};
use trestle_historical::fence::{self, BuildFence, FenceRow};
use trestle_historical::testing::TestDatabase;
use trestle_historical::{HistoryError, Manifest, Migration, MigrationRef, MigrationSet, migrate};

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn migration(
    id: &'static str,
    depends_on: &'static [MigrationRef],
    sql: &'static str,
    extra: &str,
) -> Migration {
    let deps: Vec<String> = depends_on.iter().map(|d| format!("\"{d}\"")).collect();
    Migration {
        app: "orders",
        id,
        depends_on,
        sql,
        manifest: Some(leak(format!(
            r#"{{"format": 2, "app": "orders", "id": "{id}", "depends_on": [{}], "operations": []{extra}}}"#,
            deps.join(", ")
        ))),
        data: None,
    }
}

const FIRST: &[MigrationRef] = &[MigrationRef::new("orders", "0001_initial")];
const SECOND: &[MigrationRef] = &[MigrationRef::new("orders", "0002_cents")];
const THIRD: &[MigrationRef] = &[MigrationRef::new("orders", "0003_note")];
const FENCE: &str = r#", "fence": true"#;

fn initial() -> Migration {
    migration(
        "0001_initial",
        &[],
        "CREATE TABLE orders (id bigint PRIMARY KEY, amount integer NOT NULL)",
        "",
    )
}

/// Changes what `amount` means: dollars become cents. Old code would keep
/// writing dollars, and nothing would fail.
fn cents() -> Migration {
    migration(
        "0002_cents",
        FIRST,
        "UPDATE orders SET amount = amount * 100",
        FENCE,
    )
}

fn note() -> Migration {
    migration(
        "0003_note",
        SECOND,
        "ALTER TABLE orders ADD COLUMN note text",
        "",
    )
}

fn currency() -> Migration {
    migration(
        "0004_currency",
        THIRD,
        "ALTER TABLE orders ADD COLUMN currency text NOT NULL DEFAULT 'USD'",
        FENCE,
    )
}

fn set(migrations: Vec<Migration>) -> MigrationSet {
    MigrationSet::new(migrations).unwrap()
}

fn release(n: usize) -> MigrationSet {
    set([initial(), cents(), note(), currency()]
        .into_iter()
        .take(n)
        .collect())
}

fn build(n: usize) -> BuildFence {
    BuildFence::of(&release(n))
}

async fn fences(conn: &mut PgConnection) -> Vec<FenceRow> {
    fence::status(conn).await.unwrap()
}

fn row(migration: &str, generation: i32) -> FenceRow {
    FenceRow {
        app: "orders".into(),
        migration: migration.into(),
        generation,
    }
}

/// Begins a write as `build` would: the check and `BEGIN` in one message.
async fn begin<'c>(
    conn: &'c mut PgConnection,
    build: &BuildFence,
) -> Result<sqlx::Transaction<'c, sqlx::Postgres>, sqlx::Error> {
    conn.begin_with(build.begin_sql()).await
}

fn is_fenced(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == fence::SQLSTATE)
}

#[tokio::test]
async fn fence_migrations_record_their_app_and_count_generations() {
    let db = TestDatabase::create().await;
    let mut conn = db.connect().await;
    assert!(!fence::installed(&mut conn).await.unwrap());

    migrate(db.options(), &release(1)).await.unwrap();
    assert!(
        fence::installed(&mut conn).await.unwrap(),
        "migrate creates the fence"
    );
    assert_eq!(fences(&mut conn).await, []);

    migrate(db.options(), &release(2)).await.unwrap();
    assert_eq!(fences(&mut conn).await, [row("0002_cents", 1)]);
    migrate(db.options(), &release(3)).await.unwrap();
    assert_eq!(
        fences(&mut conn).await,
        [row("0002_cents", 1)],
        "an ordinary migration leaves the fence alone"
    );
    migrate(db.options(), &release(4)).await.unwrap();
    assert_eq!(fences(&mut conn).await, [row("0004_currency", 2)]);

    assert_eq!(build(4).apps(), ["orders"]);
    assert_eq!(
        build(4).known(),
        ["orders.0002_cents", "orders.0004_currency"]
    );
    assert!(classify(&cents(), None, None).contains(&Reason::Fence));
    assert!(!classify(&note(), None, None).contains(&Reason::Fence));

    drop(conn);
    db.drop().await;
}

#[test]
fn manifests_without_a_fence_keep_their_digests() {
    let text =
        r#"{"format": 2, "app": "orders", "id": "0003_note", "depends_on": [], "operations": []}"#;
    let manifest: Manifest = serde_json::from_str(text).unwrap();
    assert!(!manifest.fence);
    let written = serde_json::to_value(&manifest).unwrap();
    assert!(written.get("fence").is_none(), "{written}");
    let fenced: Manifest =
        serde_json::from_str(&text.replace("[]}", r#"[], "fence": true}"#)).unwrap();
    assert_eq!(serde_json::to_value(&fenced).unwrap()["fence"], true);
}

#[tokio::test]
async fn builds_without_the_newest_fence_are_refused_before_begin() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &release(2)).await.unwrap();
    let mut conn = db.connect().await;

    assert_eq!(
        fence::fenced_by(&mut conn, &build(1)).await.unwrap(),
        ["orders.0002_cents"]
    );
    assert!(
        fence::fenced_by(&mut conn, &build(2))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fence::fenced_by(&mut conn, &build(3))
            .await
            .unwrap()
            .is_empty(),
        "a newer build passes"
    );
    let elsewhere = BuildFence::new(["billing"], Vec::<String>::new());
    assert!(
        fence::fenced_by(&mut conn, &elsewhere)
            .await
            .unwrap()
            .is_empty(),
        "another project's fence does not apply to a build that treats orders as foreign"
    );

    let refused = begin(&mut conn, &build(1)).await.unwrap_err();
    assert!(is_fenced(&refused), "{refused}");
    assert!(
        refused.to_string().contains("orders.0002_cents"),
        "{refused}"
    );
    // Nothing ran and no transaction is left open or aborted.
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("INSERT INTO orders VALUES (1, 100)")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = begin(&mut conn, &build(2)).await.unwrap();
    sqlx::query("INSERT INTO orders VALUES (2, 200)")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    migrate(db.options(), &release(4)).await.unwrap();
    for older in [2, 3] {
        assert_eq!(
            fence::fenced_by(&mut conn, &build(older)).await.unwrap(),
            ["orders.0004_currency"]
        );
        assert!(is_fenced(
            &begin(&mut conn, &build(older)).await.unwrap_err()
        ));
    }
    begin(&mut conn, &build(4))
        .await
        .unwrap()
        .commit()
        .await
        .unwrap();

    drop(conn);
    db.drop().await;
}

async fn holds_fence_lock(conn: &mut PgConnection) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_locks
          WHERE relation = 'trestle_schema_fence'::regclass
            AND mode = 'AccessExclusiveLock' AND granted)",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

async fn amounts(conn: &mut PgConnection) -> Vec<(i64, i32)> {
    sqlx::query_as("SELECT id, amount FROM orders ORDER BY id")
        .fetch_all(conn)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fence_migration_waits_for_writes_already_past_the_check() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &release(1)).await.unwrap();

    // An old build's write is under way: it passed the check, in dollars.
    let mut writer = db.connect().await;
    let mut tx = begin(&mut writer, &build(1)).await.unwrap();
    sqlx::query("INSERT INTO orders VALUES (1, 5)")
        .execute(&mut *tx)
        .await
        .unwrap();

    let options = db.options().clone();
    let migration = tokio::spawn(async move { migrate(&options, &release(2)).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !migration.is_finished(),
        "the fence migration waits for the write"
    );

    tx.commit().await.unwrap();
    let applied = tokio::time::timeout(Duration::from_secs(10), migration)
        .await
        .expect("finishes once the write commits")
        .unwrap()
        .unwrap();
    assert_eq!(applied, [MigrationRef::new("orders", "0002_cents")]);

    let mut conn = db.connect().await;
    assert_eq!(
        amounts(&mut conn).await,
        [(1, 500)],
        "the old write landed before the conversion, so it was converted"
    );
    assert!(is_fenced(&begin(&mut writer, &build(1)).await.unwrap_err()));

    drop((conn, writer));
    db.drop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_that_arrives_during_a_fence_migration_waits_then_sees_the_fence() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &release(1)).await.unwrap();
    let mut slow = cents();
    slow.sql = "SELECT pg_sleep(0.6); UPDATE orders SET amount = amount * 100";
    let options = db.options().clone();
    let migration =
        tokio::spawn(async move { migrate(&options, &set(vec![initial(), slow])).await });

    let mut probe = db.connect().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !holds_fence_lock(&mut probe).await {
        assert!(
            Instant::now() < deadline,
            "the migration never took the fence lock"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let write = |n: usize, id: i64| {
        let options = db.options().clone();
        tokio::spawn(async move {
            let mut conn = PgConnection::connect_with(&options).await.unwrap();
            let started = Instant::now();
            let result = match conn.begin_with(build(n).begin_sql()).await {
                Ok(mut tx) => {
                    sqlx::query("INSERT INTO orders VALUES ($1, 7)")
                        .bind(id)
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    tx.commit().await.map(drop)
                }
                Err(e) => Err(e),
            };
            (result, started.elapsed())
        })
    };
    let (old, new) = (write(1, 1), write(2, 2));
    let (old, old_waited) = old.await.unwrap();
    let (new, new_waited) = new.await.unwrap();
    migration.await.unwrap().unwrap();

    let refused = old.unwrap_err();
    assert!(is_fenced(&refused), "{refused}");
    assert!(
        old_waited >= Duration::from_millis(200),
        "the old build waited for the migration ({old_waited:?}) instead of passing a stale check"
    );
    new.unwrap();
    assert!(new_waited >= Duration::from_millis(200), "{new_waited:?}");
    assert_eq!(
        amounts(&mut probe).await,
        [(2, 7)],
        "only the new build wrote, after the conversion"
    );

    drop(probe);
    db.drop().await;
}

#[tokio::test]
async fn a_fence_migration_that_cannot_lock_in_time_raises_nothing() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &release(1)).await.unwrap();
    let mut writer = db.connect().await;
    let tx = begin(&mut writer, &build(1)).await.unwrap();

    let impatient = migration(
        "0002_cents",
        FIRST,
        "UPDATE orders SET amount = amount * 100",
        r#", "fence": true, "lock_timeout": "200ms""#,
    );
    let quick = set(vec![initial(), impatient]);
    let err = migrate(db.options(), &quick).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Failed { message, .. } if message.contains("55P03")),
        "{err}"
    );
    let mut conn = db.connect().await;
    assert_eq!(
        fences(&mut conn).await,
        [],
        "rolled back with the migration"
    );
    assert!(
        fence::fenced_by(&mut conn, &build(1))
            .await
            .unwrap()
            .is_empty()
    );

    tx.commit().await.unwrap();
    migrate(db.options(), &quick).await.unwrap();
    assert_eq!(fences(&mut conn).await, [row("0002_cents", 1)]);

    drop((conn, writer));
    db.drop().await;
}

#[tokio::test]
async fn a_non_atomic_fence_migration_fences_before_its_steps_and_records_once() {
    let db = TestDatabase::create().await;
    let base = migration(
        "0001_initial",
        &[],
        "CREATE TABLE orders (id bigint PRIMARY KEY, amount integer NOT NULL, ref text);
         INSERT INTO orders VALUES (1, 1, 'x'), (2, 2, 'x');",
        "",
    );
    let indexes = migration(
        "0002_cents",
        FIRST,
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS orders_amount_idx ON orders (amount);
-- trestle:step
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS orders_ref_key ON orders (ref);
",
        r#", "atomic": false, "fence": true"#,
    );
    let history = set(vec![base.clone(), indexes]);
    let old = BuildFence::of(&set(vec![base]));

    // Step 2 fails on duplicate refs; the fence went up before step 1.
    let err = migrate(db.options(), &history).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Partial { step: 2, .. }),
        "{err}"
    );
    let mut conn = db.connect().await;
    assert_eq!(fences(&mut conn).await, [row("0002_cents", 1)]);
    assert_eq!(
        fence::fenced_by(&mut conn, &old).await.unwrap(),
        ["orders.0002_cents"]
    );

    sqlx::query("UPDATE orders SET ref = 'y' WHERE id = 2")
        .execute(&mut conn)
        .await
        .unwrap();
    migrate(db.options(), &history).await.unwrap();
    assert_eq!(
        fences(&mut conn).await,
        [row("0002_cents", 1)],
        "resuming does not raise it twice"
    );

    drop(conn);
    db.drop().await;
}
