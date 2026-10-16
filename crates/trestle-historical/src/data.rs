//! Historical SDK for data migrations (design doc §14.2).
//!
//! A data step never imports current application types. It names models and
//! fields by stable ID and key, and [`HistoricalTx`] resolves them against
//! the schema **as it was at that migration** (replayed history), so the
//! step keeps working after later migrations rename tables or columns, or
//! after the current Rust type is renamed or removed (I08).
//!
//! The API is deliberately small (§14.2): bounded batches by primary key,
//! single-row update/insert/delete, and counts. Values are JSON scalars
//! bound as parameters, never interpolated.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde_json::{Number, Value};
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::query::Query;
use sqlx::{PgConnection, Postgres, Row as _};
use uuid::Uuid;

use crate::HistoryError;
use crate::schema::{ColumnType, FieldState, ModelState, SchemaState};

pub type DataFuture<'a> = Pin<Box<dyn Future<Output = Result<(), HistoryError>> + Send + 'a>>;
pub type DataFn = for<'a, 'c> fn(&'a mut HistoricalTx<'c>) -> DataFuture<'a>;

/// A named, frozen data step. `source` is the text of the module that
/// implements it (via `include_str!`); it is part of the migration digest,
/// so editing an applied data step is detected like editing applied SQL.
#[derive(Clone, Copy)]
pub struct DataStep {
    pub name: &'static str,
    pub source: &'static str,
    pub run: DataFn,
}

impl fmt::Debug for DataStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataStep")
            .field("name", &self.name)
            .finish()
    }
}

pub type Row = BTreeMap<String, Value>;

/// Largest batch a data step may request.
pub const MAX_BATCH: i64 = 10_000;

pub struct HistoricalTx<'c> {
    conn: &'c mut PgConnection,
    state: &'c SchemaState,
    migration: String,
}

impl<'c> HistoricalTx<'c> {
    pub(crate) fn new(
        conn: &'c mut PgConnection,
        state: &'c SchemaState,
        migration: String,
    ) -> Self {
        Self {
            conn,
            state,
            migration,
        }
    }

    /// The schema as of this migration.
    pub fn state(&self) -> &SchemaState {
        self.state
    }

    fn error(&self, message: impl Into<String>) -> HistoryError {
        HistoryError::Data {
            migration: self.migration.clone(),
            message: message.into(),
        }
    }

