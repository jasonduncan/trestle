//! `makemigrations` planning (design doc §14.3): history (H) → desired (D).
//!
//! Changes are matched by stable ID, never by name, so a renamed column is
//! a rename and a deleted-and-added field is never guessed to be one.
//! Anything the planner cannot do safely is reported as blocked:
//!
//! * removing a model or field (destructive) unless explicitly approved;
//! * adding a required field, or making one required, without a database
//!   default or a declared one-off backfill;
//! * type, primary-key, reference or default changes (write these by hand).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::schema::{FieldState, IndexState, ModelState, SchemaState, StateOp, UniqueState};

#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Model or field IDs whose removal is approved.
    pub allow_destructive: BTreeSet<String>,
    /// One-off backfill values by field ID (from `Column::backfill_with`).
    pub backfills: BTreeMap<String, Value>,
    /// Field IDs a committed data migration has filled, so they may become
    /// required. PostgreSQL still refuses `SET NOT NULL` if a null remains,
    /// and the migration then rolls back.
    pub filled: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppChange {
    pub app: String,
    pub ops: Vec<StateOp>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Per app, in app order.
    pub changes: Vec<AppChange>,
    /// Reasons generation must stop. A blocked plan writes nothing.
    pub blocked: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.blocked.is_empty()
    }
}

fn check_name(table: &str, column: &str) -> String {
    format!("{table}_{column}_check")
}

/// Assigns names to new check constraints (history records them).
fn named(mut field: FieldState, table: &str) -> FieldState {
    if field.check.is_some() && field.check_name.is_none() {
        field.check_name = Some(check_name(table, &field.column));
    }
    field
}

