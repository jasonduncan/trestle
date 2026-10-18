//! Migration history for Trestle (design doc §13–14).
//!
//! A [`Migration`] is a committed, immutable artifact: stable app/ID,
//! explicit dependencies, forward SQL, and (format 2) a [`Manifest`] with
//! the logical state deltas that SQL performs plus an optional frozen data
//! step. Migration crates depend on this crate only, never on current
//! application code, so history replays after current model types are
//! renamed or removed (I08).
//!
//! Migrations are transactional: each runs in one transaction that also
//! marks its ledger row `applied`, so the ledger and the schema cannot
//! disagree. A row left `running` or `failed` means the migration rolled
//! back and is safe to retry.

#![forbid(unsafe_code)]

pub mod artifact;
pub mod classify;
pub mod data;
pub mod fence;
mod ledger;
pub mod load;
pub mod planner;
mod runner;
pub mod schema;
pub mod sql;
#[cfg(feature = "testing")]
pub mod testing;
pub mod verify;

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use data::{DataStep, HistoricalTx};
pub use ledger::{LedgerRow, LedgerStatus, exists as ledger_exists, load as ledger_rows, missing};
pub use runner::{
    Outstanding, Plan, PlanEntry, PlanState, RunConfig, RunEvent, migrate, migrate_with, plan,
};
use schema::{SchemaState, StateOp};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MigrationRef {
    pub app: &'static str,
    pub id: &'static str,
}

impl MigrationRef {
    pub const fn new(app: &'static str, id: &'static str) -> Self {
        Self { app, id }
    }
}

impl fmt::Display for MigrationRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.app, self.id)
    }
}

/// The logical side of a migration artifact (`<app>/<id>.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// 1: legacy, the digest covers the SQL only. 2: the digest covers the
    /// SQL, this manifest and the data step's source.
    pub format: u32,
    pub app: String,
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// What the SQL does to managed state. Empty means "no managed state
    /// change" (§14.3: raw SQL must declare one or the other).
    #[serde(default)]
    pub operations: Vec<StateOp>,
    /// Name of the data step run after the SQL, in the same transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_destructive: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// `false`: the SQL runs outside a transaction, one statement per step
    /// (separated by `-- trestle:step` lines), each idempotent. Omitted
    /// when `true`, so older manifests keep their digests.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub atomic: bool,
    /// Per-migration `lock_timeout` (e.g. `"5s"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_timeout: Option<String>,
    /// Per-migration `statement_timeout` (e.g. `"30min"`, `"0"` for none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout: Option<String>,
    /// `true`: once applied, builds that do not contain this migration may
    /// not write ([`fence`], ADR-017). Omitted when `false`, so older
    /// manifests keep their digests.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fence: bool,
}

fn yes() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Separates the statements of a non-atomic migration.
pub const STEP_MARKER: &str = "-- trestle:step";

/// How the runner executes a migration, from its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOptions {
    pub atomic: bool,
    pub lock_timeout: Option<String>,
    pub statement_timeout: Option<String>,
    /// Raises the schema fence for its app ([`fence`]).
    pub fence: bool,
}

/// Splits SQL at `-- trestle:step` lines. Steps that contain only
/// comments and whitespace are dropped.
pub fn split_steps(sql: &str) -> Vec<String> {
    let mut steps = vec![String::new()];
    for line in sql.lines() {
        if line.trim() == STEP_MARKER {
            steps.push(String::new());
        } else {
            let last = steps.last_mut().expect("never empty");
            last.push_str(line);
            last.push('\n');
        }
    }
    steps
        .into_iter()
        .filter(|s| !strip_comments(s).trim().is_empty())
        .collect()
}

fn strip_comments(sql: &str) -> String {
    sql.lines()
        .map(|l| match l.find("--") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Statements in `sql` (semicolons outside quotes and comments), ignoring
/// a trailing semicolon.
pub fn statement_count(sql: &str) -> usize {
    let code = strip_comments(sql);
    let mut count = 0;
    let mut quote: Option<char> = None;
    let mut pending = false;
    for c in code.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                pending = true;
            }
            None if c == ';' => {
                if pending {
                    count += 1;
                }
                pending = false;
            }
            None if !c.is_whitespace() => pending = true,
            None => {}
        }
    }
    count + usize::from(pending)
}

