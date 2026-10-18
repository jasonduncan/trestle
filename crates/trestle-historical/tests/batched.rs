//! ADR-020: batched data steps. Each batch commits with its checkpoint, so
//! a failure keeps every committed batch and a rerun resumes after the last
//! one: every row is processed exactly once. Needs PostgreSQL
//! (`TRESTLE_TEST_DATABASE_URL`).

use serde_json::{Value, json};
use sqlx::PgConnection;
use trestle_historical::artifact::{self, Artifact};
use trestle_historical::data::{BatchFuture, DataStep, HistoricalTx};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::{Column, Model, desired};
use trestle_historical::testing::TestDatabase;
use trestle_historical::{
    HistoryError, LedgerStatus, Migration, MigrationRef, MigrationSet, PlanState, migrate, plan,
};
use uuid::Uuid;

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn to_migration(artifact: &Artifact, data: Option<DataStep>) -> Migration {
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
        data,
    }
}

fn initial() -> Migration {
    let models = vec![
        Model::new("shop.order")
            .table("orders")
            .field(Column::uuid("id").primary_key())
            .field(Column::text("title"))
            .field(Column::text("slug").nullable())
            .field(Column::integer("touched").nullable()),
    ];
    let empty = MigrationSet::new(Vec::new()).unwrap();
    let plan = diff(
        &empty.history().unwrap(),
        &desired(&models).unwrap(),
        &PlanOptions::default(),
    );
    to_migration(
        &artifact::generate(&empty, &plan, "initial").unwrap()[0],
        None,
    )
}

fn history(step: DataStep) -> MigrationSet {
    let base = initial();
    let set = MigrationSet::new(vec![base.clone()]).unwrap();
    let data = artifact::empty_batched(&set, "shop", "fill_slugs", step.name).unwrap();
    MigrationSet::new(vec![base, to_migration(&data, Some(step))]).unwrap()
}

/// Slugs three orders per batch and counts how often each row is touched.
/// A title of `boom` fails its batch partway through.
fn fill_slugs<'a>(tx: &'a mut HistoricalTx<'_>, after: Option<&'a Value>) -> BatchFuture<'a> {
    Box::pin(async move {
        let rows = tx
            .batch("shop.order", &["title", "touched"], after, 3)
            .await?;
        for row in &rows {
            let title = row["title"].as_str().unwrap_or_default();
            let touched = row["touched"].as_i64().unwrap_or(0) + 1;
            tx.update(
                "shop.order",
                &row["id"],
                &[
                    ("slug", json!(title.to_lowercase())),
                    ("touched", json!(touched)),
                ],
            )
            .await?;
            if title == "boom" {
                return Err(HistoryError::State("cannot slug boom".into()));
            }
        }
        Ok(rows.last().map(|r| r["id"].clone()))
    })
}

const FILL: DataStep = DataStep::batched("fill_slugs", "fn fill_slugs() { /* v1 */ }", fill_slugs);

async fn seed(conn: &mut PgConnection) {
    for i in 1..=10u128 {
        let title = if i == 8 {
            "boom".to_owned()
        } else {
            format!("Order {i}")
        };
        sqlx::query("INSERT INTO orders (id, title) VALUES ($1, $2)")
            .bind(Uuid::from_u128(i))
            .bind(title)
            .execute(&mut *conn)
            .await
            .unwrap();
    }
}

async fn touched(conn: &mut PgConnection) -> Vec<Option<i32>> {
    sqlx::query_scalar("SELECT touched FROM orders ORDER BY id")
        .fetch_all(conn)
        .await
        .unwrap()
}

async fn ledger(conn: &mut PgConnection) -> (String, i32, Option<String>) {
    sqlx::query_as(
        "SELECT status, step, checkpoint FROM trestle_migrations WHERE id = '0002_fill_slugs'",
    )
    .fetch_one(conn)
    .await
    .unwrap()
}

