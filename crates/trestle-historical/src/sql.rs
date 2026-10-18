//! PostgreSQL DDL for state deltas. Every identifier is re-validated as a
//! plain SQL identifier (artifacts are reviewed, but never trusted blindly)
//! and every literal is escaped, so rendered SQL cannot be steered by names
//! or backfill values.

use serde_json::Value;

use crate::schema::{
    Check, ColumnType, DbDefault, FieldState, ModelState, SchemaState, StateOp, sql_identifier,
};

pub(crate) fn ident(name: &str) -> Result<&str, String> {
    if sql_identifier(name) {
        Ok(name)
    } else {
        Err(format!("{name:?} is not a plain SQL identifier"))
    }
}

fn quote(text: &str) -> Result<String, String> {
    if text.contains('\0') {
        return Err("literal contains a NUL character".into());
    }
    Ok(format!("'{}'", text.replace('\'', "''")))
}

/// A JSON scalar as a SQL literal compatible with `ty`.
pub fn literal(value: &Value, ty: ColumnType) -> Result<String, String> {
    match (ty, value) {
        (_, Value::Null) => Ok("NULL".into()),
        (ColumnType::Text | ColumnType::Uuid, Value::String(s)) => quote(s),
        (ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt, Value::Number(n))
            if n.as_i64().is_some() =>
        {
            Ok(n.to_string())
        }
        (ColumnType::Boolean, Value::Bool(b)) => Ok(b.to_string()),
        (ty, value) => Err(format!("{value} is not a valid {} literal", ty.sql())),
    }
}

fn check_sql(column: &str, check: &Check) -> Result<String, String> {
    Ok(match check {
        Check::Chars { min, max } => format!("char_length({column}) BETWEEN {min} AND {max}"),
        Check::Range { min, max } => format!("{column} BETWEEN {min} AND {max}"),
        Check::AtLeast { min } => format!("{column} >= {min}"),
        Check::OneOf { values } => {
            let quoted: Result<Vec<String>, String> = values.iter().map(|v| quote(v)).collect();
            format!("{column} IN ({})", quoted?.join(", "))
        }
    })
}

fn default_sql(default: &DbDefault) -> Result<String, String> {
    Ok(match default {
        DbDefault::Now => "now()".into(),
        DbDefault::Int { value } => value.to_string(),
        DbDefault::Text { value } => quote(value)?,
        DbDefault::Bool { value } => value.to_string(),
    })
}

fn referenced<'a>(
    state: &'a SchemaState,
    current: &'a ModelState,
    target: &str,
) -> Result<(&'a str, &'a str), String> {
    let model = if target == current.id {
        current
    } else {
        state
            .model(target)
            .ok_or_else(|| format!("reference to unknown model {target}"))?
    };
    let pk = model.primary_key();
    match pk.as_slice() {
        [only] => Ok((&model.table, &only.column)),
        _ => Err(format!("{target} has no single-column primary key")),
    }
}

fn column_definition(
    state: &SchemaState,
    model: &ModelState,
    field: &FieldState,
) -> Result<String, String> {
    let column = ident(&field.column)?;
    let mut sql = format!("{column} {}", field.ty.sql());
    if !field.nullable {
        sql.push_str(" NOT NULL");
    }
    if let Some(default) = &field.default {
        sql.push_str(&format!(" DEFAULT {}", default_sql(default)?));
    }
    if let Some(check) = &field.check {
        let name = field
            .check_name
            .as_deref()
            .ok_or_else(|| format!("{} has a check without a constraint name", field.id))?;
        sql.push_str(&format!(
            " CONSTRAINT {} CHECK ({})",
            ident(name)?,
            check_sql(column, check)?
        ));
    }
    if let Some(target) = &field.references {
        let (table, pk) = referenced(state, model, target)?;
        sql.push_str(&format!(" REFERENCES {} ({})", ident(table)?, ident(pk)?));
    }
    Ok(sql)
}

fn columns(model: &ModelState, field_ids: &[String]) -> Result<String, String> {
    let names: Result<Vec<&str>, String> = field_ids
        .iter()
        .map(|id| {
            let f = model
                .field(id)
                .ok_or_else(|| format!("{} has no field {id}", model.id))?;
            ident(&f.column)
        })
        .collect();
    Ok(names?.join(", "))
}

fn model<'a>(state: &'a SchemaState, id: &str) -> Result<&'a ModelState, String> {
    state.model(id).ok_or_else(|| format!("no model {id}"))
}

fn field<'a>(m: &'a ModelState, id: &str) -> Result<&'a FieldState, String> {
    m.field(id)
        .ok_or_else(|| format!("{} has no field {id}", m.id))
}

