//! Planning and applying migrations (§14.3).

use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, Executor, PgConnection};

use crate::data::HistoricalTx;
use crate::fence;
use crate::ledger::{self, LedgerRow, LedgerStatus};
use crate::schema::SchemaState;
use crate::{HistoryError, Migration, MigrationRef, MigrationSet};

/// Session advisory lock serializing runners. Stable forever.
const LOCK_KEY: i64 = 0x7472_6573_746c_6501;

/// Explicit deadlines. The first bounds waiting for another runner; the
/// others bound each migration statement.
const RUNNER_LOCK_WAIT: &str = "60s";
const DDL_LOCK_TIMEOUT: &str = "10s";
const STATEMENT_TIMEOUT: &str = "15min";
/// Defaults for each step of a non-atomic migration (design §6): a short
/// lock wait, and no statement limit because index builds take long.
const STEP_LOCK_TIMEOUT: &str = "5s";
const STEP_STATEMENT_TIMEOUT: &str = "0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanState {
    Applied,
    Pending,
    /// A previous attempt did not commit; safe to retry (transactional only).
    Retry {
        previous: LedgerStatus,
        attempts: i32,
        /// Where a non-atomic migration resumes (0-based); 0 otherwise.
        step: i32,
    },
}

#[derive(Debug, Clone)]
pub struct PlanEntry {
    pub migration: MigrationRef,
    pub state: PlanState,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    pub fn outstanding(&self) -> impl Iterator<Item = &PlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.state != PlanState::Applied)
    }
}

/// Compares the set with the ledger without changing anything. Fails on
/// changed applied migrations and on applied migrations this build lacks.
pub async fn plan(conn: &mut PgConnection, set: &MigrationSet) -> Result<Plan, HistoryError> {
    let rows = ledger::load(conn).await?;
    build_plan(set, &rows)
}

fn build_plan(set: &MigrationSet, rows: &[LedgerRow]) -> Result<Plan, HistoryError> {
    for row in rows {
        if set.is_foreign(&row.app) {
            continue;
        }
        let known = set.iter().any(|m| m.app == row.app && m.id == row.id);
        if !known && row.status == LedgerStatus::Applied {
            return Err(HistoryError::UnknownApplied(format!(
                "{}.{}",
                row.app, row.id
            )));
        }
    }
    let mut entries = Vec::new();
    for migration in set.iter() {
        let row = rows
            .iter()
            .find(|row| row.app == migration.app && row.id == migration.id);
        let state = match row {
            None => PlanState::Pending,
            Some(row) if row.status == LedgerStatus::Applied => {
                let current = migration.digest();
                if row.digest != current {
                    return Err(HistoryError::DigestMismatch {
                        migration: migration.key().to_string(),
                        recorded: row.digest.clone(),
                        current,
                    });
                }
                PlanState::Applied
            }
            Some(row) => {
                // A non-atomic migration that was attempted at all may have
                // left work behind (even its first step: an invalid index,
                // or a step that finished before its progress was recorded),
                // so its content must not change before it finishes. A
                // failed atomic migration rolled back and may be fixed.
                let current = migration.digest();
                let attempted = row.step > 0 || !migration.options().atomic;
                if attempted && row.digest != current {
                    return Err(HistoryError::DigestMismatch {
                        migration: migration.key().to_string(),
                        recorded: row.digest.clone(),
                        current,
                    });
                }
                PlanState::Retry {
                    previous: row.status,
                    attempts: row.attempts,
                    step: row.step,
                }
            }
        };
        entries.push(PlanEntry {
            migration: migration.key(),
            state,
        });
    }
    Ok(Plan { entries })
}

/// What happens to one migration during a run, for progress output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    Started(MigrationRef),
    Applied(MigrationRef),
    Failed {
        migration: MigrationRef,
        message: String,
    },
}

/// The outstanding migrations a run is about to apply, with the step each
/// resumes at. A [`RunConfig::guard`] sees this under the runner lock.
pub type Outstanding = [(MigrationRef, i32)];