#[tokio::test]
async fn a_failed_batch_keeps_earlier_batches_and_the_rerun_resumes_after_them() {
    let db = TestDatabase::create().await;
    let set = history(FILL);
    migrate(db.options(), &MigrationSet::new(vec![initial()]).unwrap())
        .await
        .unwrap();
    let mut conn = db.connect().await;
    seed(&mut conn).await;

    // Batch 3 (orders 7–9) fails at order 8 after updating order 7.
    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Partial { step: 3, message, .. } if message.contains("cannot slug boom")),
        "{err}"
    );
    let mut expected = vec![Some(1); 6];
    expected.extend([None; 4]);
    assert_eq!(
        touched(&mut conn).await,
        expected,
        "batches 1 and 2 committed; batch 3 rolled back whole"
    );
    let (status, step, checkpoint) = ledger(&mut conn).await;
    assert_eq!((status.as_str(), step), ("partial", 2));
    assert_eq!(
        checkpoint.as_deref(),
        Some(json!(Uuid::from_u128(6).to_string()).to_string().as_str()),
        "the last committed batch's last key"
    );
    let p = plan(&mut conn, &set).await.unwrap();
    assert_eq!(
        p.entries[1].state,
        PlanState::Retry {
            previous: LedgerStatus::Partial,
            attempts: 1,
            step: 2
        }
    );

    // Fix the data (not the step, whose digest is fixed now) and rerun.
    sqlx::query("UPDATE orders SET title = 'Fine' WHERE title = 'boom'")
        .execute(&mut conn)
        .await
        .unwrap();
    migrate(db.options(), &set).await.unwrap();
    assert_eq!(
        touched(&mut conn).await,
        vec![Some(1); 10],
        "every order exactly once: nothing repeated, nothing skipped"
    );
    let slugs: Vec<Option<String>> = sqlx::query_scalar("SELECT slug FROM orders ORDER BY id")
        .fetch_all(&mut conn)
        .await
        .unwrap();
    assert_eq!(slugs[7].as_deref(), Some("fine"));
    let (status, step, checkpoint) = ledger(&mut conn).await;
    assert_eq!(
        (status.as_str(), step, checkpoint),
        ("applied", 5, None),
        "batches 3 (7–9), 4 (10) and 5 (empty) ran on resume"
    );
    assert!(migrate(db.options(), &set).await.unwrap().is_empty());
    drop(conn);
    db.drop().await;
}

fn stuck<'a>(_: &'a mut HistoricalTx<'_>, _: Option<&'a Value>) -> BatchFuture<'a> {
    Box::pin(async { Ok(Some(json!("same"))) })
}

#[tokio::test]
async fn a_batch_that_does_not_advance_is_stopped() {
    let db = TestDatabase::create().await;
    let set = history(DataStep::batched("stuck", "fn stuck() {}", stuck));
    let err = migrate(db.options(), &set).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::Partial { step: 2, message, .. } if message.contains("would never finish")),
        "{err}"
    );
    db.drop().await;
}

#[test]
fn batched_steps_run_alone_and_not_atomically() {
    let base = initial();
    let set = MigrationSet::new(vec![base.clone()]).unwrap();
    let batched = artifact::empty_batched(&set, "shop", "fill", "fill_slugs").unwrap();
    assert!(!batched.manifest.atomic);
    assert!(to_migration(&batched, Some(FILL)).problems().is_empty());

    let atomic = artifact::empty(&set, "shop", "fill", "fill_slugs").unwrap();
    let problems = to_migration(&atomic, Some(FILL)).problems();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("needs \"atomic\": false")),
        "{problems:?}"
    );

    let mut with_sql = to_migration(&batched, Some(FILL));
    with_sql.sql = "CREATE INDEX CONCURRENTLY IF NOT EXISTS orders_title_idx ON orders (title);\n";
    let problems = with_sql.problems();
    assert!(
        problems.iter().any(|p| p.contains("runs alone")),
        "{problems:?}"
    );

    let once = DataStep::once("fill_slugs", "fn x() {}", |_| Box::pin(async { Ok(()) }));
    let problems = to_migration(&batched, Some(once)).problems();
    assert!(
        problems.iter().any(|p| p.contains("or make it batched")),
        "{problems:?}"
    );
}

#[tokio::test]
async fn a_ledger_without_checkpoints_is_upgraded() {
    let db = TestDatabase::create().await;
    migrate(db.options(), &MigrationSet::new(vec![initial()]).unwrap())
        .await
        .unwrap();
    let mut conn = db.connect().await;
    sqlx::query("ALTER TABLE trestle_migrations DROP COLUMN checkpoint")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO orders (id, title) VALUES ($1, 'One')")
        .bind(Uuid::from_u128(1))
        .execute(&mut conn)
        .await
        .unwrap();
    migrate(db.options(), &history(FILL)).await.unwrap();
    assert_eq!(touched(&mut conn).await, [Some(1)]);
    drop(conn);
    db.drop().await;
}
