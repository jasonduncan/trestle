//! Schema IR (design doc §13–14): the managed persistence state.
//!
//! Three states stay distinct (I07):
//!
//! * **desired** (D): built from the current application's [`Model`]s;
//! * **historical** (H): replayed from committed migrations' [`StateOp`]s;
//! * **observed** (O): read from the database catalog (see `verify`).
//!
//! Identity is stable and separate from storage: a model ID such as
//! `lessons.lesson` and a field ID such as `lessons.lesson.title` never
//! change when a Rust type, table or column is renamed (§13.2).
//! Presentation (labels, widgets) is not part of this state, so changing it
//! can never produce a migration (I10).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnType {
    Uuid,
    Text,
    SmallInt,
    Integer,
    BigInt,
    Boolean,
    #[serde(rename = "timestamptz")]
    TimestampTz,
}

impl ColumnType {
    pub fn sql(self) -> &'static str {
        match self {
            ColumnType::Uuid => "uuid",
            ColumnType::Text => "text",
            ColumnType::SmallInt => "smallint",
            ColumnType::Integer => "integer",
            ColumnType::BigInt => "bigint",
            ColumnType::Boolean => "boolean",
            ColumnType::TimestampTz => "timestamptz",
        }
    }

    /// `information_schema.columns.data_type` for this type.
    pub fn catalog_name(self) -> &'static str {
        match self {
            ColumnType::TimestampTz => "timestamp with time zone",
            other => other.sql(),
        }
    }
}

/// Portable check rules, rendered to SQL `CHECK` constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Check {
    /// `char_length` (code points) within bounds.
    Chars {
        min: u32,
        max: u32,
    },
    Range {
        min: i64,
        max: i64,
    },
    AtLeast {
        min: i64,
    },
    OneOf {
        values: Vec<String>,
    },
}