/// Options for [`migrate_with`].
#[derive(Default)]
pub struct RunConfig {
    /// How long to wait for another runner's lock (Postgres interval
    /// syntax). Default 60s; exceeding it is [`HistoryError::LockTimeout`].
    pub lock_wait: Option<String>,
    /// Recorded in the ledger for audit (the reconciler's snapshot ID).
    pub backup_ref: Option<String>,
    /// Called under the lock with exactly what will run; an error stops
    /// the run before anything is applied (approval checks).
    #[allow(clippy::type_complexity)]
    pub guard: Option<Box<dyn Fn(&Outstanding) -> Result<(), HistoryError> + Send + Sync>>,
    /// Progress callback.
    #[allow(clippy::type_complexity)]
    pub events: Option<Box<dyn Fn(RunEvent) + Send + Sync>>,
    /// Apply even when the catalog differs from applied history. For a
    /// known, reviewed difference only.
    pub allow_drift: bool,
}

/// Applies outstanding migrations in order and returns what was applied.
///
/// Uses one dedicated connection for the whole run: the session advisory
/// lock belongs to that connection and is released when it closes.
pub async fn migrate(
    options: &PgConnectOptions,
    set: &MigrationSet,
) -> Result<Vec<MigrationRef>, HistoryError> {
    migrate_with(options, set, &RunConfig::default()).await
}

/// [`migrate`] with a lock wait, audit reference, guard and progress events.
pub async fn migrate_with(
    options: &PgConnectOptions,
    set: &MigrationSet,
    config: &RunConfig,
) -> Result<Vec<MigrationRef>, HistoryError> {
    let mut conn = PgConnection::connect_with(options)
        .await
        .map_err(|e| HistoryError::Unreachable(e.to_string()))?;
    let result = run_locked(&mut conn, set, config).await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(LOCK_KEY)
        .execute(&mut conn)
        .await;
    let _ = conn.close().await;
    result
}

async fn run_locked(
    conn: &mut PgConnection,
    set: &MigrationSet,
    config: &RunConfig,
) -> Result<Vec<MigrationRef>, HistoryError> {
    let wait = config.lock_wait.as_deref().unwrap_or(RUNNER_LOCK_WAIT);
    set_config(conn, "lock_timeout", wait).await?;
    let locked = sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *conn)
        .await;
    if let Err(e) = locked {
        let timed_out = e
            .as_database_error()
            .and_then(|d| d.code())
            .is_some_and(|c| c == "55P03");
        return Err(if timed_out {
            HistoryError::LockTimeout(wait.to_owned())
        } else {
            e.into()
        });
    }
    set_config(conn, "lock_timeout", DDL_LOCK_TIMEOUT).await?;
    set_config(conn, "statement_timeout", STATEMENT_TIMEOUT).await?;
    // Silence NOTICEs such as "relation already exists, skipping" from the
    // idempotent ledger DDL; warnings and errors still surface.
    set_config(conn, "client_min_messages", "warning").await?;

    ledger::ensure(conn).await?;
    // Validate the whole history before applying anything.
    let plan = plan(conn, set).await?;
    // The plan assumes the schema its applied history describes.
    if !config.allow_drift {
        let drift = drift(conn, set, &plan).await?;
        if !drift.is_empty() {
            return Err(HistoryError::Drift(drift.join("; ")));
        }
    }
    // Collected first: an iterator adapter held across `.await` makes the
    // future's `Send` bound unprovable (rust-lang/rust#102211).
    let outstanding: Vec<(MigrationRef, i32)> = plan
        .outstanding()
        .map(|e| match e.state {
            PlanState::Retry { step, .. } => (e.migration, step),
            _ => (e.migration, 0),
        })
        .collect();
    let mut problems = Vec::new();
    for (key, _) in &outstanding {
        if let Some(m) = set.get(*key) {
            problems.extend(m.problems());
        }
    }
    if !problems.is_empty() {
        return Err(HistoryError::State(problems.join("; ")));
    }
    if let Some(guard) = &config.guard {
        guard(&outstanding)?;
    }
    let emit = |event: RunEvent| {
        if let Some(events) = &config.events {
            events(event);
        }
    };
    // Data steps resolve models against history at their own step.
    let needs_history = outstanding
        .iter()
        .any(|(key, _)| set.get(*key).is_some_and(|m| m.data.is_some()));
    let states = if needs_history {
        Some(set.states()?)
    } else {
        None
    };
    let mut applied = Vec::new();
    for (key, step) in outstanding {
        let migration = set.get(key).expect("plan entries are drawn from the set");
        emit(RunEvent::Started(key));
        let backup = config.backup_ref.as_deref();
        let result = if migration.options().atomic {
            let state = states.as_ref().and_then(|states| {
                let index = set.iter().position(|m| m.key() == key)?;
                states.get(index)
            });
            apply(conn, migration, state, backup).await
        } else {
            apply_steps(conn, migration, step, backup).await
        };
        if let Err(e) = result {
            emit(RunEvent::Failed {
                migration: key,
                message: e.to_string(),
            });
            return Err(e);
        }
        emit(RunEvent::Applied(key));
        applied.push(migration.key());
    }
    Ok(applied)
}

