//! The contract with an automated deployer (design §7): `migrate --plan
//! --json`, approval by plan digest, `migrate --auto`, `verify --json`, and
//! stable exit codes.

use std::fmt;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use sqlx::postgres::PgConnectOptions;
use trestle_historical::classify::{self, Reason};
use trestle_historical::{
    HistoryError, LedgerStatus, MigrationRef, MigrationSet, Outstanding, PlanState, RunConfig,
    RunEvent,
};

/// Exit codes (design §5). Stable: deployers depend on them.
pub mod exit {
    pub const OK: u8 = 0;
    /// Check failed, changes pending, or the plan is blocked.
    pub const CHECK: u8 = 1;
    /// Usage or configuration error.
    pub const USAGE: u8 = 2;
    /// The plan needs approval, or differs from the approved digest.
    pub const APPROVAL: u8 = 3;
    /// A migration failed and rolled back cleanly; safe to retry.
    pub const FAILED: u8 = 4;
    /// A migration failed in a partial or unknown state; needs a human.
    pub const PARTIAL: u8 = 5;
    /// `verify` found drift.
    pub const DRIFT: u8 = 6;
    /// Could not get the migration lock in time.
    pub const LOCK: u8 = 7;
    /// Cannot reach the database.
    pub const UNREACHABLE: u8 = 10;
}

/// A command failure: a message for stderr and an exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: u8,
    pub message: String,
}

