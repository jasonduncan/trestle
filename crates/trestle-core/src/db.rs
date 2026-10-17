//! Database error classification and transaction outcomes.
//!
//! Repositories take an explicit `&mut PgConnection` (usually a transaction)
//! and never fall back to a pool on their own (§6.3).

use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::{PgPool, Postgres, Transaction};

use crate::Problem;

/// The violated constraint's name, if `err` is a unique violation.
pub fn unique_violation(err: &sqlx::Error) -> Option<&str> {
    let db = err.as_database_error()?;
    if db.is_unique_violation() {
        db.constraint()
    } else {
        None
    }
}

/// Commits `tx`, classifying failure (§6.4):
///
/// * the server answered with an error → the transaction is known to be
///   rolled back, reported like any other database error;
/// * anything else (connection lost mid-COMMIT) → the outcome is unknown.
pub async fn commit(tx: Transaction<'_, Postgres>) -> Result<(), Problem> {
    tx.commit().await.map_err(|err| {
        if err.as_database_error().is_some() {
            Problem::from(err)
        } else {
            tracing::error!(error = %err, "commit failed; outcome unknown");
            Problem::outcome_unknown()
        }
    })
}

static READ_ONLY_READS: AtomicBool = AtomicBool::new(cfg!(debug_assertions));

/// Whether `Mode::Read` operations run in a read-only transaction, so a
/// read that tries to write fails instead of writing (ADR-018). On by
/// default in debug builds (tests and development), where a misdeclared
/// read is caught; off by default in release builds, because the extra
/// `BEGIN` costs about a fifth of read throughput. `trestle serve
/// --read-only-reads` turns it on.
pub fn read_only_reads() -> bool {
    READ_ONLY_READS.load(Ordering::Relaxed)
}

/// Sets [`read_only_reads`] for this process.
pub fn set_read_only_reads(on: bool) {
    READ_ONLY_READS.store(on, Ordering::Relaxed);
}

/// Begins a read-only transaction. Drop it when done: its ROLLBACK goes
/// out with the pool's own check when the connection returns, off the
/// request's path.
pub async fn begin_read(pool: &PgPool) -> Result<Transaction<'static, Postgres>, Problem> {
    Ok(pool.begin_with("BEGIN READ ONLY").await?)
}

impl From<sqlx::Error> for Problem {
    /// Unclassified database errors become a generic 500. Callers map
    /// expected constraint violations explicitly before using `?`.
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::PoolTimedOut => {
                tracing::warn!("database pool timed out");
                Problem::unavailable("database_unavailable")
            }
            // query_canceled: statement_timeout fired. The statement failed,
            // so its transaction cannot commit: the outcome is known.
            sqlx::Error::Database(db) if db.code().as_deref() == Some("57014") => {
                tracing::warn!("database statement timed out");
                Problem::unavailable("database_timeout")
            }
            // read_only_sql_transaction: a `Mode::Read` operation wrote.
            sqlx::Error::Database(db) if db.code().as_deref() == Some("25006") => {
                tracing::error!(error = %db.message(), "a read operation tried to write");
                Problem::internal()
            }
            // The schema fence refused this build before its transaction
            // began (ADR-017).
            sqlx::Error::Database(db) if db.code().as_deref() == Some(crate::fence::SQLSTATE) => {
                tracing::warn!(error = %db.message(), "write refused: schema fenced");
                crate::fence::fenced()
            }
            err => {
                tracing::error!(error = %err, "database error");
                Problem::internal()
            }
        }
    }
}