/// Desired models ordered so referenced models come first.
fn creation_order<'a>(desired: &'a SchemaState, missing: &[&'a ModelState]) -> Vec<&'a ModelState> {
    let mut ordered: Vec<&ModelState> = Vec::new();
    let mut pending: Vec<&ModelState> = missing.to_vec();
    while !pending.is_empty() {
        let ready = pending.iter().position(|m| {
            m.fields
                .iter()
                .filter_map(|f| f.references.as_deref())
                .all(|target| {
                    target == m.id
                        || !pending.iter().any(|p| p.id == target)
                        || !desired.models.contains_key(target)
                })
        });
        // A reference cycle falls back to declaration order; the SQL step
        // reports the unresolved reference.
        let index = ready.unwrap_or(0);
        ordered.push(pending.remove(index));
    }
    ordered
}

pub fn diff(history: &SchemaState, desired: &SchemaState, options: &PlanOptions) -> Plan {
    let mut ops: Vec<StateOp> = Vec::new();
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut blocked = Vec::new();

    // New models, referenced ones first.
    let missing: Vec<&ModelState> = desired
        .models
        .values()
        .filter(|m| !history.models.contains_key(&m.id))
        .collect();
    for model in creation_order(desired, &missing) {
        let mut model = model.clone();
        let table = model.table.clone();
        model.fields = model.fields.into_iter().map(|f| named(f, &table)).collect();
        ops.push(StateOp::CreateModel { model });
    }

    // Changes to existing models.
    for d in desired.models.values() {
        let Some(h) = history.models.get(&d.id) else {
            continue;
        };
        let id = d.id.clone();
        if h.table != d.table {
            ops.push(StateOp::RenameTable {
                model: id.clone(),
                table: d.table.clone(),
            });
        }
        for df in &d.fields {
            match h.field(&df.id) {
                None => add_field(
                    &id,
                    &d.table,
                    df,
                    options,
                    &mut ops,
                    &mut notes,
                    &mut blocked,
                ),
                Some(hf) => alter_field(
                    &id,
                    &d.table,
                    hf,
                    df,
                    options,
                    &mut ops,
                    &mut notes,
                    &mut blocked,
                ),
            }
        }
        diff_named(
            &id,
            &h.unique,
            &d.unique,
            |u: &UniqueState| (&u.name, &u.fields),
            |u| StateOp::AddUnique {
                model: id.clone(),
                unique: u.clone(),
            },
            |name| StateOp::RemoveUnique {
                model: id.clone(),
                name,
            },
            &mut ops,
        );
        diff_named(
            &id,
            &h.indexes,
            &d.indexes,
            |i: &IndexState| (&i.name, &i.fields),
            |i| StateOp::AddIndex {
                model: id.clone(),
                index: i.clone(),
            },
            |name| StateOp::RemoveIndex {
                model: id.clone(),
                name,
            },
            &mut ops,
        );
        for hf in &h.fields {
            if d.field(&hf.id).is_none() {
                destructive(
                    &hf.id,
                    StateOp::RemoveField {
                        model: id.clone(),
                        field: hf.id.clone(),
                    },
                    options,
                    &mut ops,
                    &mut blocked,
                );
            }
        }
    }

    // Removed models last.
    for h in history.models.values() {
        if !desired.models.contains_key(&h.id) {
            destructive(
                &h.id,
                StateOp::RemoveModel {
                    model: h.id.clone(),
                },
                options,
                &mut ops,
                &mut blocked,
            );
        }
    }

    let mut by_app: BTreeMap<String, Vec<StateOp>> = BTreeMap::new();
    for op in ops {
        let app = op.model().split('.').next().unwrap_or_default().to_owned();
        by_app.entry(app).or_default().push(op);
    }
    Plan {
        changes: by_app
            .into_iter()
            .map(|(app, ops)| AppChange {
                notes: notes.remove(&app).unwrap_or_default(),
                app,
                ops,
            })
            .collect(),
        blocked,
    }
}

fn note(notes: &mut BTreeMap<String, Vec<String>>, model: &str, text: String) {
    let app = model.split('.').next().unwrap_or_default().to_owned();
    notes.entry(app).or_default().push(text);
}

fn destructive(
    id: &str,
    op: StateOp,
    options: &PlanOptions,
    ops: &mut Vec<StateOp>,
    blocked: &mut Vec<String>,
) {
    if options.allow_destructive.contains(id) {
        ops.push(op);
    } else {
        blocked.push(format!(
            "{id} was removed from the models. Dropping it destroys data; approve with --allow-destructive {id} after a retention decision, or restore it"
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn add_field(
    model: &str,
    table: &str,
    field: &FieldState,
    options: &PlanOptions,
    ops: &mut Vec<StateOp>,
    notes: &mut BTreeMap<String, Vec<String>>,
    blocked: &mut Vec<String>,
) {
    if field.primary_key {
        blocked.push(format!(
            "{}: adding a primary-key field needs a hand-written migration",
            field.id
        ));
        return;
    }
    let field = named(field.clone(), table);
    if field.nullable || field.default.is_some() {
        ops.push(StateOp::AddField {
            model: model.to_owned(),
            field,
        });
        return;
    }
    match options.backfills.get(&field.id) {
        Some(value) => {
            let mut nullable = field.clone();
            nullable.nullable = true;
            ops.push(StateOp::AddField { model: model.to_owned(), field: nullable });
            ops.push(StateOp::Backfill {
                model: model.to_owned(),
                field: field.id.clone(),
                value: value.clone(),
            });
            ops.push(StateOp::AlterNullability {
                model: model.to_owned(),
                field: field.id.clone(),
                nullable: false,
            });
            note(notes, model, format!("{} is backfilled with {value} for existing rows", field.id));
        }
        None => blocked.push(format!(
            "{} is required and has no database default, but {table} may already have rows. Make it nullable, give it a database default, declare .backfill_with(value), or add it nullable, backfill with a data migration (--empty), then make it required",
            field.id
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn alter_field(
    model: &str,
    table: &str,
    h: &FieldState,
    d: &FieldState,
    options: &PlanOptions,
    ops: &mut Vec<StateOp>,
    notes: &mut BTreeMap<String, Vec<String>>,
    blocked: &mut Vec<String>,
) {
    let id = &d.id;
    if h.ty != d.ty {
        blocked.push(format!(
            "{id} changes type from {} to {}; write that migration by hand",
            h.ty.sql(),
            d.ty.sql()
        ));
    }
    if h.primary_key != d.primary_key || h.references != d.references {
        blocked.push(format!(
            "{id} changes its primary key or reference; write that migration by hand"
        ));
    }
    if h.default != d.default {
        blocked.push(format!(
            "{id} changes its database default; not supported by the generator yet"
        ));
    }
    if h.column != d.column {
        ops.push(StateOp::RenameColumn {
            model: model.to_owned(),
            field: id.clone(),
            column: d.column.clone(),
        });
    }
    if h.check != d.check {
        let check_name = d.check.as_ref().map(|_| check_name(table, &d.column));
        ops.push(StateOp::AlterCheck {
            model: model.to_owned(),
            field: id.clone(),
            check: d.check.clone(),
            check_name,
        });
        note(
            notes,
            model,
            format!("{id}: replacing its check constraint validates existing rows"),
        );
    }
    match (h.nullable, d.nullable) {
        (false, true) => ops.push(StateOp::AlterNullability {
            model: model.to_owned(),
            field: id.clone(),
            nullable: true,
        }),
        (true, false) if options.filled.contains(id) => {
            ops.push(StateOp::AlterNullability {
                model: model.to_owned(),
                field: id.clone(),
                nullable: false,
            });
            note(notes, model, format!("{id} becomes required; relies on a data migration having filled it"));
        }
        (true, false) => match options.backfills.get(id) {
            Some(value) => {
                ops.push(StateOp::Backfill {
                    model: model.to_owned(),
                    field: id.clone(),
                    value: value.clone(),
                });
                ops.push(StateOp::AlterNullability {
                    model: model.to_owned(),
                    field: id.clone(),
                    nullable: false,
                });
            }
            None if d.default.is_some() => blocked.push(format!(
                "{id} becomes required; existing nulls are not filled by a default. Declare .backfill_with(value) or backfill with a data migration first"
            )),
            None => blocked.push(format!(
                "{id} becomes required but existing rows may be null. Declare .backfill_with(value) or backfill with a data migration first"
            )),
        },
        _ => {}
    }
}

fn diff_named<T: Clone>(
    _model: &str,
    history: &[T],
    desired: &[T],
    key: impl Fn(&T) -> (&String, &Vec<String>),
    add: impl Fn(&T) -> StateOp,
    remove: impl Fn(String) -> StateOp,
    ops: &mut Vec<StateOp>,
) {
    for h in history {
        let (name, fields) = key(h);
        let same = desired.iter().find(|d| key(d).0 == name);
        if same.is_none_or(|d| key(d).1 != fields) {
            ops.push(remove(name.clone()));
        }
    }
    for d in desired {
        let (name, fields) = key(d);
        let same = history.iter().find(|h| key(h).0 == name);
        if same.is_none_or(|h| key(h).1 != fields) {
            ops.push(add(d));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, Model, desired};
    use serde_json::json;

    fn history(models: &[Model]) -> SchemaState {
        let d = desired(models).unwrap();
        let mut h = SchemaState::default();
        for op in diff(&SchemaState::default(), &d, &PlanOptions::default())
            .changes
            .into_iter()
            .flat_map(|c| c.ops)
        {
            h.apply(&op).unwrap();
        }
        h
    }

    fn book() -> Model {
        Model::new("library.book")
            .table("books")
            .field(Column::uuid("id").primary_key())
            .field(Column::text("title").chars(1, 200))
    }

    #[test]
    fn identical_states_plan_nothing() {
        let h = history(&[book()]);
        assert!(diff(&h, &desired(&[book()]).unwrap(), &PlanOptions::default()).is_empty());
    }

    #[test]
    fn storage_renames_are_renames_not_drop_and_add() {
        let h = history(&[book()]);
        let renamed = Model::new("library.book")
            .table("publications")
            .field(Column::uuid("id").primary_key())
            .field(Column::text("title").column("name").chars(1, 200));
        let plan = diff(&h, &desired(&[renamed]).unwrap(), &PlanOptions::default());
        assert!(plan.blocked.is_empty());
        let ops = &plan.changes[0].ops;
        assert!(matches!(&ops[0], StateOp::RenameTable { table, .. } if table == "publications"));
        assert!(matches!(&ops[1], StateOp::RenameColumn { column, .. } if column == "name"));
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn a_new_required_field_needs_a_backfill_strategy() {
        let h = history(&[book()]);
        let with_isbn = book().field(Column::text("isbn"));
        let blocked = diff(
            &h,
            &desired(std::slice::from_ref(&with_isbn)).unwrap(),
            &PlanOptions::default(),
        );
        assert!(
            blocked.blocked[0]
                .contains("library.book.isbn is required and has no database default")
        );

        let backfilled = book().field(Column::text("isbn").backfill_with(json!("unknown")));
        let options = PlanOptions {
            backfills: backfilled.backfills(),
            ..Default::default()
        };
        let plan = diff(&h, &desired(&[backfilled]).unwrap(), &options);
        assert!(plan.blocked.is_empty());
        let kinds: Vec<&str> = plan.changes[0]
            .ops
            .iter()
            .map(|op| match op {
                StateOp::AddField { .. } => "add",
                StateOp::Backfill { .. } => "backfill",
                StateOp::AlterNullability {
                    nullable: false, ..
                } => "require",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["add", "backfill", "require"]);
    }

    #[test]
    fn removals_are_blocked_unless_approved() {
        let h = history(&[book().field(Column::text("isbn").nullable())]);
        let d = desired(&[book()]).unwrap();
        let plan = diff(&h, &d, &PlanOptions::default());
        assert!(plan.changes.is_empty());
        assert!(plan.blocked[0].contains("library.book.isbn was removed"));
        let approved = PlanOptions {
            allow_destructive: ["library.book.isbn".to_owned()].into(),
            ..Default::default()
        };
        let plan = diff(&h, &d, &approved);
        assert!(plan.blocked.is_empty());
        assert!(matches!(
            plan.changes[0].ops[0],
            StateOp::RemoveField { .. }
        ));
    }

    #[test]
    fn type_changes_are_blocked() {
        let h = history(&[book()]);
        let changed = Model::new("library.book")
            .table("books")
            .field(Column::uuid("id").primary_key())
            .field(Column::integer("title"));
        let plan = diff(&h, &desired(&[changed]).unwrap(), &PlanOptions::default());
        assert!(
            plan.blocked
                .iter()
                .any(|b| b.contains("changes type from text to integer"))
        );
    }

    #[test]
    fn referenced_models_are_created_first() {
        let shelf = Model::new("library.shelf")
            .table("shelves")
            .field(Column::uuid("id").primary_key());
        let placed = book().field(Column::uuid("shelf_id").references("library.shelf"));
        let plan = diff(
            &SchemaState::default(),
            &desired(&[placed, shelf]).unwrap(),
            &PlanOptions::default(),
        );
        let created: Vec<&str> = plan.changes[0].ops.iter().map(|op| op.model()).collect();
        assert_eq!(created, ["library.shelf", "library.book"]);
    }
}