impl Failure {
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The exit code for a migration-runner error.
    pub fn from_history(e: &HistoryError) -> Self {
        let code = match e {
            HistoryError::Failed { .. } => exit::FAILED,
            HistoryError::Partial { .. } => exit::PARTIAL,
            HistoryError::NotApproved(_) => exit::APPROVAL,
            HistoryError::LockTimeout(_) => exit::LOCK,
            HistoryError::Unreachable(_) => exit::UNREACHABLE,
            // Lost mid-run: whether the last commit landed is unknown.
            HistoryError::Database(_) | HistoryError::Data { .. } => exit::PARTIAL,
            _ => exit::CHECK,
        };
        Self::new(code, e.to_string())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

impl std::ops::Deref for Failure {
    type Target = str;
    fn deref(&self) -> &str {
        &self.message
    }
}

impl PartialEq<&str> for Failure {
    fn eq(&self, other: &&str) -> bool {
        self.message == *other
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::new(exit::CHECK, message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(exit::CHECK, message)
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// The approval digest of exactly what a run would do: each outstanding
/// migration's content digest, resume step and class, in order.
pub fn plan_digest(set: &MigrationSet, outstanding: &Outstanding) -> String {
    let classes = classify::classify_set(set);
    let mut text = String::from("trestle.plan/1\n");
    for (key, step) in outstanding {
        let (digest, class) = match set.iter().position(|m| m.key() == *key) {
            Some(i) => {
                let m = set.iter().nth(i).expect("index from position");
                let class = if classes[i].is_empty() {
                    "auto"
                } else {
                    "approval"
                };
                (m.digest(), class)
            }
            None => (String::new(), "unknown"),
        };
        text.push_str(&format!("{key}\t{digest}\t{step}\t{class}\n"));
    }
    sha256(text.as_bytes())
}

#[derive(Debug, Serialize)]
pub struct PlanStep {
    pub app: String,
    pub id: String,
    pub digest: String,
    pub atomic: bool,
    /// Raises the schema fence (ADR-017). Omitted when `false`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fence: bool,
    pub risk: &'static str,
    pub has_data_step: bool,
    pub statements: usize,
    /// `pending`, `retry` (an earlier attempt rolled back) or `partial`.
    pub status: &'static str,
    /// The 1-based step a partial non-atomic migration resumes at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_step: Option<i32>,
    pub class: &'static str,
    pub requires_approval: bool,
    pub reasons: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct PlanReport {
    pub schema: &'static str,
    pub project: String,
    pub trestle_version: &'static str,
    pub database: Value,
    pub plan_digest: String,
    pub steps: Vec<PlanStep>,
    pub requires_approval: bool,
    /// `auto` when every step is allowlisted and nothing is blocked.
    pub class: &'static str,
    pub blocked: Vec<String>,
}

impl PlanReport {
    pub fn exit_code(&self) -> u8 {
        if !self.blocked.is_empty() {
            exit::CHECK
        } else {
            exit::OK
        }
    }
}

/// `migrate --plan`: what would run, how it is classified, and its digest.
pub async fn plan_report(
    conn: &mut PgConnection,
    set: &MigrationSet,
    project: &str,
) -> Result<PlanReport, Failure> {
    let db = |e: sqlx::Error| Failure::new(exit::UNREACHABLE, format!("database: {e}"));
    let ledger_present = trestle_historical::ledger_exists(conn).await.map_err(db)?;
    let mut blocked = Vec::new();
    let (outstanding, applied) = match trestle_historical::plan(conn, set).await {
        Ok(plan) => {
            let applied = plan
                .entries
                .iter()
                .filter(|e| e.state == PlanState::Applied)
                .count();
            let outstanding: Vec<(MigrationRef, PlanState)> = plan
                .outstanding()
                .map(|e| (e.migration, e.state.clone()))
                .collect();
            (outstanding, applied)
        }
        Err(e @ HistoryError::Database(_)) => return Err(Failure::from_history(&e)),
        Err(e) => {
            blocked.push(e.to_string());
            (Vec::new(), 0)
        }
    };
    let classes = classify::classify_set(set);
    let mut steps = Vec::new();
    let mut keys = Vec::new();
    for (key, state) in &outstanding {
        let index = set
            .iter()
            .position(|m| m.key() == *key)
            .expect("plan entries come from the set");
        let m = set.iter().nth(index).expect("index from position");
        blocked.extend(m.problems());
        let options = m.options();
        let reasons = &classes[index];
        let (status, step) = match state {
            PlanState::Retry {
                previous: LedgerStatus::Partial,
                step,
                ..
            } => ("partial", *step),
            PlanState::Retry { step, .. } if *step > 0 => ("partial", *step),
            PlanState::Retry { step, .. } => ("retry", *step),
            _ => ("pending", 0),
        };
        keys.push((*key, step));
        steps.push(PlanStep {
            app: m.app.to_owned(),
            id: m.id.to_owned(),
            digest: format!("sha256:{}", m.digest()),
            atomic: options.atomic,
            fence: options.fence,
            risk: classify::risk(reasons),
            has_data_step: m.data.is_some(),
            statements: if options.atomic {
                trestle_historical::statement_count(m.sql)
            } else {
                m.steps().len()
            },
            status,
            resume_step: (status == "partial").then_some(step + 1),
            class: if reasons.is_empty() {
                "auto"
            } else {
                "approval"
            },
            requires_approval: !reasons.is_empty(),
            reasons: reasons.iter().map(|r| r.as_str()).collect(),
        });
    }
    let requires_approval = steps.iter().any(|s| s.requires_approval);
    Ok(PlanReport {
        schema: "trestle.plan/1",
        project: project.to_owned(),
        trestle_version: env!("CARGO_PKG_VERSION"),
        database: json!({ "ledger_present": ledger_present, "applied": applied }),
        plan_digest: plan_digest(set, &keys),
        class: if requires_approval || !blocked.is_empty() {
            "approval"
        } else {
            "auto"
        },
        requires_approval,
        steps,
        blocked,
    })
}

/// How `migrate` decides whether it may apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Apply everything (development and test environments).
    All,
    /// Apply only if every step is `auto` (`--auto`, and production
    /// without `--approve`).
    Auto,
    /// Apply exactly the plan with this digest.
    Approved(String),
}

fn guard(set: Arc<MigrationSet>, gate: Gate) -> impl Fn(&Outstanding) -> Result<(), HistoryError> {
    move |outstanding: &Outstanding| match &gate {
        Gate::All => Ok(()),
        Gate::Approved(approved) => {
            let current = plan_digest(&set, outstanding);
            if &current == approved {
                Ok(())
            } else {
                Err(HistoryError::NotApproved(format!(
                    "the plan changed since it was approved (approved {approved}, now {current}); \
                         run `migrate --plan --json` again"
                )))
            }
        }
        Gate::Auto => {
            let classes = classify::classify_set(&set);
            let mut needs = Vec::new();
            for (key, _) in outstanding {
                if let Some(i) = set.iter().position(|m| m.key() == *key)
                    && !classes[i].is_empty()
                {
                    let reasons: Vec<&str> =
                        classes[i].iter().map(|r: &Reason| r.as_str()).collect();
                    needs.push(format!("{key} ({})", reasons.join(", ")));
                }
            }
            if needs.is_empty() {
                Ok(())
            } else {
                Err(HistoryError::NotApproved(format!(
                    "the plan needs approval: {}; review `migrate --plan`, then run \
                         `migrate --approve {}`",
                    needs.join(", "),
                    plan_digest(&set, outstanding)
                )))
            }
        }
    }
}

/// Applies under `gate`. With `json`, prints one JSON line per event and a
/// summary line on stdout.
pub async fn apply(
    options: &PgConnectOptions,
    set: MigrationSet,
    gate: Gate,
    backup_ref: Option<String>,
    lock_wait: Option<String>,
    json: bool,
) -> Result<(), Failure> {
    let set = Arc::new(set);
    let applied = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = applied.clone();
    let config = RunConfig {
        lock_wait,
        backup_ref,
        guard: Some(Box::new(guard(set.clone(), gate))),
        events: Some(Box::new(move |event| {
            let line = match &event {
                RunEvent::Started(m) => json!({"event": "started", "migration": m.to_string()}),
                RunEvent::Applied(m) => {
                    log.lock().expect("not poisoned").push(m.to_string());
                    json!({"event": "applied", "migration": m.to_string()})
                }
                RunEvent::Failed { migration, message } => json!({
                    "event": "failed", "migration": migration.to_string(), "error": message
                }),
            };
            if json {
                println!("{line}");
            } else if let RunEvent::Applied(m) = event {
                println!("Applied {m}");
            }
        })),
    };
    let result = trestle_historical::migrate_with(options, &set, &config).await;
    let applied = applied.lock().expect("not poisoned").clone();
    match result {
        Ok(_) => {
            if json {
                println!(
                    "{}",
                    json!({"schema": "trestle.migrate/1", "ok": true, "applied": applied})
                );
            } else if applied.is_empty() {
                println!("No migrations to apply.");
            }
            Ok(())
        }
        Err(e) => {
            let failure = Failure::from_history(&e);
            if json {
                println!(
                    "{}",
                    json!({
                        "schema": "trestle.migrate/1", "ok": false, "applied": applied,
                        "exit_code": failure.code, "error": failure.message
                    })
                );
            }
            Err(failure)
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct VerifyReport {
    pub schema: &'static str,
    pub ok: bool,
    /// Catalog differences from applied history, and applied migrations
    /// whose content changed.
    pub drift: Vec<String>,
    /// Non-atomic migrations stopped part-way.
    pub partial: Vec<String>,
    /// Applied migrations this build does not know (a newer release ran).
    pub unknown_applied: Vec<String>,
    /// Applied fence migrations among them: this build may not write
    /// (ADR-017).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fenced_by: Vec<String>,
    /// Ledger rows of apps declared foreign (another project's history).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub foreign: Vec<String>,
    /// Committed migrations not applied yet.
    pub pending: Vec<String>,
    /// Things the comparison cannot confirm (never counted as a match).
    pub unverified: Vec<String>,
}

/// `verify`: ledger and catalog against committed history.
pub async fn verify(conn: &mut PgConnection, set: &MigrationSet) -> Result<VerifyReport, Failure> {
    let db = |e: sqlx::Error| Failure::new(exit::UNREACHABLE, format!("database: {e}"));
    let rows = trestle_historical::ledger_rows(conn).await.map_err(db)?;
    let mut report = VerifyReport {
        schema: "trestle.verify/1",
        ..VerifyReport::default()
    };
    let mut applied_keys = Vec::new();
    for row in &rows {
        if set.is_foreign(&row.app) {
            report.foreign.push(format!("{}.{}", row.app, row.id));
            continue;
        }
        let known = set.iter().find(|m| m.app == row.app && m.id == row.id);
        let name = format!("{}.{}", row.app, row.id);
        match (known, row.status) {
            (None, LedgerStatus::Applied) => report.unknown_applied.push(name),
            (Some(m), LedgerStatus::Applied) => {
                if m.digest() != row.digest {
                    report
                        .drift
                        .push(format!("{name}: applied content differs from this build"));
                }
                applied_keys.push(m.key());
            }
            (_, LedgerStatus::Partial) => report
                .partial
                .push(format!("{name}: stopped at step {}", row.step + 1)),
            (_, LedgerStatus::Running) if row.step > 0 => report
                .partial
                .push(format!("{name}: stopped at step {}", row.step + 1)),
            _ => {}
        }
    }
    for m in set.iter() {
        if !applied_keys.contains(&m.key()) {
            report.pending.push(m.key().to_string());
        }
    }
    report.fenced_by =
        trestle_historical::fence::fenced_by(conn, &trestle_historical::fence::BuildFence::of(set))
            .await
            .map_err(db)?;
    if report.unknown_applied.is_empty() {
        match set.history_of(|k| applied_keys.contains(&k)) {
            Ok(history) => {
                let catalog = trestle_historical::verify::verify(conn, &history)
                    .await
                    .map_err(db)?;
                report.drift.extend(catalog.mismatches);
                report.unverified.extend(catalog.unverified);
            }
            Err(e) => report.unverified.push(format!("catalog not compared: {e}")),
        }
    } else {
        report.unverified.push(
            "catalog not compared: the database has migrations this build does not know".into(),
        );
    }
    report.ok =
        report.drift.is_empty() && report.partial.is_empty() && report.unknown_applied.is_empty();
    Ok(report)
}
