//! ADR-019: a plan asserts the state it starts from and reports what it
//! would do. Preconditions stop a migration before it changes anything;
//! impact counts show nulls, duplicates, fills and deletions; the runner
//! refuses to apply over drift. Needs PostgreSQL
//! (`TRESTLE_TEST_DATABASE_URL`).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;
use sqlx::PgConnection;
use trestle_historical::artifact::{self, Artifact};
use trestle_historical::fence;
use trestle_historical::impact::{self, Impact};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::{Column, Model, SchemaState, desired};
use trestle_historical::testing::TestDatabase;
use trestle_historical::{
    HistoryError, Migration, MigrationRef, MigrationSet, RunConfig, migrate, migrate_with,
};

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn to_migration(artifact: &Artifact) -> Migration {
    let depends_on: Vec<MigrationRef> = artifact
        .manifest
        .depends_on
        .iter()
        .map(|d| {
            let (app, id) = d.split_once('.').unwrap();
            MigrationRef::new(leak(app.into()), leak(id.into()))
        })
        .collect();
    Migration {
        app: leak(artifact.manifest.app.clone()),
        id: leak(artifact.manifest.id.clone()),
        depends_on: Box::leak(depends_on.into_boxed_slice()),
        sql: leak(artifact.sql.clone()),
        manifest: Some(leak(artifact.manifest_json())),
        data: None,
    }
}

fn orders(amount_required: bool, sku_unique: bool, with_note: bool) -> Vec<Model> {
    let amount = Column::integer("amount");
    let mut model = Model::new("shop.order")
        .table("orders")
        .field(Column::uuid("id").primary_key())
        .field(if amount_required {
            amount
        } else {
            amount.nullable()
        })
        .field(Column::text("sku").nullable());
    if with_note {
        model = model.field(Column::text("note").nullable());
    }
    if sku_unique {
        model = model.unique("orders_sku_key", &["sku"]);
    }
    vec![model]
}

/// Appends the planner's migration from `set`'s history to `models`.
fn generate(set: &mut Vec<Migration>, models: Vec<Model>, name: &str, options: PlanOptions) {
    let current = MigrationSet::ordered(set.clone()).unwrap();
    let plan = diff(
        &current.history().unwrap(),
        &desired(&models).unwrap(),
        &options,
    );
    assert!(plan.blocked.is_empty(), "{:#?}", plan.blocked);
    let mut artifacts = artifact::generate(&current, &plan, name).unwrap();
    set.push(to_migration(&artifacts.remove(0)));
}

fn initial() -> Vec<Migration> {
    let mut set = Vec::new();
    generate(
        &mut set,
        orders(false, false, true),
        "initial",
        PlanOptions::default(),
    );
    set
}

/// Three orders: two without an amount, two sharing a SKU, two with notes.
async fn seeded(db: &TestDatabase) -> PgConnection {
    migrate(db.options(), &MigrationSet::new(initial()).unwrap())
        .await
        .unwrap();
    let mut conn = db.connect().await;
    sqlx::raw_sql(
        "INSERT INTO orders (id, amount, sku, note) VALUES
           (gen_random_uuid(), NULL, 'x', 'first'),
           (gen_random_uuid(), 5, 'x', NULL),
           (gen_random_uuid(), NULL, 'y', 'third');
         ANALYZE orders;",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    conn
}

/// The pending migration's impact, against its history state.
async fn impact_of(conn: &mut PgConnection, set: &[Migration]) -> Vec<Impact> {
    let set = MigrationSet::new(set.to_vec()).unwrap();
    let last = set.iter().last().unwrap().clone();
    let states = set.states().unwrap();
    let before: SchemaState = states[states.len() - 2].clone();
    let ops = set.manifest(last.key()).unwrap().operations.clone();
    impact::impact(conn, &last, &ops, &before, "5s").await
}

fn find<'a>(impact: &'a [Impact], change: &str) -> &'a Impact {
    impact
        .iter()
        .find(|i| i.change == change)
        .unwrap_or_else(|| panic!("no {change} in {impact:#?}"))
}

