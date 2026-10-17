//! Reviewable migration artifacts (design doc §14.2): a manifest
//! (`<app>/<id>.json`) with the logical deltas and a SQL file
//! (`<app>/<id>.sql`) rendered from them. Generated artifacts use the same
//! manifest and ledger boundary as hand-written ones (§13.1).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::planner::{AppChange, Plan};
use crate::schema::{SchemaState, StateOp};
use crate::{HistoryError, Manifest, MigrationSet, sql};

#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub manifest: Manifest,
    pub sql: String,
}

impl Artifact {
    pub fn key(&self) -> String {
        format!("{}.{}", self.manifest.app, self.manifest.id)
    }

    pub fn manifest_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(&self.manifest).expect("manifest serializes");
        text.push('\n');
        text
    }
}

fn slug(name: &str) -> Result<String, HistoryError> {
    let valid = !name.is_empty()
        && name.len() <= 60
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(name.to_owned())
    } else {
        Err(HistoryError::State(format!(
            "migration name {name:?} must be lowercase letters, digits and underscores"
        )))
    }
}

fn single_leaf(set: &MigrationSet, app: &str) -> Result<Option<String>, HistoryError> {
    let leaves = set.leaves(app);
    match leaves.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.to_string())),
        many => Err(HistoryError::State(format!(
            "app {app} has unmerged branches ({}); add a merge migration that depends on all of them first",
            many.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Models (from other apps) that `ops` reference.
fn referenced_apps(ops: &[StateOp], own: &str) -> BTreeSet<String> {
    ops.iter()
        .flat_map(|op| match op {
            StateOp::CreateModel { model } => model
                .fields
                .iter()
                .filter_map(|f| f.references.clone())
                .collect(),
            StateOp::AddField { field, .. } => field.references.clone().into_iter().collect(),
            _ => Vec::new(),
        })
        .filter_map(|model| model.split('.').next().map(str::to_owned))
        .filter(|app| app != own)
        .collect()
}

/// Turns a plan into one artifact per app, in reference order.
pub fn generate(
    set: &MigrationSet,
    plan: &Plan,
    name: &str,
) -> Result<Vec<Artifact>, HistoryError> {
    if !plan.blocked.is_empty() {
        return Err(HistoryError::State(format!(
            "the plan is blocked:\n  - {}",
            plan.blocked.join("\n  - ")
        )));
    }
    let name = slug(name)?;
    let mut state: SchemaState = set.history()?;

    // Apps whose changes reference models created by another changed app go after it.
    let mut pending: Vec<&AppChange> = plan.changes.iter().collect();
    let mut ordered: Vec<&AppChange> = Vec::new();
    while !pending.is_empty() {
        let index = pending
            .iter()
            .position(|c| {
                referenced_apps(&c.ops, &c.app)
                    .iter()
                    .all(|app| !pending.iter().any(|p| &p.app == app))
            })
            .unwrap_or(0);
        ordered.push(pending.remove(index));
    }

    let mut artifacts: Vec<Artifact> = Vec::new();
    for change in ordered {
        let id = format!("{:04}_{name}", set.next_number(&change.app));
        let mut depends_on: Vec<String> = single_leaf(set, &change.app)?.into_iter().collect();
        for app in referenced_apps(&change.ops, &change.app) {
            let new = artifacts
                .iter()
                .find(|a| a.manifest.app == app)
                .map(Artifact::key);
            if let Some(dep) = new.or(single_leaf(set, &app)?) {
                depends_on.push(dep);
            }
        }
        let rendered = sql::render(&state, &change.ops)
            .map_err(|e| HistoryError::State(format!("{}.{id}: {e}", change.app)))?;
        for op in &change.ops {
            state.apply(op).map_err(HistoryError::State)?;
        }
        let approved_destructive = change
            .ops
            .iter()
            .filter_map(|op| match op {
                StateOp::RemoveModel { model } => Some(model.clone()),
                StateOp::RemoveField { field, .. } => Some(field.clone()),
                _ => None,
            })
            .collect();
        artifacts.push(Artifact {
            manifest: Manifest {
                format: 2,
                app: change.app.clone(),
                id: id.clone(),
                depends_on,
                operations: change.ops.clone(),
                data: None,
                approved_destructive,
                notes: change.notes.clone(),
                atomic: true,
                lock_timeout: None,
                statement_timeout: None,
                fence: false,
            },
            sql: format!(
                "-- {}.{id}: generated by makemigrations. Review before committing.\n{rendered}\n",
                change.app
            ),
        });
    }
    Ok(artifacts)
}

/// A data-only migration skeleton that runs the named data step.
pub fn empty(
    set: &MigrationSet,
    app: &str,
    name: &str,
    data: &str,
) -> Result<Artifact, HistoryError> {
    let name = slug(name)?;
    let data = slug(data)?;
    let id = format!("{:04}_{name}", set.next_number(app));
    Ok(Artifact {
        manifest: Manifest {
            format: 2,
            app: app.to_owned(),
            id: id.clone(),
            depends_on: single_leaf(set, app)?.into_iter().collect(),
            operations: Vec::new(),
            data: Some(data),
            approved_destructive: Vec::new(),
            notes: vec!["data only: changes no managed state".into()],
            atomic: true,
            lock_timeout: None,
            statement_timeout: None,
            fence: false,
        },
        sql: format!("-- {app}.{id}: data only; see the data step.\n"),
    })
}

/// Writes artifacts under `dir/<app>/`. Never overwrites an existing file.
pub fn write(dir: &Path, artifacts: &[Artifact]) -> Result<Vec<String>, HistoryError> {
    let mut written = Vec::new();
    for artifact in artifacts {
        let app_dir = dir.join(&artifact.manifest.app);
        let json = app_dir.join(format!("{}.json", artifact.manifest.id));
        let sql = app_dir.join(format!("{}.sql", artifact.manifest.id));
        if json.exists() || sql.exists() {
            return Err(HistoryError::State(format!(
                "{} already exists",
                json.display()
            )));
        }
        fs::create_dir_all(&app_dir).map_err(|e| HistoryError::State(e.to_string()))?;
        fs::write(&json, artifact.manifest_json())
            .map_err(|e| HistoryError::State(e.to_string()))?;
        fs::write(&sql, &artifact.sql).map_err(|e| HistoryError::State(e.to_string()))?;
        written.push(json.display().to_string());
        written.push(sql.display().to_string());
    }
    Ok(written)
}