    fn model(&self, id: &str) -> Result<&'c ModelState, HistoryError> {
        self.state.model(id).ok_or_else(|| {
            self.error(format!(
                "model {id} does not exist at this point in history"
            ))
        })
    }

    fn field<'m>(&self, model: &'m ModelState, key: &str) -> Result<&'m FieldState, HistoryError> {
        model.field_by_key(key).ok_or_else(|| {
            self.error(format!(
                "{} has no field {key} at this point in history",
                model.id
            ))
        })
    }

    fn primary_key<'m>(&self, model: &'m ModelState) -> Result<&'m FieldState, HistoryError> {
        match model.primary_key().as_slice() {
            [only] => Ok(only),
            _ => Err(self.error(format!("{} has no single-column primary key", model.id))),
        }
    }

    /// Up to `limit` rows ordered by primary key, starting after `after`.
    /// Rows are keyed by field key; the primary key is always included.
    pub async fn batch(
        &mut self,
        model: &str,
        keys: &[&str],
        after: Option<&Value>,
        limit: i64,
    ) -> Result<Vec<Row>, HistoryError> {
        if !(1..=MAX_BATCH).contains(&limit) {
            return Err(self.error(format!("batch size must be 1..={MAX_BATCH}")));
        }
        let m = self.model(model)?;
        let pk = self.primary_key(m)?;
        let mut fields = vec![pk];
        for key in keys {
            let f = self.field(m, key)?;
            if f.id != pk.id {
                fields.push(f);
            }
        }
        let columns: Vec<&str> = fields.iter().map(|f| f.column.as_str()).collect();
        let mut sql = format!("SELECT {} FROM {}", columns.join(", "), m.table);
        if after.is_some() {
            sql.push_str(&format!(" WHERE {} > $1", pk.column));
        }
        sql.push_str(&format!(" ORDER BY {} LIMIT {limit}", pk.column));
        let mut query = sqlx::query(&sql);
        if let Some(after) = after {
            query = self.bind(query, pk, after)?;
        }
        let rows = query.fetch_all(&mut *self.conn).await?;
        rows.iter()
            .map(|row| {
                fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| Ok((f.key().to_owned(), self.decode(row, i, f)?)))
                    .collect()
            })
            .collect()
    }

    pub async fn count(&mut self, model: &str) -> Result<i64, HistoryError> {
        let m = self.model(model)?;
        let sql = format!("SELECT count(*) FROM {}", m.table);
        Ok(sqlx::query_scalar(&sql).fetch_one(&mut *self.conn).await?)
    }

    /// Updates one row by primary key. Returns rows affected.
    pub async fn update(
        &mut self,
        model: &str,
        key: &Value,
        set: &[(&str, Value)],
    ) -> Result<u64, HistoryError> {
        let m = self.model(model)?;
        let pk = self.primary_key(m)?;
        if set.is_empty() {
            return Ok(0);
        }
        let mut targets = Vec::new();
        for (key, _) in set {
            targets.push(self.field(m, key)?);
        }
        let assignments: Vec<String> = targets
            .iter()
            .enumerate()
            .map(|(i, f)| format!("{} = ${}", f.column, i + 1))
            .collect();
        let sql = format!(
            "UPDATE {} SET {} WHERE {} = ${}",
            m.table,
            assignments.join(", "),
            pk.column,
            set.len() + 1
        );
        let mut query = sqlx::query(&sql);
        for (f, (_, value)) in targets.iter().zip(set) {
            query = self.bind(query, f, value)?;
        }
        query = self.bind(query, pk, key)?;
        Ok(query.execute(&mut *self.conn).await?.rows_affected())
    }

    pub async fn insert(
        &mut self,
        model: &str,
        values: &[(&str, Value)],
    ) -> Result<(), HistoryError> {
        let m = self.model(model)?;
        let mut fields = Vec::new();
        for (key, _) in values {
            fields.push(self.field(m, key)?);
        }
        let columns: Vec<&str> = fields.iter().map(|f| f.column.as_str()).collect();
        let params: Vec<String> = (1..=values.len()).map(|i| format!("${i}")).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            m.table,
            columns.join(", "),
            params.join(", ")
        );
        let mut query = sqlx::query(&sql);
        for (f, (_, value)) in fields.iter().zip(values) {
            query = self.bind(query, f, value)?;
        }
        query.execute(&mut *self.conn).await?;
        Ok(())
    }

    pub async fn delete(&mut self, model: &str, key: &Value) -> Result<u64, HistoryError> {
        let m = self.model(model)?;
        let pk = self.primary_key(m)?;
        let sql = format!("DELETE FROM {} WHERE {} = $1", m.table, pk.column);
        let query = self.bind(sqlx::query(&sql), pk, key)?;
        Ok(query.execute(&mut *self.conn).await?.rows_affected())
    }

    fn bind<'q>(
        &self,
        query: Query<'q, Postgres, PgArguments>,
        field: &FieldState,
        value: &Value,
    ) -> Result<Query<'q, Postgres, PgArguments>, HistoryError> {
        let mismatch = || {
            self.error(format!(
                "{value} is not a valid value for {} ({})",
                field.id,
                field.ty.sql()
            ))
        };
        let int = || value.as_i64().ok_or_else(mismatch);
        Ok(match (field.ty, value) {
            (ColumnType::Uuid, Value::Null) => query.bind(None::<Uuid>),
            (ColumnType::Uuid, Value::String(s)) => {
                query.bind(Some(Uuid::parse_str(s).map_err(|_| mismatch())?))
            }
            (ColumnType::Text, Value::Null) => query.bind(None::<String>),
            (ColumnType::Text, Value::String(s)) => query.bind(Some(s.clone())),
            (ColumnType::SmallInt, Value::Null) => query.bind(None::<i16>),
            (ColumnType::SmallInt, _) => {
                query.bind(Some(i16::try_from(int()?).map_err(|_| mismatch())?))
            }
            (ColumnType::Integer, Value::Null) => query.bind(None::<i32>),
            (ColumnType::Integer, _) => {
                query.bind(Some(i32::try_from(int()?).map_err(|_| mismatch())?))
            }
            (ColumnType::BigInt, Value::Null) => query.bind(None::<i64>),
            (ColumnType::BigInt, _) => query.bind(Some(int()?)),
            (ColumnType::Boolean, Value::Null) => query.bind(None::<bool>),
            (ColumnType::Boolean, Value::Bool(b)) => query.bind(Some(*b)),
            (ColumnType::TimestampTz, _) => {
                return Err(self.error(format!(
                    "{}: timestamp values are not supported by the historical SDK yet",
                    field.id
                )));
            }
            _ => return Err(mismatch()),
        })
    }

    fn decode(&self, row: &PgRow, index: usize, field: &FieldState) -> Result<Value, HistoryError> {
        let value = match field.ty {
            ColumnType::Uuid => row
                .try_get::<Option<Uuid>, _>(index)?
                .map(|u| Value::String(u.to_string())),
            ColumnType::Text => row.try_get::<Option<String>, _>(index)?.map(Value::String),
            ColumnType::SmallInt => row
                .try_get::<Option<i16>, _>(index)?
                .map(|n| Value::Number(Number::from(n))),
            ColumnType::Integer => row
                .try_get::<Option<i32>, _>(index)?
                .map(|n| Value::Number(Number::from(n))),
            ColumnType::BigInt => row
                .try_get::<Option<i64>, _>(index)?
                .map(|n| Value::Number(Number::from(n))),
            ColumnType::Boolean => row.try_get::<Option<bool>, _>(index)?.map(Value::Bool),
            ColumnType::TimestampTz => {
                return Err(self.error(format!(
                    "{}: timestamp values are not supported by the historical SDK yet",
                    field.id
                )));
            }
        };
        Ok(value.unwrap_or(Value::Null))
    }
}