fn render_op(state: &SchemaState, op: &StateOp) -> Result<String, String> {
    Ok(match op {
        StateOp::CreateModel { model: m } => {
            let mut lines: Vec<String> = m
                .fields
                .iter()
                .map(|f| column_definition(state, m, f).map(|d| format!("    {d}")))
                .collect::<Result<_, _>>()?;
            let pk: Vec<String> = m.primary_key().iter().map(|f| f.id.clone()).collect();
            lines.push(format!("    PRIMARY KEY ({})", columns(m, &pk)?));
            for u in &m.unique {
                lines.push(format!(
                    "    CONSTRAINT {} UNIQUE ({})",
                    ident(&u.name)?,
                    columns(m, &u.fields)?
                ));
            }
            let mut sql = format!(
                "CREATE TABLE {} (\n{}\n);",
                ident(&m.table)?,
                lines.join(",\n")
            );
            for i in &m.indexes {
                sql.push_str(&format!(
                    "\nCREATE INDEX {} ON {} ({});",
                    ident(&i.name)?,
                    ident(&m.table)?,
                    columns(m, &i.fields)?
                ));
            }
            sql
        }
        StateOp::RemoveModel { model: id } => {
            format!("DROP TABLE {};", ident(&model(state, id)?.table)?)
        }
        StateOp::RenameTable { model: id, table } => {
            format!(
                "ALTER TABLE {} RENAME TO {};",
                ident(&model(state, id)?.table)?,
                ident(table)?
            )
        }
        StateOp::AddField {
            model: id,
            field: f,
        } => {
            let m = model(state, id)?;
            format!(
                "ALTER TABLE {} ADD COLUMN {};",
                ident(&m.table)?,
                column_definition(state, m, f)?
            )
        }
        StateOp::RemoveField {
            model: id,
            field: fid,
        } => {
            let m = model(state, id)?;
            format!(
                "ALTER TABLE {} DROP COLUMN {};",
                ident(&m.table)?,
                ident(&field(m, fid)?.column)?
            )
        }
        StateOp::RenameColumn {
            model: id,
            field: fid,
            column,
        } => {
            let m = model(state, id)?;
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {};",
                ident(&m.table)?,
                ident(&field(m, fid)?.column)?,
                ident(column)?
            )
        }
        StateOp::AlterNullability {
            model: id,
            field: fid,
            nullable,
        } => {
            let m = model(state, id)?;
            format!(
                "ALTER TABLE {} ALTER COLUMN {} {} NOT NULL;",
                ident(&m.table)?,
                ident(&field(m, fid)?.column)?,
                if *nullable { "DROP" } else { "SET" }
            )
        }
        StateOp::AlterCheck {
            model: id,
            field: fid,
            check,
            check_name,
            not_valid,
        } => {
            let m = model(state, id)?;
            let f = field(m, fid)?;
            let table = ident(&m.table)?;
            let mut parts = Vec::new();
            if let Some(old) = &f.check_name {
                parts.push(format!(
                    "ALTER TABLE {table} DROP CONSTRAINT {};",
                    ident(old)?
                ));
            }
            if let Some(check) = check {
                let name = check_name
                    .as_deref()
                    .ok_or("new check needs a constraint name")?;
                parts.push(format!(
                    "ALTER TABLE {table} ADD CONSTRAINT {} CHECK ({}){};",
                    ident(name)?,
                    check_sql(ident(&f.column)?, check)?,
                    if *not_valid { " NOT VALID" } else { "" }
                ));
            }
            parts.join("\n")
        }
        StateOp::Backfill {
            model: id,
            field: fid,
            value,
        } => {
            let m = model(state, id)?;
            let f = field(m, fid)?;
            let column = ident(&f.column)?;
            format!(
                "UPDATE {} SET {column} = {} WHERE {column} IS NULL;",
                ident(&m.table)?,
                literal(value, f.ty)?
            )
        }
        StateOp::AddUnique { model: id, unique } => {
            let m = model(state, id)?;
            format!(
                "ALTER TABLE {} ADD CONSTRAINT {} UNIQUE ({});",
                ident(&m.table)?,
                ident(&unique.name)?,
                columns(m, &unique.fields)?
            )
        }
        StateOp::RemoveUnique { model: id, name } => {
            format!(
                "ALTER TABLE {} DROP CONSTRAINT {};",
                ident(&model(state, id)?.table)?,
                ident(name)?
            )
        }
        StateOp::AddIndex { model: id, index } => {
            let m = model(state, id)?;
            format!(
                "CREATE INDEX {} ON {} ({});",
                ident(&index.name)?,
                ident(&m.table)?,
                columns(m, &index.fields)?
            )
        }
        StateOp::RemoveIndex { name, .. } => format!("DROP INDEX {};", ident(name)?),
        StateOp::ValidateConstraint { model: id, name } => format!(
            "ALTER TABLE {} VALIDATE CONSTRAINT {};",
            ident(&model(state, id)?.table)?,
            ident(name)?
        ),
    })
}