/// Catalog differences from the schema applied history describes; `plan`
/// is this set's plan against the same database.
pub async fn drift(
    conn: &mut PgConnection,
    set: &MigrationSet,
    plan: &Plan,
) -> Result<Vec<String>, HistoryError> {
    let applied: Vec<MigrationRef> = plan
        .entries
        .iter()
        .filter(|e| e.state == PlanState::Applied)
        .map(|e| e.migration)
        .collect();
    if applied.is_empty() {
        return Ok(Vec::new());
    }
    // History without manifests cannot be replayed into a schema, so there
    // is nothing to compare against (`verify` reports it as unverified).
    let Ok(history) = set.history_of(|key| applied.contains(&key)) else {
        return Ok(Vec::new());
    };
    Ok(crate::verify::verify(conn, &history).await?.mismatches)
}

async fn apply(
    conn: &mut PgConnection,
    migration: &Migration,
    state: Option<&SchemaState>,
    backup_ref: Option<&str>,
) -> Result<(), HistoryError> {
    tracing::info!(migration = %migration.key(), "applying migration");
    ledger::mark_running(conn, migration, &migration.digest(), backup_ref).await?;

    let mut tx = conn.begin().await?;
    let options = migration.options();
    if let Some(v) = &options.lock_timeout {
        set_local(&mut tx, "lock_timeout", v).await?;
    }
    if let Some(v) = &options.statement_timeout {
        set_local(&mut tx, "statement_timeout", v).await?;
    }
    // Before any DDL: a write past the check may still need the tables
    // this migration is about to lock.
    if options.fence
        && let Err(err) = fence::lock(&mut tx).await
    {
        tx.rollback().await?;
        return Err(failed(conn, migration, describe(&err)).await?);
    }
    // In the migration's transaction (and under its fence), so the data it
    // checks is the data the migration acts on.
    match crate::impact::first_violated(&mut tx, &migration.preconditions()).await {
        Ok(None) => {}
        Ok(Some((name, violations))) => {
            tx.rollback().await?;
            return Err(refused(conn, migration, name, violations).await?);
        }
        Err(err) => {
            tx.rollback().await?;
            return Err(failed(conn, migration, describe(&err)).await?);
        }
    }
    // `Executor::execute` with a bare `&str` (no arguments) uses the simple
    // query protocol, so a migration may contain several statements.
    if let Err(err) = (&mut *tx).execute(migration.sql).await {
        tx.rollback().await?;
        return Err(failed(conn, migration, describe(&err)).await?);
    }
    if let Some(step) = migration.data {
        let outcome = match state {
            Some(state) => {
                let mut historical = HistoricalTx::new(&mut tx, state, migration.key().to_string());
                (step.run)(&mut historical).await
            }
            None => Err(HistoryError::State(format!(
                "{} needs replayed history",
                migration.key()
            ))),
        };
        if let Err(err) = outcome {
            let message = err.to_string().chars().take(500).collect::<String>();
            tx.rollback().await?;
            ledger::mark_failed(conn, migration, &message).await?;
            tracing::error!(migration = %migration.key(), error = %message, "data step failed");
            return Err(HistoryError::Failed {
                migration: migration.key().to_string(),
                message,
            });
        }
    }
    if options.fence {
        fence::record(&mut tx, migration).await?;
    }
    ledger::mark_applied(&mut tx, migration).await?;
    tx.commit().await?;
    Ok(())
}

