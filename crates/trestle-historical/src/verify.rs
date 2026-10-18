//! `schema verify` (design doc §14.1, §14.3): compares the observed catalog
//! (O) with the state history says should exist (H).
//!
//! Compared: tables, columns (type and nullability), unmanaged columns,
//! primary keys, foreign-key columns, and unique, check and index names.
//! Not compared (reported as unverified, never as a match): check
//! expressions and default expressions.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgConnection;

use crate::schema::{ColumnType, SchemaState};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verification {
    pub mismatches: Vec<String>,
    pub unverified: Vec<String>,
    /// Tables in the database that no model manages (besides the ledger).
    pub unmanaged_tables: Vec<String>,
}

impl Verification {
    pub fn ok(&self) -> bool {
        self.mismatches.is_empty()
    }
}

pub async fn verify(
    conn: &mut PgConnection,
    expected: &SchemaState,
) -> Result<Verification, sqlx::Error> {
    let columns: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text,
                CASE WHEN data_type = 'numeric' AND numeric_precision IS NOT NULL
                     THEN format('numeric(%s,%s)', numeric_precision, numeric_scale)
                     ELSE data_type::text END,
                is_nullable::text
         FROM information_schema.columns WHERE table_schema = current_schema()",
    )
    .fetch_all(&mut *conn)
    .await?;
    let constraints: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT c.conrelid::regclass::text, c.conname::text, c.contype::text
         FROM pg_constraint c
         WHERE c.connamespace = current_schema()::regnamespace",
    )
    .fetch_all(&mut *conn)
    .await?;
    let foreign_keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT kcu.table_name::text, kcu.column_name::text
         FROM information_schema.table_constraints tc
         JOIN information_schema.key_column_usage kcu
           ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema
         WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = current_schema()",
    )
    .fetch_all(&mut *conn)
    .await?;
    let indexes: Vec<(String, String)> = sqlx::query_as(
        "SELECT tablename::text, indexname::text FROM pg_indexes WHERE schemaname = current_schema()",
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut observed: BTreeMap<&str, BTreeMap<&str, (&str, bool)>> = BTreeMap::new();
    for (table, column, ty, nullable) in &columns {
        observed
            .entry(table)
            .or_default()
            .insert(column, (ty, nullable == "YES"));
    }
    let has_constraint = |table: &str, name: &str, kind: &str| {
        constraints
            .iter()
            .any(|(t, n, k)| t == table && n == name && k == kind)
    };

    let mut report = Verification::default();
    let mut managed = BTreeSet::new();
    for model in expected.models.values() {
        let table = model.table.as_str();
        managed.insert(table);
        let Some(actual) = observed.get(table) else {
            report
                .mismatches
                .push(format!("{}: table {table} is missing", model.id));
            continue;
        };
        for field in &model.fields {
            match actual.get(field.column.as_str()) {
                None => report.mismatches.push(format!(
                    "{}: column {table}.{} is missing",
                    field.id, field.column
                )),
                Some((ty, nullable)) => {
                    let expected = match field.ty {
                        ColumnType::Numeric { precision, scale } => {
                            format!("numeric({precision},{scale})")
                        }
                        other => other.catalog_name().to_owned(),
                    };
                    if *ty != expected {
                        report.mismatches.push(format!(
                            "{}: {table}.{} is {ty}, expected {expected}",
                            field.id, field.column
                        ));
                    }
                    if *nullable != field.nullable {
                        report.mismatches.push(format!(
                            "{}: {table}.{} nullability is {}, expected {}",
                            field.id,
                            field.column,
                            if *nullable { "NULL" } else { "NOT NULL" },
                            if field.nullable { "NULL" } else { "NOT NULL" }
                        ));
                    }
                }
            }
            if let Some(name) = &field.check_name
                && !has_constraint(table, name, "c")
            {
                report
                    .mismatches
                    .push(format!("{}: check constraint {name} is missing", field.id));
            }
            if field.references.is_some()
                && !foreign_keys
                    .iter()
                    .any(|(t, c)| t == table && *c == field.column)
            {
                report.mismatches.push(format!(
                    "{}: foreign key on {table}.{} is missing",
                    field.id, field.column
                ));
            }
        }
        for column in actual.keys() {
            if !model.fields.iter().any(|f| f.column == *column) {
                report.mismatches.push(format!(
                    "{}: {table}.{column} is not managed by any field",
                    model.id
                ));
            }
        }
        if !constraints.iter().any(|(t, _, k)| t == table && k == "p") {
            report
                .mismatches
                .push(format!("{}: {table} has no primary key", model.id));
        }
        for unique in &model.unique {
            if !has_constraint(table, &unique.name, "u") {
                report.mismatches.push(format!(
                    "{}: unique constraint {} is missing",
                    model.id, unique.name
                ));
            }
        }
        for index in &model.indexes {
            if !indexes.iter().any(|(t, n)| t == table && *n == index.name) {
                report
                    .mismatches
                    .push(format!("{}: index {} is missing", model.id, index.name));
            }
        }
    }
    report.unmanaged_tables = observed
        .keys()
        .filter(|t| {
            !managed.contains(*t) && **t != "trestle_migrations" && **t != crate::fence::TABLE
        })
        .map(|t| (*t).to_owned())
        .collect();
    report
        .unverified
        .push("check and default expressions are not compared, only constraint names".into());
    Ok(report)
}
