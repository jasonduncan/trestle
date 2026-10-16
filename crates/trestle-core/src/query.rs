//! A small typed query surface (design doc §13.2, I05).
//!
//! Queries start from a tenant or row scope: [`select`] requires one, so an
//! unscoped read cannot be written with this API. Columns are typed handles
//! on a [`Table`], values are always bound parameters, every ordering ends
//! with the primary key (deterministic, cursor-safe), and results are
//! bounded. Anything else stays an explicit, reviewed repository query.

use std::marker::PhantomData;

use sqlx::postgres::PgRow;
use sqlx::{Encode, FromRow, PgConnection, Postgres, QueryBuilder, Type};

/// A persisted model's table, declared next to its `Model`. Tests check
/// these names against the registry's desired schema.
pub trait Table: 'static {
    /// Stable model ID, e.g. `lessons.lesson`.
    const MODEL: &'static str;
    const TABLE: &'static str;
    /// Columns selected, in the order the row type decodes them.
    const COLUMNS: &'static [&'static str];
    /// Unique tie-breaker appended to every ordering.
    const PRIMARY_KEY: &'static str;
}

/// A typed column of table `T` holding values of type `V`.
pub struct Col<T, V> {
    name: &'static str,
    _marker: PhantomData<fn() -> (T, V)>,
}

impl<T, V> Clone for Col<T, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, V> Copy for Col<T, V> {}

impl<T, V> Col<T, V> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: PhantomData,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

pub const DEFAULT_LIMIT: i64 = 100;
pub const MAX_LIMIT: i64 = 1000;

/// A read already constrained by its scope.
pub struct Scoped<T: Table> {
    builder: QueryBuilder<'static, Postgres>,
    order: Vec<(&'static str, Direction)>,
    limit: i64,
    _table: PhantomData<T>,
}

/// Starts a read of `T` constrained to `scope = value` (e.g. the caller's
/// school). The scope is applied before any other condition or limit.
pub fn select<T, V>(scope: Col<T, V>, value: V) -> Scoped<T>
where
    T: Table,
    V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
{
    let mut builder = QueryBuilder::new(format!(
        "SELECT {} FROM {} WHERE {} = ",
        T::COLUMNS.join(", "),
        T::TABLE,
        scope.name
    ));
    builder.push_bind(value);
    Scoped {
        builder,
        order: Vec::new(),
        limit: DEFAULT_LIMIT,
        _table: PhantomData,
    }
}

impl<T: Table> Scoped<T> {
    fn compare<V>(mut self, col: Col<T, V>, op: &str, value: V) -> Self
    where
        V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
    {
        self.builder.push(format!(" AND {} {op} ", col.name));
        self.builder.push_bind(value);
        self
    }

    pub fn eq<V>(self, col: Col<T, V>, value: V) -> Self
    where
        V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
    {
        self.compare(col, "=", value)
    }

    pub fn lt<V>(self, col: Col<T, V>, value: V) -> Self
    where
        V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
    {
        self.compare(col, "<", value)
    }

    pub fn gt<V>(self, col: Col<T, V>, value: V) -> Self
    where
        V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
    {
        self.compare(col, ">", value)
    }

    pub fn between<V>(self, col: Col<T, V>, low: V, high: V) -> Self
    where
        V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
    {
        self.compare(col, ">=", low).compare(col, "<=", high)
    }

    /// Keyset paging: rows strictly after (`value`, `key_value`) in the
    /// order (`col`, `key`) sorted by `direction`.
    pub fn after<V, K>(
        mut self,
        col: Col<T, V>,
        value: V,
        key: Col<T, K>,
        key_value: K,
        direction: Direction,
    ) -> Self
    where
        V: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
        K: 'static + Send + Encode<'static, Postgres> + Type<Postgres>,
    {
        let op = if direction == Direction::Asc {
            ">"
        } else {
            "<"
        };
        self.builder
            .push(format!(" AND ({}, {}) {op} (", col.name, key.name));
        self.builder.push_bind(value);
        self.builder.push(", ");
        self.builder.push_bind(key_value);
        self.builder.push(")");
        self
    }