/// Records a migration its preconditions stopped before it changed
/// anything.
async fn refused(
    conn: &mut PgConnection,
    migration: &Migration,
    name: String,
    violations: i64,
) -> Result<HistoryError, HistoryError> {
    let message = format!("precondition {name:?} has {violations} violation(s)");
    ledger::mark_failed(conn, migration, &message).await?;
    tracing::error!(migration = %migration.key(), %message, "migration refused");
    Ok(HistoryError::Precondition {
        migration: migration.key().to_string(),
        name,
        violations,
    })
}

/// Records an attempt whose transaction rolled back, and describes it.
async fn failed(
    conn: &mut PgConnection,
    migration: &Migration,
    message: String,
) -> Result<HistoryError, HistoryError> {
    ledger::mark_failed(conn, migration, &message).await?;
    tracing::error!(migration = %migration.key(), error = %message, "migration failed");
    Ok(HistoryError::Failed {
        migration: migration.key().to_string(),
        message,
    })
}

/// Runs a non-atomic migration one committed step at a time, starting at
/// `start` (0-based), recording progress in the ledger before and after
/// each step so a rerun resumes at the step that did not finish.
async fn apply_steps(
    conn: &mut PgConnection,
    migration: &Migration,
    start: i32,
    backup_ref: Option<&str>,
) -> Result<(), HistoryError> {
    let steps = migration.steps();
    tracing::info!(migration = %migration.key(), steps = steps.len(), start, "applying non-atomic migration");
    ledger::mark_running(conn, migration, &migration.digest(), backup_ref).await?;
    let options = migration.options();
    set_config(
        conn,
        "lock_timeout",
        options.lock_timeout.as_deref().unwrap_or(STEP_LOCK_TIMEOUT),
    )
    .await?;
    set_config(
        conn,
        "statement_timeout",
        options
            .statement_timeout
            .as_deref()
            .unwrap_or(STEP_STATEMENT_TIMEOUT),
    )
    .await?;
    // Checked once, before any step: a resumed migration already passed.
    if start == 0 {
        match crate::impact::first_violated(conn, &migration.preconditions()).await {
            Ok(None) => {}
            Ok(Some((name, violations))) => {
                set_config(conn, "lock_timeout", DDL_LOCK_TIMEOUT).await?;
                set_config(conn, "statement_timeout", STATEMENT_TIMEOUT).await?;
                return Err(refused(conn, migration, name, violations).await?);
            }
            Err(err) => {
                set_config(conn, "lock_timeout", DDL_LOCK_TIMEOUT).await?;
                set_config(conn, "statement_timeout", STATEMENT_TIMEOUT).await?;
                return Err(failed(conn, migration, describe(&err)).await?);
            }
        }
    }
    // Fenced before the first step, in its own transaction: older builds
    // stop writing before any of this migration's work begins.
    let result = match raise_fence(conn, migration, &options).await {
        Ok(()) => run_steps(conn, migration, &steps, start).await,
        Err(e) => Err((start, e)),
    };
    set_config(conn, "lock_timeout", DDL_LOCK_TIMEOUT).await?;
    set_config(conn, "statement_timeout", STATEMENT_TIMEOUT).await?;
    let (step, err) = match result {
        Ok(()) => {
            let n = i32::try_from(steps.len()).unwrap_or(i32::MAX);
            ledger::mark_step(conn, migration, n).await?;
            ledger::mark_applied(conn, migration).await?;
            return Ok(());
        }
        Err(e) => e,
    };
    let message = describe(&err);
    ledger::mark_partial(conn, migration, step, &message).await?;
    tracing::error!(migration = %migration.key(), step = step + 1, error = %message, "non-atomic migration stopped");
    Err(HistoryError::Partial {
        migration: migration.key().to_string(),
        step: step + 1,
        message,
    })
}

