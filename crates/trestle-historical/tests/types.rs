//! ADR-023: decimal, date, instant and duration columns. The planner
//! renders them, `verify` checks them (numeric precision included), and
//! `HistoricalTx` reads and writes them in their contract forms. Needs
//! PostgreSQL (`TRESTLE_TEST_DATABASE_URL`).

use serde_json::{Value, json};
use trestle_historical::artifact::{self, Artifact};
use trestle_historical::data::{DataFuture, DataStep, HistoricalTx};
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::{Column, ColumnType, Model, desired};
use trestle_historical::sql::literal;
use trestle_historical::testing::TestDatabase;
use trestle_historical::verify::verify;
use trestle_historical::{HistoryError, Migration, MigrationRef, MigrationSet, migrate};

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

fn invoices() -> Vec<Model> {
    vec![
        Model::new("billing.invoice")
            .table("invoices")
            .field(Column::uuid("id").primary_key())
            .field(Column::decimal("amount", 10, 2))
            .field(Column::date("due").nullable())
            .field(Column::timestamptz("sent_at").nullable())
            .field(Column::duration("term").nullable()),
    ]
}

fn initial() -> Artifact {
    let empty = MigrationSet::new(Vec::new()).unwrap();
    let plan = diff(
        &empty.history().unwrap(),
        &desired(&invoices()).unwrap(),
        &PlanOptions::default(),
    );
    artifact::generate(&empty, &plan, "initial")
        .unwrap()
        .remove(0)
}

const ID: &str = "00000000-0000-4000-8000-000000000001";

/// Writes every type through the SDK, then reads it back.
fn round_trip<'a>(tx: &'a mut HistoricalTx<'_>) -> DataFuture<'a> {
    Box::pin(async move {
        tx.insert(
            "billing.invoice",
            &[
                ("id", json!(ID)),
                ("amount", json!("1234.50")),
                ("due", json!("2026-12-31")),
                ("sent_at", json!("2026-10-17T09:30:00.25+02:00")),
                ("term", json!("P30DT12H")),
            ],
        )
        .await?;
        let rows = tx
            .batch(
                "billing.invoice",
                &["amount", "due", "sent_at", "term"],
                None,
                10,
            )
            .await?;
        let row = &rows[0];
        let got: Vec<&Value> = ["amount", "due", "sent_at", "term"]
            .iter()
            .map(|k| &row[*k])
            .collect();
        let want = [
            json!("1234.50"),
            json!("2026-12-31"),
            json!("2026-10-17T07:30:00.25Z"),
            json!("P30DT12H"),
        ];
        if got.iter().zip(&want).any(|(g, w)| *g != w) {
            return Err(HistoryError::State(format!("read back {got:?}")));
        }
        let changed = tx
            .update(
                "billing.invoice",
                &json!(ID),
                &[("amount", json!("0.10")), ("term", json!("PT0S"))],
            )
            .await?;
        assert_eq!(changed, 1);
        let rows = tx
            .batch("billing.invoice", &["amount", "term"], None, 10)
            .await?;
        if rows[0]["amount"] != json!("0.10") || rows[0]["term"] != json!("PT0S") {
            return Err(HistoryError::State(format!("updated to {:?}", rows[0])));
        }
        // A value that does not fit the column is refused before SQL.
        let refused = tx
            .update("billing.invoice", &json!(ID), &[("amount", json!(12.5))])
            .await;
        if refused.is_ok() {
            return Err(HistoryError::State(
                "a JSON number was bound as a decimal".into(),
            ));
        }
        Ok(())
    })
}

#[tokio::test]
async fn columns_render_verify_and_round_trip_through_the_sdk() {
    let first = initial();
    assert!(
        first.sql.contains("amount numeric(10,2) NOT NULL"),
        "{}",
        first.sql
    );
    assert!(first.sql.contains("due date"));
    assert!(first.sql.contains("sent_at timestamptz"));
    assert!(first.sql.contains("term interval"));

    let base = to_migration(&first, None);
    let set = MigrationSet::new(vec![base.clone()]).unwrap();
    let data = artifact::empty(&set, "billing", "round_trip", "round_trip").unwrap();
    let step = DataStep::once("round_trip", "fn round_trip() {}", round_trip);
    let set = MigrationSet::new(vec![base, to_migration(&data, Some(step))]).unwrap();

    let db = TestDatabase::create().await;
    migrate(db.options(), &set)
        .await
        .expect("the data step's checks pass");
    let mut conn = db.connect().await;
    // sqlx sessions start in UTC; another zone must not change the form.
    sqlx::query("SET TIME ZONE 'America/New_York'")
        .execute(&mut conn)
        .await
        .unwrap();
    let sent: String = sqlx::query_scalar(&format!(
        "SELECT {} FROM invoices",
        trestle_historical::sql::iso_instant("'2026-10-17T07:30:00.25Z'::timestamptz")
    ))
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(sent, "2026-10-17T07:30:00.25Z");
    let report = verify(&mut conn, &set.history().unwrap()).await.unwrap();
    assert!(report.mismatches.is_empty(), "{:?}", report.mismatches);

    // A wider numeric is drift: precision is part of the type.
    sqlx::query("ALTER TABLE invoices ALTER COLUMN amount TYPE numeric(12,2)")
        .execute(&mut conn)
        .await
        .unwrap();
    let report = verify(&mut conn, &set.history().unwrap()).await.unwrap();
    assert_eq!(
        report.mismatches,
        ["billing.invoice.amount: invoices.amount is numeric(12,2), expected numeric(10,2)"]
    );
    drop(conn);
    db.drop().await;
}

#[test]
fn literals_are_checked_against_the_column() {
    let numeric = ColumnType::Numeric {
        precision: 10,
        scale: 2,
    };
    assert_eq!(literal(&json!("12.50"), numeric).unwrap(), "'12.50'");
    assert!(literal(&json!("1.234"), numeric).is_err());
    assert!(literal(&json!(12.5), numeric).is_err());
    assert_eq!(
        literal(&json!("2026-02-28"), ColumnType::Date).unwrap(),
        "'2026-02-28'"
    );
    assert!(literal(&json!("2026-02-30"), ColumnType::Date).is_err());
    assert_eq!(
        literal(&json!("2026-10-17T09:30:00+02:00"), ColumnType::TimestampTz).unwrap(),
        "'2026-10-17T07:30:00Z'"
    );
    assert_eq!(
        literal(&json!("PT30M"), ColumnType::Interval).unwrap(),
        "'PT30M'"
    );
    assert!(literal(&json!("P1Y"), ColumnType::Interval).is_err());
    assert_eq!(numeric.sql(), "numeric(10,2)");
}