    pub fn is_null<V>(mut self, col: Col<T, Option<V>>) -> Self {
        self.builder.push(format!(" AND {} IS NULL", col.name));
        self
    }

    pub fn is_not_null<V>(mut self, col: Col<T, Option<V>>) -> Self {
        self.builder.push(format!(" AND {} IS NOT NULL", col.name));
        self
    }

    pub fn order_by<V>(mut self, col: Col<T, V>, direction: Direction) -> Self {
        self.order.push((col.name, direction));
        self
    }

    /// Clamped to `1..=MAX_LIMIT`.
    pub fn limit(mut self, limit: i64) -> Self {
        self.limit = limit.clamp(1, MAX_LIMIT);
        self
    }

    fn finish(mut self) -> QueryBuilder<'static, Postgres> {
        // The tie-breaker follows the leading direction so keyset paging
        // (`(col, pk) > (…)` or `< (…)`) matches the ordering exactly.
        if !self.order.iter().any(|(c, _)| *c == T::PRIMARY_KEY) {
            let direction = self.order.first().map_or(Direction::Asc, |(_, d)| *d);
            self.order.push((T::PRIMARY_KEY, direction));
        }
        let order: Vec<String> = self
            .order
            .iter()
            .map(|(c, d)| format!("{c} {}", if *d == Direction::Asc { "ASC" } else { "DESC" }))
            .collect();
        self.builder.push(format!(
            " ORDER BY {} LIMIT {}",
            order.join(", "),
            self.limit
        ));
        self.builder
    }

    /// The SQL this query runs (values appear as `$n` parameters).
    pub fn sql(self) -> String {
        self.finish().sql().to_owned()
    }

    pub async fn fetch_all<R>(self, conn: &mut PgConnection) -> Result<Vec<R>, sqlx::Error>
    where
        R: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        let mut builder = self.finish();
        builder.build_query_as::<R>().fetch_all(conn).await
    }

    pub async fn fetch_optional<R>(self, conn: &mut PgConnection) -> Result<Option<R>, sqlx::Error>
    where
        R: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        let mut builder = self.limit(1).finish();
        builder.build_query_as::<R>().fetch_optional(conn).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct Books;

    impl Table for Books {
        const MODEL: &'static str = "library.book";
        const TABLE: &'static str = "books";
        const COLUMNS: &'static [&'static str] = &["id", "shelf_id", "title", "pages"];
        const PRIMARY_KEY: &'static str = "id";
    }

    const ID: Col<Books, Uuid> = Col::new("id");
    const SHELF: Col<Books, Uuid> = Col::new("shelf_id");
    const TITLE: Col<Books, String> = Col::new("title");
    const PAGES: Col<Books, Option<i32>> = Col::new("pages");

    #[test]
    fn keyset_after_matches_direction() {
        let sql = select(SHELF, Uuid::nil())
            .after(TITLE, "M".to_owned(), ID, Uuid::nil(), Direction::Desc)
            .order_by(TITLE, Direction::Desc)
            .sql();
        assert!(
            sql.contains("AND (title, id) < ($2, $3) ORDER BY title DESC, id DESC"),
            "{sql}"
        );
    }

    #[test]
    fn scope_comes_first_and_ordering_is_deterministic() {
        let sql = select(SHELF, Uuid::nil())
            .eq(TITLE, "Topology".to_owned())
            .is_not_null(PAGES)
            .order_by(TITLE, Direction::Desc)
            .limit(5_000)
            .sql();
        assert_eq!(
            sql,
            "SELECT id, shelf_id, title, pages FROM books WHERE shelf_id = $1 AND title = $2 \
             AND pages IS NOT NULL ORDER BY title DESC, id DESC LIMIT 1000"
        );
    }
}
