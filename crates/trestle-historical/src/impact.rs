//! What a pending migration would find (ADR-019): its preconditions,
//! evaluated, and the rows its operations would touch.
//!
//! Every query here runs in its own `BEGIN READ ONLY` transaction under a
//! statement timeout and is rolled back, so reporting changes nothing and a
//! slow or failing count only leaves its figure unknown. The figures are
//! taken against the database as it is now; a step that depends on an
//! earlier pending step may not be measurable yet.

use serde::Serialize;
use sqlx::{Connection, PgConnection};

use crate::schema::{SchemaState, StateOp};
use crate::sql::ident;
use crate::{Migration, Precondition};

/// One precondition as evaluated now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evaluated {
    pub name: String,
    /// Rows that break it; `None` when it could not be evaluated now.
    pub violations: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One operation's reach into existing data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Impact {
    pub model: String,
    pub table: String,
    /// `drop_table`, `drop_column`, `not_null`, `unique`, `backfill`,
    /// `add_column`, `index`, `check` or `rename`.
    pub change: &'static str,
    /// The planner's estimate of the table's rows (`pg_class.reltuples`);
    /// `None` before the table was ever analyzed.
    pub rows: Option<i64>,
    /// An exact count, where it decides what the step does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected: Option<i64>,
    /// What `affected` counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning: Option<&'static str>,
    /// As the data stands, the step would fail.
    pub blocks: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Runs one counting query read-only. `Err` carries a short reason.
async fn count(conn: &mut PgConnection, sql: &str, timeout: &str) -> Result<i64, String> {
    let attempt = async {
        let mut tx = conn.begin_with("BEGIN READ ONLY").await?;
        sqlx::query("SELECT set_config('statement_timeout', $1, true)")
            .bind(timeout)
            .execute(&mut *tx)
            .await?;
        let value: Option<i64> = sqlx::query_scalar(sql).fetch_one(&mut *tx).await?;
        tx.rollback().await?;
        Ok::<_, sqlx::Error>(value.unwrap_or(0))
    };
    attempt
        .await
        .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
            Some(code) if code == "57014" => format!("timed out after {timeout}"),
            Some(code) if code == "42P01" || code == "42703" => {
                "not measurable until earlier steps apply".to_owned()
            }
            _ => {
                let message = e.to_string();
                message.chars().take(200).collect()
            }
        })
}

/// Evaluates `migration`'s preconditions against the database as it is.
pub async fn preconditions(
    conn: &mut PgConnection,
    migration: &Migration,
    timeout: &str,
) -> Vec<Evaluated> {
    let mut evaluated = Vec::new();
    for check in migration.preconditions() {
        let (violations, note) = match count(conn, &check.sql(), timeout).await {
            Ok(n) => (Some(n), None),
            Err(note) => (None, Some(note)),
        };
        evaluated.push(Evaluated {
            name: check.name,
            violations,
            note,
        });
    }
    evaluated
}

/// The first precondition that does not hold, inside the runner's own
/// transaction (so it sees what the migration will act on).
pub(crate) async fn first_violated(
    conn: &mut PgConnection,
    checks: &[Precondition],
) -> Result<Option<(String, i64)>, sqlx::Error> {
    for check in checks {
        let violations: Option<i64> = sqlx::query_scalar(&check.sql())
            .fetch_one(&mut *conn)
            .await?;
        let violations = violations.unwrap_or(0);
        if violations != 0 {
            return Ok(Some((check.name.clone(), violations)));
        }
    }
    Ok(None)
}