/// The first identifier after `CREATE [UNIQUE] INDEX CONCURRENTLY IF NOT
/// EXISTS`, for invalid-index recovery. `None` for other statements.
pub fn concurrent_index_name(step: &str) -> Option<String> {
    let code = strip_comments(step);
    let words: Vec<&str> = code.split_whitespace().collect();
    let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
    let mut i = 0;
    let mut expect = |w: &str| {
        if upper.get(i).is_some_and(|u| u == w) {
            i += 1;
            true
        } else {
            false
        }
    };
    if !expect("CREATE") {
        return None;
    }
    expect("UNIQUE");
    if !(expect("INDEX")
        && expect("CONCURRENTLY")
        && expect("IF")
        && expect("NOT")
        && expect("EXISTS"))
    {
        return None;
    }
    let name = words.get(i)?.trim_matches('"');
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    valid.then(|| name.to_owned())
}

/// A timeout value Postgres accepts: digits with an optional unit.
fn valid_timeout(v: &str) -> bool {
    let digits = v.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = &v[digits.len()..];
    !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(unit, "" | "ms" | "s" | "min" | "h")
}

#[derive(Debug, Clone)]
pub struct Migration {
    pub app: &'static str,
    pub id: &'static str,
    pub depends_on: &'static [MigrationRef],
    /// Forward SQL. Must not contain its own transaction control.
    pub sql: &'static str,
    /// Manifest JSON. Required for planning and for data steps.
    pub manifest: Option<&'static str>,
    pub data: Option<DataStep>,
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Keys sorted at every level, so formatting never changes a digest.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

impl Migration {
    pub fn key(&self) -> MigrationRef {
        MigrationRef::new(self.app, self.id)
    }

    fn manifest_value(&self) -> Option<Value> {
        self.manifest.and_then(|m| serde_json::from_str(m).ok())
    }

    /// How to run it. A migration without a manifest is atomic.
    pub fn options(&self) -> RunOptions {
        let manifest = self.manifest_value().unwrap_or(Value::Null);
        let text = |k: &str| manifest[k].as_str().map(str::to_owned);
        RunOptions {
            atomic: manifest["atomic"].as_bool().unwrap_or(true),
            lock_timeout: text("lock_timeout"),
            statement_timeout: text("statement_timeout"),
            fence: manifest["fence"].as_bool().unwrap_or(false),
        }
    }

    /// The statements a non-atomic migration runs, in order.
    pub fn steps(&self) -> Vec<String> {
        split_steps(self.sql)
    }

    /// Rules for running this migration safely (design §6), checked by
    /// `check` and again by the runner before anything is applied.
    pub fn problems(&self) -> Vec<String> {
        let key = self.key();
        let options = self.options();
        let mut problems = Vec::new();
        for (name, value) in [
            ("lock_timeout", &options.lock_timeout),
            ("statement_timeout", &options.statement_timeout),
        ] {
            if let Some(v) = value
                && !valid_timeout(v)
            {
                problems.push(format!(
                    "{key}: {name} {v:?} must be a number with an optional unit (ms, s, min, h)"
                ));
            }
        }
        let marked = self.sql.lines().any(|l| l.trim() == STEP_MARKER);
        if options.atomic {
            if marked {
                problems.push(format!(
                    "{key}: `{STEP_MARKER}` lines are only for migrations with \"atomic\": false"
                ));
            }
            return problems;
        }
        if self.data.is_some() {
            problems.push(format!(
                "{key}: a non-atomic migration cannot have a data step; put the data step in its own atomic migration"
            ));
        }
        let steps = self.steps();
        if steps.is_empty() {
            problems.push(format!(
                "{key}: a non-atomic migration needs at least one step"
            ));
        }
        for (n, step) in steps.iter().enumerate() {
            let n = n + 1;
            if statement_count(step) != 1 {
                problems.push(format!(
                    "{key} step {n}: one statement per step; separate statements with `{STEP_MARKER}` lines"
                ));
                continue;
            }
            let upper = strip_comments(step).to_ascii_uppercase();
            let words: Vec<&str> = upper.split_whitespace().collect();
            let has = |seq: &[&str]| words.windows(seq.len()).any(|w| w == seq);
            let concurrent = has(&["CONCURRENTLY"]);
            let validate = has(&["VALIDATE", "CONSTRAINT"]);
            if !concurrent && !validate {
                problems.push(format!(
                    "{key} step {n}: only CONCURRENTLY index statements and VALIDATE CONSTRAINT run \
                     outside a transaction; put other DDL in an atomic migration"
                ));
            } else if words.first() == Some(&"CREATE") && !has(&["IF", "NOT", "EXISTS"]) {
                problems.push(format!(
                    "{key} step {n}: use CREATE INDEX CONCURRENTLY IF NOT EXISTS so a retry is safe"
                ));
            } else if words.first() == Some(&"DROP") && !has(&["IF", "EXISTS"]) {
                problems.push(format!(
                    "{key} step {n}: use DROP INDEX CONCURRENTLY IF EXISTS so a retry is safe"
                ));
            }
        }
        problems
    }