/// Database defaults. Distinct from application defaults and from one-off
/// migration backfill values (§13.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DbDefault {
    Now,
    Int { value: i64 },
    Text { value: String },
    Bool { value: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldState {
    /// Stable ID: `<model id>.<key>`.
    pub id: String,
    pub column: String,
    #[serde(rename = "type")]
    pub ty: ColumnType,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub primary_key: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
    /// Name of the existing check constraint. Historical data: kept across
    /// column and table renames so later migrations can drop it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_name: Option<String>,
    /// Model ID this field references (foreign key to its primary key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DbDefault>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl FieldState {
    /// The field's key within its model (last ID segment).
    pub fn key(&self) -> &str {
        self.id.rsplit('.').next().unwrap_or(&self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UniqueState {
    pub name: String,
    /// Field IDs.
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexState {
    pub name: String,
    /// Field IDs.
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelState {
    /// Stable ID: `<app>.<name>`.
    pub id: String,
    pub table: String,
    pub fields: Vec<FieldState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<UniqueState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<IndexState>,
}

impl ModelState {
    pub fn app(&self) -> &str {
        self.id.split('.').next().unwrap_or(&self.id)
    }

    pub fn field(&self, id: &str) -> Option<&FieldState> {
        self.fields.iter().find(|f| f.id == id)
    }

    /// Field by key (`title`) rather than full ID.
    pub fn field_by_key(&self, key: &str) -> Option<&FieldState> {
        self.fields.iter().find(|f| f.key() == key)
    }

    pub fn primary_key(&self) -> Vec<&FieldState> {
        self.fields.iter().filter(|f| f.primary_key).collect()
    }

    fn field_mut(&mut self, id: &str) -> Result<&mut FieldState, String> {
        let model = self.id.clone();
        self.fields
            .iter_mut()
            .find(|f| f.id == id)
            .ok_or_else(|| format!("model {model} has no field {id}"))
    }
}

/// A logical state delta recorded in a migration artifact (§14.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StateOp {
    CreateModel {
        model: ModelState,
    },
    RemoveModel {
        model: String,
    },
    RenameTable {
        model: String,
        table: String,
    },
    AddField {
        model: String,
        field: FieldState,
    },
    RemoveField {
        model: String,
        field: String,
    },
    RenameColumn {
        model: String,
        field: String,
        column: String,
    },
    AlterNullability {
        model: String,
        field: String,
        nullable: bool,
    },
    AlterCheck {
        model: String,
        field: String,
        check: Option<Check>,
        check_name: Option<String>,
    },
    /// One-off value for existing rows where the field is null. Changes no
    /// state; frozen into the artifact, never read from current code.
    Backfill {
        model: String,
        field: String,
        value: serde_json::Value,
    },
    AddUnique {
        model: String,
        unique: UniqueState,
    },
    RemoveUnique {
        model: String,
        name: String,
    },
    AddIndex {
        model: String,
        index: IndexState,
    },
    RemoveIndex {
        model: String,
        name: String,
    },
}

impl StateOp {
    pub fn model(&self) -> &str {
        match self {
            StateOp::CreateModel { model } => &model.id,
            StateOp::RemoveModel { model }
            | StateOp::RenameTable { model, .. }
            | StateOp::AddField { model, .. }
            | StateOp::RemoveField { model, .. }
            | StateOp::RenameColumn { model, .. }
            | StateOp::AlterNullability { model, .. }
            | StateOp::AlterCheck { model, .. }
            | StateOp::Backfill { model, .. }
            | StateOp::AddUnique { model, .. }
            | StateOp::RemoveUnique { model, .. }
            | StateOp::AddIndex { model, .. }
            | StateOp::RemoveIndex { model, .. } => model,
        }
    }

    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            StateOp::RemoveModel { .. } | StateOp::RemoveField { .. }
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaState {
    pub models: BTreeMap<String, ModelState>,
}

impl SchemaState {
    pub fn model(&self, id: &str) -> Option<&ModelState> {
        self.models.get(id)
    }

    fn model_mut(&mut self, id: &str) -> Result<&mut ModelState, String> {
        self.models
            .get_mut(id)
            .ok_or_else(|| format!("no model {id}"))
    }

    /// Applies one delta. Inconsistent history is an error, never repaired.
    pub fn apply(&mut self, op: &StateOp) -> Result<(), String> {
        match op {
            StateOp::CreateModel { model } => {
                if self.models.contains_key(&model.id) {
                    return Err(format!("model {} already exists", model.id));
                }
                self.models.insert(model.id.clone(), model.clone());
            }
            StateOp::RemoveModel { model } => {
                self.models
                    .remove(model)
                    .ok_or_else(|| format!("no model {model} to remove"))?;
                let referencing: Vec<String> = self
                    .models
                    .values()
                    .flat_map(|m| &m.fields)
                    .filter(|f| f.references.as_deref() == Some(model))
                    .map(|f| f.id.clone())
                    .collect();
                if !referencing.is_empty() {
                    return Err(format!(
                        "{model} is still referenced by {}",
                        referencing.join(", ")
                    ));
                }
            }
            StateOp::RenameTable { model, table } => self.model_mut(model)?.table = table.clone(),
            StateOp::AddField { model, field } => {
                let m = self.model_mut(model)?;
                if m.field(&field.id).is_some() {
                    return Err(format!("model {model} already has field {}", field.id));
                }
                m.fields.push(field.clone());
            }
            StateOp::RemoveField { model, field } => {
                let m = self.model_mut(model)?;
                let before = m.fields.len();
                m.fields.retain(|f| f.id != *field);
                if m.fields.len() == before {
                    return Err(format!("model {model} has no field {field} to remove"));
                }
                m.unique.retain(|u| !u.fields.contains(field));
                m.indexes.retain(|i| !i.fields.contains(field));
            }
            StateOp::RenameColumn {
                model,
                field,
                column,
            } => {
                self.model_mut(model)?.field_mut(field)?.column = column.clone();
            }
            StateOp::AlterNullability {
                model,
                field,
                nullable,
            } => self.model_mut(model)?.field_mut(field)?.nullable = *nullable,
            StateOp::AlterCheck {
                model,
                field,
                check,
                check_name,
            } => {
                let f = self.model_mut(model)?.field_mut(field)?;
                f.check = check.clone();
                f.check_name = check_name.clone();
            }
            StateOp::Backfill { model, field, .. } => {
                self.model_mut(model)?.field_mut(field)?;
            }
            StateOp::AddUnique { model, unique } => {
                self.model_mut(model)?.unique.push(unique.clone())
            }
            StateOp::RemoveUnique { model, name } => {
                self.model_mut(model)?.unique.retain(|u| u.name != *name)
            }
            StateOp::AddIndex { model, index } => {
                self.model_mut(model)?.indexes.push(index.clone())
            }
            StateOp::RemoveIndex { model, name } => {
                self.model_mut(model)?.indexes.retain(|i| i.name != *name)
            }
        }
        Ok(())
    }
}

/// Desired-state declaration of one model (the "explicit equivalent" of a
/// Model derive, §18 M3). Pure data: no database access.
#[derive(Debug, Clone)]
pub struct Model {
    id: String,
    table: Option<String>,
    columns: Vec<Column>,
    unique: Vec<(String, Vec<String>)>,
    indexes: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone)]
pub struct Column {
    key: String,
    column: Option<String>,
    ty: ColumnType,
    nullable: bool,
    primary_key: bool,
    check: Option<Check>,
    references: Option<String>,
    default: Option<DbDefault>,
    backfill: Option<serde_json::Value>,
}

impl Column {
    fn new(key: &str, ty: ColumnType) -> Self {
        Self {
            key: key.to_owned(),
            column: None,
            ty,
            nullable: false,
            primary_key: false,
            check: None,
            references: None,
            default: None,
            backfill: None,
        }
    }

    pub fn uuid(key: &str) -> Self {
        Self::new(key, ColumnType::Uuid)
    }
    pub fn text(key: &str) -> Self {
        Self::new(key, ColumnType::Text)
    }
    pub fn small_int(key: &str) -> Self {
        Self::new(key, ColumnType::SmallInt)
    }
    pub fn integer(key: &str) -> Self {
        Self::new(key, ColumnType::Integer)
    }
    pub fn big_int(key: &str) -> Self {
        Self::new(key, ColumnType::BigInt)
    }
    pub fn boolean(key: &str) -> Self {
        Self::new(key, ColumnType::Boolean)
    }
    pub fn timestamptz(key: &str) -> Self {
        Self::new(key, ColumnType::TimestampTz)
    }

    /// Storage column name when it differs from the key. Changing it later
    /// is a rename, never drop-and-add.
    pub fn column(mut self, name: &str) -> Self {
        self.column = Some(name.to_owned());
        self
    }
    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }
    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self
    }
    pub fn chars(mut self, min: u32, max: u32) -> Self {
        self.check = Some(Check::Chars { min, max });
        self
    }
    pub fn range(mut self, min: i64, max: i64) -> Self {
        self.check = Some(Check::Range { min, max });
        self
    }
    pub fn at_least(mut self, min: i64) -> Self {
        self.check = Some(Check::AtLeast { min });
        self
    }
    pub fn one_of(mut self, values: &[&str]) -> Self {
        self.check = Some(Check::OneOf {
            values: values.iter().map(|v| (*v).to_owned()).collect(),
        });
        self
    }
    pub fn references(mut self, model: &str) -> Self {
        self.references = Some(model.to_owned());
        self
    }
    pub fn default_now(mut self) -> Self {
        self.default = Some(DbDefault::Now);
        self
    }
    pub fn default_int(mut self, value: i64) -> Self {
        self.default = Some(DbDefault::Int { value });
        self
    }
    pub fn default_bool(mut self, value: bool) -> Self {
        self.default = Some(DbDefault::Bool { value });
        self
    }
    pub fn default_text(mut self, value: &str) -> Self {
        self.default = Some(DbDefault::Text {
            value: value.to_owned(),
        });
        self
    }
    /// One-off value for existing rows when this required field is added
    /// (or made required). Frozen into the generated migration; never a
    /// database or application default.
    pub fn backfill_with(mut self, value: serde_json::Value) -> Self {
        self.backfill = Some(value);
        self
    }
}

impl Model {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            table: None,
            columns: Vec::new(),
            unique: Vec::new(),
            indexes: Vec::new(),
        }
    }

    pub fn table(mut self, table: &str) -> Self {
        self.table = Some(table.to_owned());
        self
    }

    pub fn field(mut self, column: Column) -> Self {
        self.columns.push(column);
        self
    }

    pub fn unique(mut self, name: &str, keys: &[&str]) -> Self {
        self.unique.push((
            name.to_owned(),
            keys.iter().map(|k| (*k).to_owned()).collect(),
        ));
        self
    }

    pub fn index(mut self, name: &str, keys: &[&str]) -> Self {
        self.indexes.push((
            name.to_owned(),
            keys.iter().map(|k| (*k).to_owned()).collect(),
        ));
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Backfill values declared on columns, by field ID.
    pub fn backfills(&self) -> BTreeMap<String, serde_json::Value> {
        self.columns
            .iter()
            .filter_map(|c| Some((format!("{}.{}", self.id, c.key), c.backfill.clone()?)))
            .collect()
    }

    /// Definition problems (identifiers, references to unknown keys).
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let id = &self.id;
        let segments: Vec<&str> = id.split('.').collect();
        if segments.len() != 2 || !segments.iter().all(|s| sql_identifier(s)) {
            problems.push(format!("model id {id:?} must be <app>.<name> in lowercase"));
        }
        match &self.table {
            None => problems.push(format!("model {id} has no table name")),
            Some(table) if !sql_identifier(table) => problems.push(format!(
                "model {id}: table {table:?} is not a plain SQL identifier"
            )),
            Some(_) => {}
        }
        for (i, c) in self.columns.iter().enumerate() {
            if !sql_identifier(&c.key) || c.column.as_deref().is_some_and(|n| !sql_identifier(n)) {
                problems.push(format!(
                    "model {id}: field {:?} needs plain SQL identifiers",
                    c.key
                ));
            }
            if self.columns[..i].iter().any(|o| o.key == c.key) {
                problems.push(format!("model {id}: duplicate field {}", c.key));
            }
            if c.primary_key && c.nullable {
                problems.push(format!(
                    "model {id}: primary key {} cannot be nullable",
                    c.key
                ));
            }
        }
        if !self.columns.iter().any(|c| c.primary_key) {
            problems.push(format!("model {id} has no primary key"));
        }
        for (name, keys) in self.unique.iter().chain(&self.indexes) {
            if !sql_identifier(name) {
                problems.push(format!(
                    "model {id}: constraint/index name {name:?} is not a plain SQL identifier"
                ));
            }
            for key in keys {
                if !self.columns.iter().any(|c| &c.key == key) {
                    problems.push(format!("model {id}: {name} names unknown field {key}"));
                }
            }
        }
        problems
    }

    /// The desired state. Check-constraint names are left empty: they are
    /// history, assigned when a migration first creates the check.
    pub fn state(&self) -> ModelState {
        let field_id = |key: &str| format!("{}.{key}", self.id);
        ModelState {
            id: self.id.clone(),
            table: self.table.clone().unwrap_or_default(),
            fields: self
                .columns
                .iter()
                .map(|c| FieldState {
                    id: field_id(&c.key),
                    column: c.column.clone().unwrap_or_else(|| c.key.clone()),
                    ty: c.ty,
                    nullable: c.nullable,
                    primary_key: c.primary_key,
                    check: c.check.clone(),
                    check_name: None,
                    references: c.references.clone(),
                    default: c.default.clone(),
                })
                .collect(),
            unique: self
                .unique
                .iter()
                .map(|(name, keys)| UniqueState {
                    name: name.clone(),
                    fields: keys.iter().map(|k| field_id(k)).collect(),
                })
                .collect(),
            indexes: self
                .indexes
                .iter()
                .map(|(name, keys)| IndexState {
                    name: name.clone(),
                    fields: keys.iter().map(|k| field_id(k)).collect(),
                })
                .collect(),
        }
    }
}

/// Plain lowercase SQL identifier within PostgreSQL's 63-byte limit, so it
/// never needs quoting.
pub fn sql_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 63
        && matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Builds desired state from models, reporting definition problems and
/// references to unknown models.
pub fn desired(models: &[Model]) -> Result<SchemaState, Vec<String>> {
    let mut problems: Vec<String> = models.iter().flat_map(Model::problems).collect();
    let mut state = SchemaState::default();
    for model in models {
        let s = model.state();
        if state.models.values().any(|m| m.table == s.table) {
            problems.push(format!("two models use table {}", s.table));
        }
        if state.models.insert(s.id.clone(), s).is_some() {
            problems.push(format!("model {} is declared twice", model.id));
        }
    }
    for model in state.models.values() {
        for field in &model.fields {
            if let Some(target) = &field.references {
                match state.models.get(target) {
                    None => {
                        problems.push(format!("{} references unknown model {target}", field.id))
                    }
                    Some(t) if t.primary_key().len() != 1 => problems.push(format!(
                        "{} references {target}, which has no single-column primary key",
                        field.id
                    )),
                    Some(_) => {}
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(state)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Model {
        Model::new("library.book")
            .table("books")
            .field(Column::uuid("id").primary_key())
            .field(Column::text("title").chars(1, 200))
            .unique("books_title_key", &["title"])
    }

    #[test]
    fn stable_ids_are_independent_of_storage_names() {
        let a = book().state();
        let renamed = Model::new("library.book")
            .table("publications")
            .field(Column::uuid("id").primary_key())
            .field(Column::text("title").column("name").chars(1, 200))
            .state();
        assert_eq!(a.fields[1].id, renamed.fields[1].id);
        assert_ne!(a.fields[1].column, renamed.fields[1].column);
    }

    #[test]
    fn replay_applies_deltas_and_rejects_inconsistent_history() {
        let mut state = SchemaState::default();
        state
            .apply(&StateOp::CreateModel {
                model: book().state(),
            })
            .unwrap();
        state
            .apply(&StateOp::RenameColumn {
                model: "library.book".into(),
                field: "library.book.title".into(),
                column: "name".into(),
            })
            .unwrap();
        assert_eq!(state.models["library.book"].fields[1].column, "name");
        assert!(
            state
                .apply(&StateOp::CreateModel {
                    model: book().state()
                })
                .is_err()
        );
        assert!(
            state
                .apply(&StateOp::RemoveField {
                    model: "library.book".into(),
                    field: "library.book.isbn".into()
                })
                .is_err()
        );
    }

    #[test]
    fn state_ops_round_trip_as_json() {
        let op = StateOp::AddField {
            model: "library.book".into(),
            field: book().state().fields[1].clone(),
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["op"], "add_field");
        assert_eq!(json["field"]["type"], "text");
        assert_eq!(serde_json::from_value::<StateOp>(json).unwrap(), op);
    }

    #[test]
    fn desired_state_checks_definitions() {
        let bad = Model::new("Library.Book").field(Column::text("x").nullable().primary_key());
        let problems = desired(&[bad, book(), book()]).unwrap_err();
        assert!(problems.iter().any(|p| p.contains("must be <app>.<name>")));
        assert!(problems.iter().any(|p| p.contains("has no table name")));
        assert!(problems.iter().any(|p| p.contains("cannot be nullable")));
        assert!(problems.iter().any(|p| p.contains("declared twice")));
        let dangling = Model::new("library.loan")
            .table("loans")
            .field(Column::uuid("id").primary_key())
            .field(Column::uuid("book_id").references("library.missing"));
        assert!(desired(&[dangling]).unwrap_err()[0].contains("unknown model library.missing"));
    }
}
