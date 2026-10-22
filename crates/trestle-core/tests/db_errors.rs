//! A deadlock is a retryable 503 `database_conflict`, not a 500: PostgreSQL
//! rolled the transaction back, so nothing was written. Needs PostgreSQL
//! (`TRESTLE_TEST_DATABASE_URL`).

use http::{StatusCode, header};
use sqlx::Connection;
use trestle_core::Problem;
use trestle_historical::testing::TestDatabase;

#[tokio::test]
async fn a_deadlock_is_a_retryable_503() {
    let db = TestDatabase::create().await;
    let (mut a, mut b) = (db.connect().await, db.connect().await);
    sqlx::query("CREATE TABLE t (id int PRIMARY KEY, v int)")
        .execute(&mut a)
        .await
        .unwrap();
    sqlx::query("INSERT INTO t VALUES (1, 0), (2, 0)")
        .execute(&mut a)
        .await
        .unwrap();
    let update = "UPDATE t SET v = v + 1 WHERE id = $1";
    let mut ta = a.begin().await.unwrap();
    let mut tb = b.begin().await.unwrap();
    sqlx::query(update).bind(1).execute(&mut *ta).await.unwrap();
    sqlx::query(update).bind(2).execute(&mut *tb).await.unwrap();
    // Each now waits for the other's row; PostgreSQL cancels one.
    let (ra, rb) = tokio::join!(
        sqlx::query(update).bind(2).execute(&mut *ta),
        sqlx::query(update).bind(1).execute(&mut *tb),
    );
    let err = ra
        .err()
        .or(rb.err())
        .expect("one side is the deadlock victim");
    let problem = Problem::from(err);
    assert_eq!(problem.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem.code(), "database_conflict");
    let response = axum::response::IntoResponse::into_response(problem);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    drop((ta, tb));
    db.drop().await;
}