#[tokio::test]
async fn impact_counts_what_each_step_would_touch() {
    let db = TestDatabase::create().await;
    let mut conn = seeded(&db).await;

    let mut set = initial();
    generate(
        &mut set,
        orders(true, true, false),
        "tighten",
        PlanOptions {
            allow_destructive: BTreeSet::from(["shop.order.note".to_owned()]),
            filled: BTreeSet::from(["shop.order.amount".to_owned()]),
            ..Default::default()
        },
    );
    // NULLs never collide under a unique constraint, so these two are not
    // duplicates.
    sqlx::raw_sql(
        "INSERT INTO orders (id, amount, sku) VALUES
           (gen_random_uuid(), 1, NULL), (gen_random_uuid(), 2, NULL);
         ANALYZE orders;",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    let impact = impact_of(&mut conn, &set).await;

    let dropped = find(&impact, "drop_column");
    assert_eq!(
        (dropped.affected, dropped.meaning, dropped.blocks),
        (Some(2), Some("values deleted"), false)
    );
    assert_eq!(
        dropped.rows,
        Some(5),
        "the planner's estimate after ANALYZE"
    );
    let required = find(&impact, "not_null");
    assert_eq!(
        (required.affected, required.meaning, required.blocks),
        (Some(2), Some("rows that are null"), true),
        "SET NOT NULL would fail on two rows"
    );
    let unique = find(&impact, "unique");
    assert_eq!(
        (unique.affected, unique.meaning, unique.blocks),
        (Some(1), Some("duplicate groups"), true)
    );

    // Counting changed nothing.
    let (rows, nulls): (i64, i64) =
        sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE amount IS NULL) FROM orders")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!((rows, nulls), (5, 2));
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn a_backfill_counts_rows_to_fill_and_unblocks_not_null() {
    let db = TestDatabase::create().await;
    let mut conn = seeded(&db).await;
    let mut set = initial();
    generate(
        &mut set,
        orders(true, false, true),
        "require_amount",
        PlanOptions {
            backfills: BTreeMap::from([("shop.order.amount".to_owned(), json!(0))]),
            ..Default::default()
        },
    );
    let impact = impact_of(&mut conn, &set).await;
    let fill = find(&impact, "backfill");
    assert_eq!(
        (fill.affected, fill.meaning),
        (Some(2), Some("rows to fill"))
    );
    assert!(
        !find(&impact, "not_null").blocks,
        "the backfill fills them first"
    );

    migrate(db.options(), &MigrationSet::new(set).unwrap())
        .await
        .expect("and the migration applies");
    drop(conn);
    db.drop().await;
}

fn manifest(id: &str, extra: &str) -> &'static str {
    leak(format!(
        r#"{{"format": 2, "app": "shop", "id": "{id}", "depends_on": ["shop.0001_initial"], "operations": []{extra}}}"#
    ))
}

const AFTER_INITIAL: &[MigrationRef] = &[MigrationRef::new("shop", "0001_initial")];
const NEEDS_AMOUNTS: &str = r#", "preconditions": [{"name": "every order has an amount", "violations": "SELECT count(*) FROM orders WHERE amount IS NULL;"}]"#;

fn checked(id: &'static str, sql: &'static str, extra: &str) -> Migration {
    Migration {
        app: "shop",
        id,
        depends_on: AFTER_INITIAL,
        sql,
        manifest: Some(manifest(id, extra)),
        data: None,
    }
}

async fn ledger(conn: &mut PgConnection, id: &str) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM trestle_migrations WHERE id = $1")
        .bind(id)
        .fetch_optional(conn)
        .await
        .unwrap()
}

async fn has_column(conn: &mut PgConnection, column: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns
          WHERE table_name = 'orders' AND column_name = $1)",
    )
    .bind(column)
    .fetch_one(conn)
    .await
    .unwrap()
}

