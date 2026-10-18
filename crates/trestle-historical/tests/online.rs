//! ADR-021: online changes. `makemigrations --online` builds indexes on
//! existing tables `CONCURRENTLY` and adds checks to them `NOT VALID`,
//! validating in a second, non-atomic migration. Needs PostgreSQL
//! (`TRESTLE_TEST_DATABASE_URL`).

use sqlx::PgConnection;
use trestle_historical::artifact::{self, Artifact};
use trestle_historical::classify::{Reason, classify_set};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::{Column, Model, StateOp, desired};
use trestle_historical::testing::TestDatabase;
use trestle_historical::verify::verify;
use trestle_historical::{HistoryError, Migration, MigrationRef, MigrationSet, migrate};

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

fn orders(indexed: bool, checked: bool, noted: bool) -> Vec<Model> {
    let amount = Column::integer("amount");
    let mut model = Model::new("shop.order")
        .table("orders")
        .field(Column::uuid("id").primary_key())
        .field(Column::text("title"))
        .field(if checked {
            amount.range(0, 1000)
        } else {
            amount
        });
    if noted {
        model = model.field(Column::text("note").nullable());
    }
    if indexed {
        model = model.index("orders_title_idx", &["title"]);
    }
    vec![model]
}

fn generate(set: &[Migration], models: Vec<Model>, online: bool) -> Vec<Artifact> {
    named(set, models, online, "tighten")
}

fn named(set: &[Migration], models: Vec<Model>, online: bool, name: &str) -> Vec<Artifact> {
    let current = MigrationSet::ordered(set.to_vec()).unwrap();
    let options = PlanOptions {
        online,
        ..Default::default()
    };
    let plan = diff(
        &current.history().unwrap(),
        &desired(&models).unwrap(),
        &options,
    );
    assert!(plan.blocked.is_empty(), "{:#?}", plan.blocked);
    artifact::generate(&current, &plan, name).unwrap()
}

fn initial() -> Vec<Migration> {
    named(&[], orders(false, false, false), false, "initial")
        .iter()
        .map(to_migration)
        .collect()
}

async fn populated(db: &TestDatabase, amounts: &[i32]) -> PgConnection {
    migrate(db.options(), &MigrationSet::new(initial()).unwrap())
        .await
        .unwrap();
    let mut conn = db.connect().await;
    for amount in amounts {
        sqlx::query("INSERT INTO orders (id, title, amount) VALUES (gen_random_uuid(), 't', $1)")
            .bind(amount)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    conn
}

async fn constraint_valid(conn: &mut PgConnection, name: &str) -> Option<bool> {
    sqlx::query_scalar("SELECT convalidated FROM pg_constraint WHERE conname = $1")
        .bind(name)
        .fetch_optional(conn)
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

#[test]
fn an_online_change_splits_into_an_atomic_and_a_concurrent_migration() {
    let base = initial();
    let artifacts = generate(&base, orders(true, true, true), true);
    let ids: Vec<&str> = artifacts.iter().map(|a| a.manifest.id.as_str()).collect();
    assert_eq!(ids, ["0002_tighten", "0003_tighten_online"]);

    let (first, second) = (&artifacts[0], &artifacts[1]);
    assert!(first.manifest.atomic);
    assert!(first.sql.contains("ADD COLUMN note text;"));
    assert!(
        first
            .sql
            .contains("CHECK (amount BETWEEN 0 AND 1000) NOT VALID;"),
        "{}",
        first.sql
    );
    assert!(!first.sql.contains("CREATE INDEX"), "{}", first.sql);

    assert!(!second.manifest.atomic);
    assert_eq!(second.manifest.depends_on, ["shop.0002_tighten"]);
    assert!(second.sql.contains(
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS orders_title_idx ON orders (title);\n-- trestle:step\n"
    ));
    assert!(
        second
            .sql
            .contains("VALIDATE CONSTRAINT orders_amount_check;"),
        "{}",
        second.sql
    );
    assert!(matches!(
        second.manifest.operations[..],
        [StateOp::AddIndex { .. }, StateOp::ValidateConstraint { .. }]
    ));

    let mut all = base;
    all.extend(artifacts.iter().map(to_migration));
    for m in &all {
        assert!(m.problems().is_empty(), "{}: {:?}", m.key(), m.problems());
    }
    let classes = classify_set(&MigrationSet::new(all).unwrap());
    assert_eq!(
        classes[1],
        Vec::<Reason>::new(),
        "the atomic half reads no rows: auto"
    );
    assert_eq!(
        classes[2],
        [Reason::NonAtomic],
        "generated, so not hand SQL; concurrent, so no table lock"
    );
}

#[test]
fn without_online_the_change_stays_one_atomic_migration() {
    let artifacts = generate(&initial(), orders(true, true, true), false);
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].sql.contains("CREATE INDEX orders_title_idx"));
    assert!(!artifacts[0].sql.contains("NOT VALID"));
    let json = artifacts[0].manifest_json();
    assert!(
        !json.contains("not_valid"),
        "older manifests keep their form: {json}"
    );
}

