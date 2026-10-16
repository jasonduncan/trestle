//! Database error classification and transaction outcomes.
//!
//! Repositories take an explicit `&mut PgConnection` (usually a transaction)
//! and never fall back to a pool on their own (§6.3).

use sqlx::{Postgres, Transaction};

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
            err => {
                tracing::error!(error = %err, "database error");
                Problem::internal()
            }
        }
    }
}