/// What `ops` (one migration's operations, applied to `before`) would do to
/// existing rows. Tables the migration creates have none.
pub async fn impact(
    conn: &mut PgConnection,
    migration: &Migration,
    ops: &[StateOp],
    before: &SchemaState,
    timeout: &str,
) -> Vec<Impact> {
    let backfilled: Vec<(&str, &str)> = ops
        .iter()
        .filter_map(|op| match op {
            StateOp::Backfill { model, field, .. } => Some((model.as_str(), field.as_str())),
            _ => None,
        })
        .collect();
    // A data step can fix anything, so nothing is called blocking past one.
    let repairs = migration.data.is_some();
    let mut out = Vec::new();
    for op in ops {
        let Some(model) = before.model(op.model()) else {
            continue;
        };
        let Ok(table) = ident(&model.table).map(|t| format!("\"{t}\"")) else {
            continue;
        };
        let column = |field: &str| {
            model
                .field(field)
                .and_then(|f| ident(&f.column).ok())
                .map(|c| format!("\"{c}\""))
        };
        // (change, exact count query, what it counts, blocks when > 0)
        let (change, exact, meaning, blocking) = match op {
            StateOp::CreateModel { .. }
            | StateOp::RemoveUnique { .. }
            | StateOp::RemoveIndex { .. } => continue,
            StateOp::RemoveModel { .. } => (
                "drop_table",
                Some(format!("SELECT count(*) FROM {table}")),
                Some("rows deleted"),
                false,
            ),
            StateOp::RemoveField { field, .. } => (
                "drop_column",
                column(field)
                    .map(|c| format!("SELECT count(*) FROM {table} WHERE {c} IS NOT NULL")),
                Some("values deleted"),
                false,
            ),
            StateOp::AlterNullability {
                field,
                nullable: false,
                ..
            } => (
                "not_null",
                column(field).map(|c| format!("SELECT count(*) FROM {table} WHERE {c} IS NULL")),
                Some("rows that are null"),
                !repairs && !backfilled.contains(&(model.id.as_str(), field.as_str())),
            ),
            StateOp::AlterNullability { .. } => ("not_null", None, None, false),
            StateOp::Backfill { field, .. } => (
                "backfill",
                column(field).map(|c| format!("SELECT count(*) FROM {table} WHERE {c} IS NULL")),
                Some("rows to fill"),
                false,
            ),
            StateOp::AddUnique { unique, .. } => {
                let columns: Option<Vec<String>> =
                    unique.fields.iter().map(|f| column(f)).collect();
                (
                    "unique",
                    columns.map(|cols| {
                        format!(
                            "SELECT count(*) FROM (SELECT 1 FROM {table} WHERE {} GROUP BY {} HAVING count(*) > 1) d",
                            cols.iter()
                                .map(|c| format!("{c} IS NOT NULL"))
                                .collect::<Vec<_>>()
                                .join(" AND "),
                            cols.join(", ")
                        )
                    }),
                    Some("duplicate groups"),
                    !repairs,
                )
            }
            StateOp::AddField { .. } => ("add_column", None, None, false),
            StateOp::AddIndex { .. } => ("index", None, None, false),
            StateOp::AlterCheck { .. } => ("check", None, None, false),
            StateOp::ValidateConstraint { .. } => ("validate", None, None, false),
            StateOp::RenameTable { .. } | StateOp::RenameColumn { .. } => {
                ("rename", None, None, false)
            }
        };
        let rows = estimate(conn, &model.table).await;
        let (affected, note) = match exact {
            Some(sql) => match count(conn, &sql, timeout).await {
                Ok(n) => (Some(n), None),
                Err(note) => (None, Some(note)),
            },
            None => (None, None),
        };
        out.push(Impact {
            model: model.id.clone(),
            table: model.table.clone(),
            change,
            rows,
            affected,
            meaning: meaning.filter(|_| affected.is_some() || note.is_some()),
            blocks: blocking && affected.is_some_and(|n| n > 0),
            note,
        });
    }
    out
}

/// `pg_class.reltuples`; `None` for a table never analyzed (or absent).
async fn estimate(conn: &mut PgConnection, table: &str) -> Option<i64> {
    let rows: Option<i64> =
        sqlx::query_scalar("SELECT reltuples::bigint FROM pg_class WHERE oid = to_regclass($1)")
            .bind(table)
            .fetch_optional(conn)
            .await
            .ok()
            .flatten();
    rows.filter(|n| *n >= 0)
}