    /// Content digest recorded in the ledger. Applied history is immutable:
    /// a changed digest for an applied migration blocks the runner.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        match self.manifest_value() {
            Some(manifest) if manifest["format"] != 1 => {
                hasher.update(b"trestle.migration/2\0");
                hasher.update(serde_json::to_vec(&canonical(&manifest)).unwrap_or_default());
                hasher.update([0]);
                hasher.update(self.sql.as_bytes());
                hasher.update([0]);
                if let Some(data) = &self.data {
                    hasher.update(data.name.as_bytes());
                    hasher.update([0]);
                    hasher.update(data.source.as_bytes());
                }
            }
            _ => {
                hasher.update(b"trestle.migration/1\0");
                hasher.update(self.sql.as_bytes());
            }
        }
        hex(hasher.finalize())
    }
}

/// A validated set of migrations across apps, in dependency order.
#[derive(Debug, Clone)]
pub struct MigrationSet {
    migrations: Vec<Migration>,
    manifests: Vec<Option<Manifest>>,
    /// Apps whose ledger rows belong to another project sharing the
    /// database (see [`MigrationSet::with_foreign_apps`]).
    foreign: Vec<String>,
}

impl MigrationSet {
    /// Validates a set whose order already satisfies every dependency.
    pub fn new(migrations: Vec<Migration>) -> Result<Self, HistoryError> {
        let mut seen = HashSet::new();
        let mut manifests = Vec::new();
        for migration in &migrations {
            let key = migration.key();
            if !valid_name(key.app) || !valid_name(key.id) {
                return Err(HistoryError::InvalidName(key.to_string()));
            }
            for dependency in migration.depends_on {
                if !seen.contains(dependency) {
                    let known_later = migrations.iter().any(|m| m.key() == *dependency);
                    return Err(HistoryError::Dependency {
                        migration: key.to_string(),
                        dependency: dependency.to_string(),
                        reason: if known_later {
                            "must come earlier"
                        } else {
                            "is not defined"
                        },
                    });
                }
            }
            if !seen.insert(key) {
                return Err(HistoryError::Duplicate(key.to_string()));
            }
            manifests.push(parse_manifest(migration)?);
        }
        Ok(Self {
            migrations,
            manifests,
            foreign: Vec::new(),
        })
    }

    /// Orders migrations by dependency (ties by app and ID), then validates.
    pub fn ordered(mut unordered: Vec<Migration>) -> Result<Self, HistoryError> {
        unordered.sort_by_key(|m| m.key());
        let mut ordered: Vec<Migration> = Vec::new();
        while !unordered.is_empty() {
            let ready = unordered.iter().position(|m| {
                m.depends_on.iter().all(|d| {
                    ordered.iter().any(|o| o.key() == *d)
                        || !unordered.iter().any(|u| u.key() == *d)
                })
            });
            match ready {
                Some(index) => ordered.push(unordered.remove(index)),
                None => {
                    let stuck: Vec<String> =
                        unordered.iter().map(|m| m.key().to_string()).collect();
                    return Err(HistoryError::State(format!(
                        "dependency cycle among {}",
                        stuck.join(", ")
                    )));
                }
            }
        }
        Self::new(ordered)
    }