async fn raise_fence(
    conn: &mut PgConnection,
    migration: &Migration,
    options: &crate::RunOptions,
) -> Result<(), sqlx::Error> {
    if !options.fence {
        return Ok(());
    }
    let mut tx = conn.begin().await?;
    fence::lock(&mut tx).await?;
    fence::record(&mut tx, migration).await?;
    tx.commit().await
}

async fn run_steps(
    conn: &mut PgConnection,
    migration: &Migration,
    steps: &[String],
    start: i32,
) -> Result<(), (i32, sqlx::Error)> {
    let start = usize::try_from(start).unwrap_or(0);
    for (index, sql) in steps.iter().enumerate().skip(start) {
        let n = i32::try_from(index).unwrap_or(i32::MAX);
        let fail = |e| (n, e);
        ledger::mark_step(conn, migration, n).await.map_err(fail)?;
        if let Some(name) = crate::concurrent_index_name(sql) {
            drop_invalid_index(conn, &name).await.map_err(fail)?;
        }
        tracing::info!(migration = %migration.key(), step = n + 1, "running step");
        conn.execute(sql.as_str()).await.map_err(fail)?;
        ledger::mark_step(conn, migration, n + 1)
            .await
            .map_err(fail)?;
    }
    Ok(())
}

/// A failed `CREATE INDEX CONCURRENTLY` leaves an INVALID index behind,
/// which `IF NOT EXISTS` would then skip. Drop it so the retry rebuilds.
async fn drop_invalid_index(conn: &mut PgConnection, name: &str) -> Result<(), sqlx::Error> {
    let invalid: Option<bool> = sqlx::query_scalar(
        "SELECT NOT i.indisvalid FROM pg_index i
         JOIN pg_class c ON c.oid = i.indexrelid
         WHERE c.relname = $1 AND c.relnamespace = current_schema()::regnamespace",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    if invalid == Some(true) {
        tracing::warn!(
            index = name,
            "dropping invalid index left by an earlier attempt"
        );
        conn.execute(format!("DROP INDEX CONCURRENTLY IF EXISTS \"{name}\"").as_str())
            .await?;
    }
    Ok(())
}

async fn set_local(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    name: &str,
    value: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config($1, $2, true)")
        .bind(name)
        .bind(value)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn set_config(conn: &mut PgConnection, name: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config($1, $2, false)")
        .bind(name)
        .bind(value)
        .execute(conn)
        .await?;
    Ok(())
}

fn describe(err: &sqlx::Error) -> String {
    let message = match err.as_database_error() {
        Some(db) => match db.code() {
            Some(code) => format!("{} (SQLSTATE {code})", db.message()),
            None => db.message().to_owned(),
        },
        None => err.to_string(),
    };
    message.chars().take(500).collect()
}

/// Compile-time guard: public futures must stay `Send` so they can be
/// spawned and awaited inside axum handlers.
#[allow(dead_code)]
fn assert_public_futures_are_send(
    options: &PgConnectOptions,
    conn: &mut PgConnection,
    set: &MigrationSet,
) {
    fn is_send<T: Send>(_: T) {}
    is_send(migrate(options, set));
    is_send(plan(conn, set));
    is_send(ledger::missing(conn, &[]));
}