/// SQL for the second half of an online change (ADR-021): one step per
/// operation, each safe outside a transaction and safe to repeat.
pub fn render_online(before: &SchemaState, ops: &[StateOp]) -> Result<String, String> {
    let mut steps = Vec::new();
    for op in ops {
        steps.push(match op {
            StateOp::AddIndex { model: id, index } => {
                let m = model(before, id)?;
                format!(
                    "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} ({});",
                    ident(&index.name)?,
                    ident(&m.table)?,
                    columns(m, &index.fields)?
                )
            }
            StateOp::ValidateConstraint { .. } => render_op(before, op)?,
            other => {
                return Err(format!(
                    "{} cannot run outside a transaction",
                    serde_json::to_value(other)
                        .ok()
                        .and_then(|v| v["op"].as_str().map(str::to_owned))
                        .unwrap_or_default()
                ));
            }
        });
    }
    Ok(steps.join(&format!("\n{}\n", crate::STEP_MARKER)))
}

/// SQL for `ops` applied in order to `before`.
pub fn render(before: &SchemaState, ops: &[StateOp]) -> Result<String, String> {
    let mut state = before.clone();
    let mut statements = Vec::new();
    for op in ops {
        statements.push(render_op(&state, op)?);
        state.apply(op)?;
    }
    Ok(statements.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, Model};
    use serde_json::json;

    #[test]
    fn literals_are_escaped_and_typed() {
        assert_eq!(
            literal(&json!("O'Brien"), ColumnType::Text).unwrap(),
            "'O''Brien'"
        );
        assert_eq!(literal(&json!(5), ColumnType::Integer).unwrap(), "5");
        assert!(literal(&json!("5"), ColumnType::Integer).is_err());
        assert!(literal(&json!(1.5), ColumnType::BigInt).is_err());
        assert!(literal(&json!("a\0b"), ColumnType::Text).is_err());
    }

    #[test]
    fn renders_create_with_checks_references_and_indexes() {
        let mut shelf = Model::new("library.shelf")
            .table("shelves")
            .field(Column::uuid("id").primary_key())
            .state();
        let mut book = Model::new("library.book")
            .table("books")
            .field(Column::uuid("id").primary_key())
            .field(Column::uuid("shelf_id").references("library.shelf"))
            .field(Column::text("title").chars(1, 200))
            .field(
                Column::text("status")
                    .one_of(&["draft", "it's"])
                    .default_text("draft"),
            )
            .unique("books_title_key", &["title"])
            .index("books_shelf_idx", &["shelf_id"])
            .state();
        book.fields[2].check_name = Some("books_title_check".into());
        book.fields[3].check_name = Some("books_status_check".into());
        shelf.fields[0].check_name = None;
        let sql = render(
            &SchemaState::default(),
            &[
                StateOp::CreateModel { model: shelf },
                StateOp::CreateModel { model: book },
            ],
        )
        .unwrap();
        assert!(sql.contains("shelf_id uuid NOT NULL REFERENCES shelves (id)"));
        assert!(
            sql.contains(
                "CONSTRAINT books_title_check CHECK (char_length(title) BETWEEN 1 AND 200)"
            )
        );
        assert!(sql.contains(
            "DEFAULT 'draft' CONSTRAINT books_status_check CHECK (status IN ('draft', 'it''s'))"
        ));
        assert!(sql.contains("CONSTRAINT books_title_key UNIQUE (title)"));
        assert!(sql.contains("CREATE INDEX books_shelf_idx ON books (shelf_id);"));
    }

    #[test]
    fn refuses_unsafe_identifiers_from_artifacts() {
        let mut state = SchemaState::default();
        let mut m = Model::new("library.book")
            .table("books")
            .field(Column::uuid("id").primary_key())
            .state();
        m.table = "books; DROP TABLE users".into();
        state.models.insert(m.id.clone(), m);
        let err = render(
            &state,
            &[StateOp::RemoveModel {
                model: "library.book".into(),
            }],
        )
        .unwrap_err();
        assert!(err.contains("not a plain SQL identifier"));
    }
}