    /// Declares apps whose migrations another project applies to the same
    /// database, or that this project once had and has handed over. Their
    /// ledger rows are neither "unknown applied" nor drift: plans, `migrate`
    /// and `verify` leave them alone. An app this set has migrations for
    /// cannot be foreign.
    pub fn with_foreign_apps<I, A>(mut self, apps: I) -> Result<Self, HistoryError>
    where
        I: IntoIterator<Item = A>,
        A: Into<String>,
    {
        for app in apps {
            let app = app.into();
            if self.migrations.iter().any(|m| m.app == app) {
                return Err(HistoryError::State(format!(
                    "app {app:?} has migrations in this project, so it cannot be foreign"
                )));
            }
            if !self.foreign.contains(&app) {
                self.foreign.push(app);
            }
        }
        Ok(self)
    }

    /// Whether ledger rows of `app` belong to another project.
    pub fn is_foreign(&self, app: &str) -> bool {
        self.foreign.iter().any(|a| a == app)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Migration> {
        self.migrations.iter()
    }

    pub fn get(&self, key: MigrationRef) -> Option<&Migration> {
        self.migrations.iter().find(|m| m.key() == key)
    }

    pub fn manifest(&self, key: MigrationRef) -> Option<&Manifest> {
        let index = self.migrations.iter().position(|m| m.key() == key)?;
        self.manifests[index].as_ref()
    }

    /// State after each migration, in order (H at every step).
    pub fn states(&self) -> Result<Vec<SchemaState>, HistoryError> {
        let mut state = SchemaState::default();
        let mut states = Vec::new();
        for (migration, manifest) in self.migrations.iter().zip(&self.manifests) {
            let manifest = manifest.as_ref().ok_or_else(|| {
                HistoryError::State(format!(
                    "{} has no manifest; declare its state delta (or an empty one) before planning",
                    migration.key()
                ))
            })?;
            for op in &manifest.operations {
                state
                    .apply(op)
                    .map_err(|e| HistoryError::State(format!("{}: {e}", migration.key())))?;
            }
            states.push(state.clone());
        }
        Ok(states)
    }

    /// Historical state after all migrations (H).
    pub fn history(&self) -> Result<SchemaState, HistoryError> {
        Ok(self.states()?.pop().unwrap_or_default())
    }

    /// State after only the migrations `applied` accepts, in order: the
    /// expected state of a database whose ledger records exactly those.
    pub fn history_of(
        &self,
        applied: impl Fn(MigrationRef) -> bool,
    ) -> Result<SchemaState, HistoryError> {
        let mut state = SchemaState::default();
        for (migration, manifest) in self.migrations.iter().zip(&self.manifests) {
            if !applied(migration.key()) {
                continue;
            }
            let manifest = manifest.as_ref().ok_or_else(|| {
                HistoryError::State(format!("{} has no manifest", migration.key()))
            })?;
            for op in &manifest.operations {
                state
                    .apply(op)
                    .map_err(|e| HistoryError::State(format!("{}: {e}", migration.key())))?;
            }
        }
        Ok(state)
    }

    /// Migrations of `app` that no other migration of `app` depends on.
    /// More than one means unmerged branches (§14.3).
    pub fn leaves(&self, app: &str) -> Vec<MigrationRef> {
        let of_app: Vec<&Migration> = self.migrations.iter().filter(|m| m.app == app).collect();
        of_app
            .iter()
            .filter(|m| !of_app.iter().any(|o| o.depends_on.contains(&m.key())))
            .map(|m| m.key())
            .collect()
    }

    /// Next four-digit migration number for `app`.
    pub fn next_number(&self, app: &str) -> u32 {
        self.migrations
            .iter()
            .filter(|m| m.app == app)
            .filter_map(|m| m.id.get(..4)?.parse::<u32>().ok())
            .max()
            .map_or(1, |n| n + 1)
    }
}

fn parse_manifest(migration: &Migration) -> Result<Option<Manifest>, HistoryError> {
    let key = migration.key();
    let Some(text) = migration.manifest else {
        if migration.data.is_some() {
            return Err(HistoryError::State(format!(
                "{key} has a data step but no manifest"
            )));
        }
        return Ok(None);
    };
    let manifest: Manifest = serde_json::from_str(text)
        .map_err(|e| HistoryError::State(format!("{key}: invalid manifest: {e}")))?;
    let problem = |text: String| Err(HistoryError::State(format!("{key}: {text}")));
    if manifest.app != migration.app || manifest.id != migration.id {
        return problem(format!("manifest names {}.{}", manifest.app, manifest.id));
    }
    let declared: Vec<String> = migration
        .depends_on
        .iter()
        .map(ToString::to_string)
        .collect();
    if manifest.depends_on != declared {
        return problem("manifest dependencies differ from the registered ones".into());
    }
    if !matches!(manifest.format, 1 | 2) {
        return problem(format!("unknown manifest format {}", manifest.format));
    }
    if manifest.data.as_deref() != migration.data.map(|d| d.name) {
        return problem("manifest data step and registered data step differ".into());
    }
    for op in &manifest.operations {
        if op.is_destructive() {
            let target = match op {
                StateOp::RemoveModel { model } => model,
                StateOp::RemoveField { field, .. } => field,
                _ => unreachable!(),
            };
            if !manifest.approved_destructive.contains(target) {
                return problem(format!("removes {target} without recording its approval"));
            }
        }
    }
    Ok(Some(manifest))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("invalid migration name {0:?}: use lowercase letters, digits and underscores")]
    InvalidName(String),
    #[error("duplicate migration {0}")]
    Duplicate(String),
    #[error("migration {migration} depends on {dependency}, which {reason}")]
    Dependency {
        migration: String,
        dependency: String,
        reason: &'static str,
    },
    #[error(
        "applied migration {migration} has changed (recorded digest {recorded}, current {current}); applied history is immutable"
    )]
    DigestMismatch {
        migration: String,
        recorded: String,
        current: String,
    },
    #[error(
        "the database has applied migration {0}, which this build does not know; refusing to run an older or divergent history"
    )]
    UnknownApplied(String),
    #[error(
        "non-atomic migration {migration} stopped at step {step} and is partially applied: {message}. \
         Fix the cause and run migrate again; it resumes at step {step}"
    )]
    Partial {
        migration: String,
        step: i32,
        message: String,
    },
    /// The plan needs approval, or differs from the approved one.
    #[error("{0}")]
    NotApproved(String),
    #[error("could not get the migration lock within {0}; another migrate is running")]
    LockTimeout(String),
    #[error("cannot reach the database: {0}")]
    Unreachable(String),
    #[error("migration {migration} failed and was rolled back: {message}")]
    Failed { migration: String, message: String },
    #[error("data step in {migration}: {message}")]
    Data { migration: String, message: String },
    #[error("inconsistent history: {0}")]
    State(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHOOLS_1: MigrationRef = MigrationRef::new("schools", "0001_initial");
    const B_1: MigrationRef = MigrationRef::new("b", "0001");

    const fn m(
        app: &'static str,
        id: &'static str,
        depends_on: &'static [MigrationRef],
    ) -> Migration {
        Migration {
            app,
            id,
            depends_on,
            sql: "SELECT 1",
            manifest: None,
            data: None,
        }
    }

    #[test]
    fn accepts_dependency_ordered_set() {
        let set = MigrationSet::new(vec![
            m("schools", "0001_initial", &[]),
            m("lessons", "0001_initial", &[SCHOOLS_1]),
        ]);
        assert!(set.is_ok());
    }

    #[test]
    fn rejects_duplicates_bad_names_and_bad_dependencies() {
        assert!(matches!(
            MigrationSet::new(vec![m("a", "0001", &[]), m("a", "0001", &[])]),
            Err(HistoryError::Duplicate(_))
        ));
        assert!(matches!(
            MigrationSet::new(vec![m("A", "0001", &[])]),
            Err(HistoryError::InvalidName(_))
        ));
        assert!(matches!(
            MigrationSet::new(vec![m("a", "0001", &[B_1])]),
            Err(HistoryError::Dependency {
                reason: "is not defined",
                ..
            })
        ));
        assert!(matches!(
            MigrationSet::new(vec![m("a", "0001", &[B_1]), m("b", "0001", &[]),]),
            Err(HistoryError::Dependency {
                reason: "must come earlier",
                ..
            })
        ));
    }

    #[test]
    fn digest_is_stable_and_content_sensitive() {
        let a = m("a", "0001", &[]);
        let mut b = a.clone();
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.digest().len(), 64);
        b.sql = "SELECT 2";
        assert_ne!(a.digest(), b.digest());
    }
}