#[tokio::test]
async fn preconditions_stop_a_migration_before_it_changes_anything() {
    let db = TestDatabase::create().await;
    let mut conn = seeded(&db).await;
    let extra = checked(
        "0002_extra",
        "ALTER TABLE orders ADD COLUMN extra text;",
        NEEDS_AMOUNTS,
    );
    let mut history = initial();
    history.push(extra.clone());
    let set = MigrationSet::new(history).unwrap();

    let evaluated = impact::preconditions(&mut conn, &extra, "5s").await;
    assert_eq!(evaluated[0].violations, Some(2));

    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Precondition { violations: 2, name, .. } if name == "every order has an amount"),
        "{err}"
    );
    assert!(!has_column(&mut conn, "extra").await, "nothing ran");
    assert_eq!(
        ledger(&mut conn, "0002_extra").await.as_deref(),
        Some("failed")
    );

    sqlx::query("UPDATE orders SET amount = 0 WHERE amount IS NULL")
        .execute(&mut conn)
        .await
        .unwrap();
    assert_eq!(
        impact::preconditions(&mut conn, &extra, "5s").await[0].violations,
        Some(0)
    );
    migrate(db.options(), &set).await.expect("holds now");
    assert!(has_column(&mut conn, "extra").await);
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn a_non_atomic_migration_checks_before_its_first_step_and_fence() {
    let db = TestDatabase::create().await;
    let mut conn = seeded(&db).await;
    let index = checked(
        "0002_index",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS orders_amount_idx ON orders (amount);\n",
        &format!(r#", "atomic": false, "fence": true{NEEDS_AMOUNTS}"#),
    );
    let mut history = initial();
    history.push(index);
    let set = MigrationSet::new(history).unwrap();

    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(matches!(err, HistoryError::Precondition { .. }), "{err}");
    let built: bool = sqlx::query_scalar("SELECT to_regclass('orders_amount_idx') IS NOT NULL")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert!(!built, "no step ran");
    assert!(
        fence::status(&mut conn).await.unwrap().is_empty(),
        "and older builds were not fenced"
    );
    drop(conn);
    db.drop().await;
}

#[test]
fn malformed_preconditions_are_problems() {
    let cases = [
        (
            r#", "preconditions": [{"name": "", "violations": "SELECT 0"}]"#,
            "every precondition needs a name of its own",
        ),
        (
            r#", "preconditions": [{"name": "a", "violations": "SELECT 0"}, {"name": "a", "violations": "SELECT 0"}]"#,
            "every precondition needs a name of its own",
        ),
        (
            r#", "preconditions": [{"name": "a", "violations": "DELETE FROM orders"}]"#,
            "must be one SELECT returning a count",
        ),
        (
            r#", "preconditions": [{"name": "a", "violations": "SELECT 0; SELECT 1"}]"#,
            "must be one SELECT returning a count",
        ),
        (
            r#", "preconditions": {"name": "a"}"#,
            "preconditions must be a list",
        ),
    ];
    for (extra, expected) in cases {
        let problems = checked("0002_x", "SELECT 1;", extra).problems();
        assert!(
            problems.iter().any(|p| p.contains(expected)),
            "{extra}: {problems:?}"
        );
    }
    assert!(
        checked("0002_x", "SELECT 1;", NEEDS_AMOUNTS)
            .problems()
            .is_empty()
    );
}

#[tokio::test]
async fn migrate_refuses_to_apply_over_drift() {
    let db = TestDatabase::create().await;
    let mut conn = seeded(&db).await;
    sqlx::query("ALTER TABLE orders DROP COLUMN note")
        .execute(&mut conn)
        .await
        .unwrap();
    let mut history = initial();
    history.push(checked(
        "0002_extra",
        "ALTER TABLE orders ADD COLUMN extra text;",
        "",
    ));
    let set = MigrationSet::new(history).unwrap();

    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Drift(d) if d.contains("note")),
        "{err}"
    );
    assert!(!has_column(&mut conn, "extra").await);
    assert_eq!(
        ledger(&mut conn, "0002_extra").await,
        None,
        "not even attempted"
    );

    let config = RunConfig {
        allow_drift: true,
        ..RunConfig::default()
    };
    migrate_with(db.options(), &set, &config)
        .await
        .expect("a reviewed difference can be overridden");
    assert!(has_column(&mut conn, "extra").await);
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn a_check_on_a_table_an_earlier_step_creates_is_not_counted_yet() {
    let db = TestDatabase::create().await;
    let mut conn = db.connect().await;
    let later = checked("0002_later", "SELECT 1;", NEEDS_AMOUNTS);
    let evaluated = impact::preconditions(&mut conn, &later, "5s").await;
    assert_eq!(evaluated[0].violations, None);
    assert_eq!(
        evaluated[0].note.as_deref(),
        Some("not measurable until earlier steps apply")
    );
    drop(conn);
    db.drop().await;
}