#[test]
fn an_index_alone_is_one_online_migration() {
    let artifacts = generate(&initial(), orders(true, false, false), true);
    let ids: Vec<&str> = artifacts.iter().map(|a| a.manifest.id.as_str()).collect();
    assert_eq!(ids, ["0002_tighten_online"]);
    assert_eq!(artifacts[0].manifest.depends_on, ["shop.0001_initial"]);
}

#[tokio::test]
async fn the_online_pair_applies_to_a_populated_table() {
    let db = TestDatabase::create().await;
    let mut conn = populated(&db, &[1, 2, 3]).await;
    let mut all = initial();
    all.extend(
        generate(&all, orders(true, true, true), true)
            .iter()
            .map(to_migration),
    );
    let set = MigrationSet::new(all).unwrap();

    migrate(db.options(), &set).await.unwrap();
    assert_eq!(index_valid(&mut conn, "orders_title_idx").await, Some(true));
    assert_eq!(
        constraint_valid(&mut conn, "orders_amount_check").await,
        Some(true)
    );
    let report = verify(&mut conn, &set.history().unwrap()).await.unwrap();
    assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);
    drop(conn);
    db.drop().await;
}

#[tokio::test]
async fn a_row_that_breaks_the_new_check_stops_validation_until_fixed() {
    let db = TestDatabase::create().await;
    let mut conn = populated(&db, &[1, 5000]).await;
    let mut all = initial();
    all.extend(
        generate(&all, orders(true, true, true), true)
            .iter()
            .map(to_migration),
    );
    let set = MigrationSet::new(all).unwrap();

    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Partial { step: 2, migration, .. } if migration == "shop.0003_tighten_online"),
        "{err}"
    );
    assert_eq!(index_valid(&mut conn, "orders_title_idx").await, Some(true));
    assert_eq!(
        constraint_valid(&mut conn, "orders_amount_check").await,
        Some(false),
        "added NOT VALID, not yet validated"
    );
    // New rows are already checked.
    let refused =
        sqlx::query("INSERT INTO orders (id, title, amount) VALUES (gen_random_uuid(), 't', 9999)")
            .execute(&mut conn)
            .await
            .unwrap_err();
    assert_eq!(
        refused
            .as_database_error()
            .and_then(|d| d.code())
            .as_deref(),
        Some("23514")
    );

    sqlx::query("UPDATE orders SET amount = 500 WHERE amount > 1000")
        .execute(&mut conn)
        .await
        .unwrap();
    migrate(db.options(), &set).await.unwrap();
    assert_eq!(
        constraint_valid(&mut conn, "orders_amount_check").await,
        Some(true)
    );
    drop(conn);
    db.drop().await;
}
