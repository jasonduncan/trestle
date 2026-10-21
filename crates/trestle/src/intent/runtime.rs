//! Running an intent: models, contracts and operations built from the file,
//! executed through the same executor as hand-written operations.
//!
//! Every query is tenant-scoped in SQL before anything else (`[tenancy]`),
//! writes recheck membership inside their transaction (`FOR SHARE`), and
//! identifiers come only from the validated intent; values are always bound.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use axum::extract::FromRef;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};
use tokio::sync::Mutex;
use trestle_core::Problem;
use trestle_core::identity::{ActorId, Identity};
use trestle_core::invocation::Invocation;
use trestle_core::operation::{
    BoxFuture, ContextDescriptor, DynConn, DynContext, DynOperation, DynPolicy, Effects, Mode,
    OperationDescriptor,
};
use trestle_core::pagination::{decode_cursor, encode_cursor};
use trestle_core::registry::{AdminView, App as CoreApp, AppBuilder, Http};
use trestle_historical::schema::{Column, Model};
use trestle_protocol::{Audience, ContractDoc, Field, FormPresentation, Widget};
use uuid::Uuid;

use super::spec::{
    AUTHENTICATED, CheckSpec, Compare, Crud, EndpointSpec, FieldSpec, FieldType, Intent, ModelSpec,
    Repeat, ScopeSpec, SetValue,
};

/// Header that selects the tenant to act in. Grants nothing by itself.
pub use trestle_core::invocation::TENANT_HEADER;
/// Most items a list returns.
pub const MAX_LIMIT: i64 = 100;

fn internal(message: &str) -> Problem {
    tracing::error!("{message}");
    Problem::internal()
}

/// A record of an intent model, with field access checked against the
/// intent: asking for a field the model does not have is an error that
/// names the model, never a silent `null`.
#[derive(Debug, Clone)]
pub struct Row {
    model: Arc<ModelSpec>,
    data: Map<String, Value>,
}

impl Row {
    fn new(model: Arc<ModelSpec>, data: Map<String, Value>) -> Self {
        Self { model, data }
    }

    /// The model ID, e.g. `notes.note`.
    pub fn model(&self) -> &str {
        &self.model.id
    }

    pub fn id(&self) -> Option<Uuid> {
        self.data
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
    }

    fn known(&self, field: &str) -> Result<(), Problem> {
        if matches!(field, "id" | "version" | "created_at") || self.model.field(field).is_some() {
            Ok(())
        } else {
            Err(internal(&format!(
                "field {field:?} is not part of model {} in trestle.toml",
                self.model.id
            )))
        }
    }

    /// The field's value as `T` (`row.get::<String>("title")`). A missing
    /// optional field reads as `null`, so use `Option<T>` for those.
    pub fn get<T: DeserializeOwned>(&self, field: &str) -> Result<T, Problem> {
        self.known(field)?;
        let value = self.data.get(field).cloned().unwrap_or(Value::Null);
        serde_json::from_value(value).map_err(|e| {
            internal(&format!(
                "field {field:?} of {} does not read as the requested type: {e}",
                self.model.id
            ))
        })
    }

    /// Sets a declared field (hooks use this to adjust a record). A field
    /// with `writable = "actions"` is refused (ADR-037).
    pub fn set(&mut self, field: &str, value: impl Serialize) -> Result<(), Problem> {
        self.known(field)?;
        if matches!(field, "id" | "version" | "created_at") {
            return Err(internal(&format!("field {field:?} is managed by Trestle")));
        }
        refuse_managed(&self.model, [field])?;
        let value = serde_json::to_value(value).map_err(|e| internal(&e.to_string()))?;
        self.data.insert(field.to_owned(), value);
        Ok(())
    }

    /// The fields as JSON.
    pub fn to_json(&self) -> Value {
        Value::Object(self.data.clone())
    }
}

/// The validated input of a handler endpoint: path parameters and body
/// fields, already checked against the endpoint's contract.
#[derive(Debug, Clone)]
pub struct Input {
    data: Map<String, Value>,
}

impl Input {
    /// The `{id}` path parameter.
    pub fn id(&self) -> Result<Uuid, Problem> {
        self.get("id")
    }

    pub fn get<T: DeserializeOwned>(&self, name: &str) -> Result<T, Problem> {
        let value = self.data.get(name).cloned().unwrap_or(Value::Null);
        serde_json::from_value(value)
            .map_err(|e| internal(&format!("input {name:?} does not read as requested: {e}")))
    }
}

/// What a handler or hook gets: who is acting, in which tenant, and data
/// access that runs in the operation's own transaction.
///
/// A `Ctx` sees every row of its tenant. Row scopes (ADR-040) narrow the
/// generated operations, not this trusted code; a hook can apply richer
/// rules of its own with `Ctx` queries.
#[derive(Clone)]
pub struct Ctx {
    inner: Arc<CtxInner>,
    /// Models whose write hooks are running, outermost first (ADR-039). A
    /// write to one of them from inside its hooks does not run them again.
    hooking: Vec<String>,
}

struct CtxInner {
    conn: Mutex<Option<DynConn>>,
    actor: ActorId,
    tenant: Option<Uuid>,
    intent: Arc<Resolved>,
    /// False for read operations (GET): their handlers may not mutate.
    writes: bool,
}

impl Ctx {
    /// Runs `f` with a handle on a `DynOperation`'s own connection, scoped
    /// to the caller resolved by [`Permission::resolve`] (ADR-031), and
    /// hands the connection back for [`DynOperation::run`] to return. The
    /// handle writes only on a write transaction ([`DynConn::Write`]).
    pub async fn run<T>(
        resolved: &Arc<Resolved>,
        conn: DynConn,
        ctx: &DynContext,
        f: impl AsyncFnOnce(Ctx) -> Result<T, Problem>,
    ) -> (DynConn, Result<T, Problem>) {
        let (actor, tenant) = match ctx_of(ctx) {
            Ok(c) => (c.actor, c.tenant),
            Err(p) => return (conn, Err(p)),
        };
        let writes = matches!(conn, DynConn::Write(_));
        let handle = Ctx {
            inner: Arc::new(CtxInner {
                conn: Mutex::new(Some(conn)),
                actor,
                tenant,
                intent: resolved.clone(),
                writes,
            }),
            hooking: Vec::new(),
        };
        let result = f(handle.clone()).await;
        let conn = handle.inner.conn.lock().await.take();
        match conn {
            Some(conn) => (conn, result),
            None => unreachable!("the connection is only taken here"),
        }
    }

    fn check_writable(&self) -> Result<(), Problem> {
        if self.inner.writes {
            Ok(())
        } else {
            Err(internal(
                "a read operation tried to write; declare the endpoint with a non-GET method",
            ))
        }
    }

    pub fn actor(&self) -> ActorId {
        self.inner.actor
    }

    /// The tenant selected with `Trestle-Tenant`, when `[tenancy]` is set.
    pub fn tenant(&self) -> Option<Uuid> {
        self.inner.tenant
    }

    fn model(&self, id: &str) -> Result<Arc<ModelSpec>, Problem> {
        self.inner
            .intent
            .intent
            .models
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| internal(&format!("model {id:?} is not declared in trestle.toml")))
    }

    /// Runs `f` on the operation's connection with the tenant's scope:
    /// every row of the tenant. Handlers, hooks and Rust operations are
    /// trusted code, so their data access is not narrowed by row scopes
    /// (ADR-040).
    async fn with<T>(
        &self,
        f: impl AsyncFnOnce(&mut PgConnection, &Scope) -> Result<T, Problem>,
    ) -> Result<T, Problem> {
        self.within(None, f).await
    }

    /// [`Ctx::with`], narrowed to the rows a scoped grant admits when
    /// `rows` is set (ADR-040): `fetch` and `list` then see only those.
    async fn within<T>(
        &self,
        rows: Option<&RowFilter>,
        f: impl AsyncFnOnce(&mut PgConnection, &Scope) -> Result<T, Problem>,
    ) -> Result<T, Problem> {
        let mut guard = self.inner.conn.lock().await;
        let conn = guard
            .as_mut()
            .ok_or_else(|| internal("a handler used its context after the operation ended"))?;
        let mut scope = self.inner.intent.scope(self.inner.tenant);
        scope.rows = rows.cloned();
        f(conn.conn(), &scope).await
    }

    /// One record, or 404.
    pub async fn get(&self, model: &str, id: Uuid) -> Result<Row, Problem> {
        let model = &self.model(model)?;
        self.with(async |conn, scope| {
            fetch(conn, model, scope, id, false)
                .await?
                .ok_or_else(Problem::not_found)
        })
        .await
    }

    /// One record, locked `FOR UPDATE` until the operation commits, or 404
    /// (ADR-039). Another write to the row, and another `get_for_update`
    /// of it, waits until then, so hooks and operations can serialize on
    /// a related row. Only in a write.
    pub async fn get_for_update(&self, model: &str, id: Uuid) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        self.with(async |conn, scope| {
            fetch(conn, model, scope, id, true)
                .await?
                .ok_or_else(Problem::not_found)
        })
        .await
    }

    /// Records matching `filter` (field equality), newest first.
    pub async fn list(&self, model: &str, filter: Value) -> Result<Vec<Row>, Problem> {
        let model = &self.model(model)?;
        let filter = filter
            .as_object()
            .cloned()
            .ok_or_else(|| internal("a list filter must be a JSON object"))?;
        self.with(async |conn, scope| {
            list(
                conn,
                model,
                scope,
                &filter,
                None,
                ("created_at", true),
                MAX_LIMIT,
                None,
            )
            .await
        })
        .await
    }

    /// Creates a record. Values are validated against the model's create
    /// contract, as if they came from a request. The model's write hooks
    /// run (ADR-039).
    pub async fn create(&self, model: &str, values: Value) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        if let Some(values) = values.as_object() {
            refuse_managed(model, values.keys().map(String::as_str))?;
        }
        let contract = create_contract(model);
        let values = contract.validate(&values).map_err(Problem::from)?;
        let values = values.as_object().cloned().unwrap_or_default();
        self.write(model, CTX_OPERATION, Change::Create, None, values)
            .await
    }

    /// Changes fields of a record (no version check), or 404. The model's
    /// write hooks run (ADR-039).
    pub async fn update(&self, model: &str, id: Uuid, values: Value) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        let values = update_values(model, id, values)?;
        let change = Change::Update { id, version: None };
        self.write(model, CTX_OPERATION, change, None, values).await
    }

    /// Changes fields of a record only if its version is still
    /// `expected_version` (ADR-039): 404 if it is missing, and the 412
    /// `precondition_failed` of a stale `If-Match` if another write got
    /// there first. The row is locked `FOR UPDATE` before the comparison.
    /// The model's write hooks run.
    pub async fn update_versioned(
        &self,
        model: &str,
        id: Uuid,
        expected_version: i64,
        values: Value,
    ) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        let values = update_values(model, id, values)?;
        let current = self
            .with(async |conn, scope| fetch(conn, model, scope, id, true).await)
            .await?
            .ok_or_else(Problem::not_found)?;
        if current.data.get("version").and_then(Value::as_i64) != Some(expected_version) {
            return Err(Problem::precondition_failed());
        }
        let change = Change::Update {
            id,
            version: Some(expected_version),
        };
        self.write(model, CTX_OPERATION, change, Some(current), values)
            .await
    }

    /// Deletes a record, or 404. The model's write hooks run (ADR-039).
    pub async fn delete(&self, model: &str, id: Uuid) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        let change = Change::Delete { id, version: None };
        self.write(model, CTX_OPERATION, change, None, Map::new())
            .await
    }

    /// The model's write hooks, unless it has none or they are already
    /// running in this chain of hooks (ADR-039).
    fn write_hooks(&self, model: &ModelSpec) -> Result<Option<WriteHooks>, Problem> {
        if model.hooks.is_empty() || self.hooking.contains(&model.id) {
            return Ok(None);
        }
        if self.hooking.len() >= MAX_HOOK_DEPTH {
            return Err(internal(&format!(
                "write hooks nested more than {MAX_HOOK_DEPTH} deep: {} -> {}",
                self.hooking.join(" -> "),
                model.id
            )));
        }
        let find = |point: &str| {
            model
                .hooks
                .get(point)
                .map(|named| {
                    self.inner
                        .intent
                        .handlers
                        .write_hooks
                        .get(&named.name)
                        .cloned()
                        .ok_or_else(|| {
                            internal(&format!("write hook {:?} is not registered", named.name))
                        })
                })
                .transpose()
        };
        Ok(Some(WriteHooks {
            before: find("before_write")?,
            after: find("after_write")?,
        }))
    }

    /// The context a model's write hooks get: the same connection, actor
    /// and tenant, with the model added to the running chain.
    fn nested(&self, model: &ModelSpec) -> Ctx {
        let mut hooking = self.hooking.clone();
        hooking.push(model.id.clone());
        Ctx {
            inner: self.inner.clone(),
            hooking,
        }
    }

    /// Every write to an intent model goes through here, whichever path
    /// asked for it (ADR-039): the model's `before_write` hook (which may
    /// add changes), the row write, then its `after_write` hook, all on the
    /// operation's connection. `old` is the row as locked by the caller;
    /// for an update or delete with hooks it is fetched `FOR UPDATE` here
    /// otherwise.
    async fn write(
        &self,
        model: &Arc<ModelSpec>,
        operation: &str,
        change: Change,
        mut old: Option<Row>,
        mut values: Map<String, Value>,
    ) -> Result<Row, Problem> {
        let hooks = self.write_hooks(model)?;
        let kind = change.kind();
        if let (Some(_), None, Change::Update { id, .. } | Change::Delete { id, .. }) =
            (&hooks, &old, &change)
        {
            let id = *id;
            old = Some(
                self.with(async |conn, scope| fetch(conn, model, scope, id, true).await)
                    .await?
                    .ok_or_else(Problem::not_found)?,
            );
        }
        if let Some(before) = hooks.as_ref().and_then(|h| h.before.clone()) {
            let new = match kind {
                WriteKind::Create => Some(Row::new(model.clone(), values.clone())),
                WriteKind::Update => old.clone().map(|mut row| {
                    row.data.extend(values.clone());
                    row
                }),
                WriteKind::Delete => None,
            };
            let write = Write {
                operation: operation.to_owned(),
                kind,
                old: old.clone(),
                new,
                changes: Map::new(),
                before: true,
            };
            let write = before(self.nested(model), write).await?;
            values.extend(write.changes);
        }
        let missing = |version: Option<i64>| {
            if version.is_some() {
                Problem::precondition_failed()
            } else {
                Problem::not_found()
            }
        };
        let written = match change {
            Change::Create => {
                self.with(async |conn, scope| insert(conn, model, scope, values).await)
                    .await?
            }
            Change::Update { id, version } => self
                .with(async |conn, scope| update(conn, model, scope, id, values, version).await)
                .await?
                .ok_or_else(|| missing(version))?,
            Change::Delete { id, version } => self
                .with(async |conn, scope| delete(conn, model, scope, id, version).await)
                .await?
                .ok_or_else(|| missing(version))?,
        };
        if let Some(after) = hooks.and_then(|h| h.after) {
            let write = Write {
                operation: operation.to_owned(),
                kind,
                old,
                new: (kind != WriteKind::Delete).then(|| written.clone()),
                changes: Map::new(),
                before: false,
            };
            after(self.nested(model), write).await?;
        }
        Ok(written)
    }
}

/// `Write::operation` for writes made through [`Ctx`]: by handlers, hooks
/// and Rust operations (ADR-039).
const CTX_OPERATION: &str = "ctx";

/// A backstop on nested write hooks (ADR-039). A model's hooks never
/// re-enter themselves, so a chain is at most as long as the number of
/// hooked models; this bounds it regardless.
const MAX_HOOK_DEPTH: usize = 8;

/// The row write [`Ctx::write`] makes.
enum Change {
    Create,
    /// `version: Some` is a conditional write whose miss is a 412.
    Update {
        id: Uuid,
        version: Option<i64>,
    },
    Delete {
        id: Uuid,
        version: Option<i64>,
    },
}

impl Change {
    fn kind(&self) -> WriteKind {
        match self {
            Change::Create => WriteKind::Create,
            Change::Update { .. } => WriteKind::Update,
            Change::Delete { .. } => WriteKind::Delete,
        }
    }
}

/// A model's registered write hooks.
struct WriteHooks {
    before: Option<WriteHookFn>,
    after: Option<WriteHookFn>,
}

/// Validated update values for `Ctx::update` and `Ctx::update_versioned`.
fn update_values(
    model: &ModelSpec,
    id: Uuid,
    values: Value,
) -> Result<Map<String, Value>, Problem> {
    let mut instance = values
        .as_object()
        .cloned()
        .ok_or_else(|| internal("update values must be a JSON object"))?;
    refuse_managed(model, instance.keys().map(String::as_str))?;
    instance.insert("id".into(), json!(id));
    let contract = update_contract(model, false);
    let values = contract
        .validate(&Value::Object(instance))
        .map_err(Problem::from)?;
    let mut values = values.as_object().cloned().unwrap_or_default();
    values.remove("id");
    Ok(values)
}

/// What a write changes (ADR-039).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Create,
    Update,
    Delete,
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteKind::Create => "create",
            WriteKind::Update => "update",
            WriteKind::Delete => "delete",
        }
    }
}

/// One write to a model, as its write hooks see it (ADR-039).
#[derive(Debug, Clone)]
pub struct Write {
    operation: String,
    kind: WriteKind,
    old: Option<Row>,
    new: Option<Row>,
    changes: Map<String, Value>,
    before: bool,
}

impl Write {
    /// The operation whose write this is (`crm.deal.update`, `crm.win`),
    /// or `"ctx"` for a write through [`Ctx`] from a handler, a hook or a
    /// Rust operation.
    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn kind(&self) -> WriteKind {
        self.kind
    }

    /// The row before the write, locked `FOR UPDATE`; `None` on create.
    pub fn old(&self) -> Option<&Row> {
        self.old.as_ref()
    }

    /// In `before_write`, the row about to be written (on create, the
    /// values to insert, without `id` or defaults); in `after_write`, the
    /// row as written. `None` on delete.
    pub fn row(&self) -> Option<&Row> {
        self.new.as_ref()
    }

    /// Changes a field of the row about to be written. Only `before_write`
    /// on a create or update may; like [`Row::set`], it refuses fields
    /// only actions write (ADR-037).
    pub fn set(&mut self, field: &str, value: impl Serialize) -> Result<(), Problem> {
        let Some(row) = self.new.as_mut().filter(|_| self.before) else {
            return Err(internal(
                "only a before_write hook on a create or update may change the row",
            ));
        };
        row.set(field, value)?;
        let value = row.data.get(field).cloned().unwrap_or(Value::Null);
        self.changes.insert(field.to_owned(), value);
        Ok(())
    }
}

/// Refuses a write by trusted code (a handler, a hook or a Rust operation)
/// that names a field only actions may write (ADR-037). It is a bug in that
/// code, not in the request: a 500 whose log names the field.
fn refuse_managed<'a>(
    model: &ModelSpec,
    fields: impl IntoIterator<Item = &'a str>,
) -> Result<(), Problem> {
    match fields
        .into_iter()
        .find(|name| model.field(name).is_some_and(FieldSpec::managed))
    {
        None => Ok(()),
        Some(name) => Err(internal(&format!(
            "field {name:?} of {} is written only by actions (writable = \"actions\"); \
             a handler or hook may not write it",
            model.id
        ))),
    }
}

/// The values a CRUD write may store: every field except the ones only
/// actions write (ADR-037).
fn crud_writes(model: &ModelSpec, values: &Map<String, Value>) -> Map<String, Value> {
    model
        .fields
        .iter()
        .filter(|f| !f.managed())
        .filter_map(|f| values.get(&f.id).map(|v| (f.id.clone(), v.clone())))
        .collect()
}

/// A Rust handler for an endpoint (`handler = "notes::pin"`).
pub type HandlerFn =
    Arc<dyn Fn(Ctx, Input) -> BoxFuture<'static, Result<Row, Problem>> + Send + Sync>;
/// A Rust hook on a CRUD endpoint (`hooks = { validate = "notes::check" }`).
pub type HookFn = Arc<dyn Fn(Ctx, Row) -> BoxFuture<'static, Result<Row, Problem>> + Send + Sync>;
/// A model's write hook (`hooks = { before_write = "crm::deal_rules" }` on
/// `[[models]]`, ADR-039): runs on every write to the model.
pub type WriteHookFn =
    Arc<dyn Fn(Ctx, Write) -> BoxFuture<'static, Result<Write, Problem>> + Send + Sync>;

/// Handlers and hooks registered by name in the project binary.
#[derive(Clone, Default)]
pub struct Handlers {
    pub(crate) handlers: BTreeMap<String, HandlerFn>,
    pub(crate) hooks: BTreeMap<String, HookFn>,
    pub(crate) write_hooks: BTreeMap<String, WriteHookFn>,
}

impl Handlers {
    pub fn handler<F, Fut>(&mut self, name: &str, f: F)
    where
        F: Fn(Ctx, Input) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Row, Problem>> + Send + 'static,
    {
        self.handlers
            .insert(name.to_owned(), Arc::new(move |c, i| Box::pin(f(c, i))));
    }

    pub fn hook<F, Fut>(&mut self, name: &str, f: F)
    where
        F: Fn(Ctx, Row) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Row, Problem>> + Send + 'static,
    {
        self.hooks
            .insert(name.to_owned(), Arc::new(move |c, r| Box::pin(f(c, r))));
    }

    /// A model's write hook (ADR-039). It returns the [`Write`] it was
    /// given, with any changes `before_write` made through [`Write::set`].
    pub fn write_hook<F, Fut>(&mut self, name: &str, f: F)
    where
        F: Fn(Ctx, Write) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Write, Problem>> + Send + 'static,
    {
        self.write_hooks
            .insert(name.to_owned(), Arc::new(move |c, w| Box::pin(f(c, w))));
    }

    /// Whether nothing is registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.handlers.is_empty() && self.hooks.is_empty() && self.write_hooks.is_empty()
    }
}

/// An intent with its handlers linked, ready to install.
pub struct Resolved {
    pub intent: Intent,
    pub file: String,
    handlers: Handlers,
}

/// Tenant scope of one request, and the rows a scoped grant narrows it to.
struct Scope {
    tenant: Option<(String, Uuid)>,
    /// Applied by `fetch` and `list` only. The writes that follow a scoped
    /// `fetch` act on the row it found and locked.
    rows: Option<RowFilter>,
}

/// The rows a scoped grant admits (ADR-040): those that satisfy one of
/// `scopes` for `actor`. Applied in SQL after the tenant.
#[derive(Clone)]
struct RowFilter {
    actor: Uuid,
    scopes: Vec<ScopeSpec>,
}

impl RowFilter {
    /// `(<scope> OR ...)` on the row `t`, with the actor bound as `$n`.
    /// `lock` (writes only) locks the `through` row found `FOR SHARE`
    /// until commit, as the membership recheck does, so removing it waits
    /// for the write, and a write waiting on its removal sees it gone.
    fn sql(&self, tenant: Option<&str>, n: usize, lock: bool) -> String {
        let parts: Vec<String> = self
            .scopes
            .iter()
            .map(|scope| match &scope.through {
                None => format!("t.{} = ${n}", scope.field),
                Some(x) => format!(
                    "EXISTS (SELECT 1 FROM {} x WHERE {}x.{} = t.{} AND x.{} = ${n}{})",
                    x.table,
                    tenant
                        .map(|c| format!("x.{c} = t.{c} AND "))
                        .unwrap_or_default(),
                    x.key,
                    scope.field,
                    x.actor,
                    if lock { " FOR SHARE" } else { "" }
                ),
            })
            .collect();
        format!("({})", parts.join(" OR "))
    }
}

impl Resolved {
    fn scope(&self, tenant: Option<Uuid>) -> Scope {
        Scope {
            tenant: match (&self.intent.tenancy, tenant) {
                (Some(t), Some(id)) => Some((t.column(), id)),
                _ => None,
            },
            rows: None,
        }
    }

    /// Links named handlers and hooks. Every name the file uses must be
    /// registered, and every registered name must be used.
    pub fn link(intent: Intent, file: &str, handlers: Handlers) -> Result<Self, Vec<String>> {
        let mut problems = Vec::new();
        let mut used_handlers = Vec::new();
        let mut used_hooks = Vec::new();
        for endpoint in &intent.endpoints {
            match endpoint {
                EndpointSpec::Custom { id, handler, .. } => {
                    used_handlers.push(handler.name.clone());
                    if !handlers.handlers.contains_key(&handler.name) {
                        let fname = handler.name.rsplit("::").next().unwrap_or("handler");
                        problems.push(format!(
                            "{file}:{} endpoint {id} names handler {:?}, which is not registered; \
                             add .handler({:?}, {fname}) in src/main.rs",
                            handler.line, handler.name, handler.name
                        ));
                    }
                }
                EndpointSpec::Action { .. } => {}
                EndpointSpec::Crud { model, hooks, .. } => {
                    for (point, hook) in hooks {
                        used_hooks.push(hook.name.clone());
                        if !handlers.hooks.contains_key(&hook.name) {
                            let fname = hook.name.rsplit("::").next().unwrap_or("hook");
                            problems.push(format!(
                                "{file}:{} {point} hook of {model} names {:?}, which is not registered; \
                                 add .hook({:?}, {fname}) in src/main.rs",
                                hook.line, hook.name, hook.name
                            ));
                        }
                    }
                }
            }
        }
        let mut used_write_hooks = Vec::new();
        for model in &intent.models {
            for (point, hook) in &model.hooks {
                used_write_hooks.push(hook.name.clone());
                if !handlers.write_hooks.contains_key(&hook.name) {
                    let fname = hook.name.rsplit("::").next().unwrap_or("hook");
                    problems.push(format!(
                        "{file}:{} {point} hook of model {} names {:?}, which is not registered; \
                         add .write_hook({:?}, {fname}) in src/main.rs",
                        hook.line, model.id, hook.name, hook.name
                    ));
                }
            }
        }
        for name in handlers.write_hooks.keys() {
            if !used_write_hooks.contains(name) {
                problems.push(format!(
                    "write hook {name:?} is registered but no model in {file} names it; \
                     add it under hooks = {{ before_write = ... }} on a model or remove the registration"
                ));
            }
        }
        for name in handlers.handlers.keys() {
            if !used_handlers.contains(name) {
                problems.push(format!(
                    "handler {name:?} is registered but no endpoint in {file} names it; \
                     add handler = {name:?} to an endpoint or remove the registration"
                ));
            }
        }
        for name in handlers.hooks.keys() {
            if !used_hooks.contains(name) {
                problems.push(format!(
                    "hook {name:?} is registered but no endpoint in {file} names it; \
                     add it under hooks = {{ ... }} or remove the registration"
                ));
            }
        }
        if problems.is_empty() {
            Ok(Self {
                intent,
                file: file.to_owned(),
                handlers,
            })
        } else {
            Err(problems)
        }
    }

    /// The apps this intent installs: one per app prefix, plus the tenancy
    /// app.
    pub fn apps<S>(self: &Arc<Self>) -> Vec<IntentApp<S>> {
        let mut apps: Vec<String> = self.intent.models.iter().map(|m| m.app.clone()).collect();
        for e in &self.intent.endpoints {
            if let EndpointSpec::Custom { id, .. } | EndpointSpec::Action { id, .. } = e
                && let Some((app, _)) = id.split_once('.')
            {
                apps.push(app.to_owned());
            }
        }
        apps.sort();
        apps.dedup();
        let mut out: Vec<IntentApp<S>> = apps
            .into_iter()
            .map(|app| IntentApp {
                id: app,
                tenancy: false,
                resolved: self.clone(),
                _state: PhantomData,
            })
            .collect();
        if let Some(t) = &self.intent.tenancy {
            out.push(IntentApp {
                id: t.name.clone(),
                tenancy: true,
                resolved: self.clone(),
                _state: PhantomData,
            });
        }
        out
    }
}

/// The models and endpoints of one app in the intent.
pub struct IntentApp<S> {
    id: String,
    tenancy: bool,
    resolved: Arc<Resolved>,
    _state: PhantomData<fn() -> S>,
}

impl<S> CoreApp<S> for IntentApp<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        let intent = &self.resolved.intent;
        if self.tenancy {
            let t = intent.tenancy.as_ref().expect("tenancy app");
            let roles: Vec<&str> = intent.roles.keys().map(String::as_str).collect();
            app.model(
                Model::new(&t.model_id())
                    .table(&t.table())
                    .field(Column::uuid("id").primary_key())
                    .field(Column::text("name").chars(1, 200))
                    .field(Column::timestamptz("created_at").default_now()),
            );
            let mut role = Column::text("role");
            if !roles.is_empty() {
                role = role.one_of(&roles);
            }
            app.model(
                Model::new(&t.membership_model_id())
                    .table(&t.membership_table())
                    .field(
                        Column::uuid(&t.column())
                            .primary_key()
                            .references(&t.model_id()),
                    )
                    .field(Column::uuid("actor_id").primary_key())
                    .field(role)
                    .field(Column::timestamptz("created_at").default_now())
                    .index(
                        &format!("{}_actor_idx", t.membership_table()),
                        &["actor_id"],
                    ),
            );
            return;
        }
        if let Some(t) = &intent.tenancy {
            app.requires(t.name.as_str());
        }
        // A ref to another app's model, and the relation picker for it,
        // need that app first (ADR-032).
        let mut required = BTreeSet::new();
        for model in intent.models.iter().filter(|m| m.app == self.id) {
            for to in model.fields.iter().filter_map(|f| f.to.as_deref()) {
                if let Some(target) = intent.models.iter().find(|m| m.id == to)
                    && target.app != self.id
                    && required.insert(target.app.as_str())
                {
                    app.requires(target.app.as_str());
                }
            }
        }
        for model in intent.models.iter().filter(|m| m.app == self.id) {
            app.model(model_of(model, intent));
        }
        for view in intent
            .views
            .iter()
            .filter(|v| v.id.split('.').next() == Some(self.id.as_str()))
        {
            let mut definition =
                trestle_core::registry::ViewDefinition::collection(&view.id, &view.query);
            definition.kind = view.kind.clone();
            definition.audience = Audience::parse(&view.audience).expect("validated audience");
            definition.resource = view.resource.clone();
            definition.title = view.title.clone();
            definition.identity = view.identity.clone();
            definition.label = view.label.clone();
            definition.columns = view.columns.clone();
            definition.filters = view.filters.clone();
            definition.record_view = view.record_view.clone();
            definition.record_input = view.record_input.clone();
            definition.actions = view.actions.clone();
            definition.action_input = view.action_input.clone();
            definition.sections = view
                .sections
                .iter()
                .map(|s| trestle_core::registry::SectionDefinition {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    query: s.query.clone(),
                    collection: s.collection,
                    identity: s.identity.clone(),
                    label: s.label.clone(),
                    columns: s.columns.clone(),
                    input: s.input.clone(),
                    component: s.component.clone(),
                    required_component: s.required_component,
                })
                .collect();
            app.view_dyn(
                definition,
                Arc::new(Permission::declared(
                    &self.resolved,
                    &view.permission,
                    Reach::Show,
                )),
            );
        }
        for endpoint in &intent.endpoints {
            match endpoint {
                EndpointSpec::Crud {
                    model,
                    ops,
                    read,
                    write,
                    forms,
                    ..
                } => {
                    let spec = intent
                        .models
                        .iter()
                        .find(|m| m.id == *model)
                        .expect("validated");
                    if spec.app != self.id {
                        continue;
                    }
                    let admin = intent.admin.iter().find(|a| a.model == *model);
                    // Admin buttons call these same operations (ADR-029).
                    let audiences: &[Audience] = if admin.is_some() {
                        &[Audience::Product, Audience::Admin]
                    } else {
                        &[Audience::Product]
                    };
                    // CRUD forms show what CRUD writes (ADR-037).
                    let crud_fields: Vec<&str> = spec
                        .fields
                        .iter()
                        .filter(|f| !f.managed())
                        .map(|f| f.id.as_str())
                        .collect();
                    if *forms {
                        let fields = &crud_fields;
                        let name = noun(model);
                        if ops.contains(&Crud::Create) {
                            let form = FormPresentation::for_operation(
                                &format!("{model}.create.form"),
                                &format!("{model}.create"),
                            )
                            .title(&format!("New {name}"))
                            .submit("Create");
                            app.form(form_fields(form, spec, intent, fields));
                        }
                        if ops.contains(&Crud::Update) {
                            let form = FormPresentation::for_operation(
                                &format!("{model}.update.form"),
                                &format!("{model}.update"),
                            )
                            .title(&format!("Edit {name}"))
                            .submit("Save");
                            app.form(form_fields(form, spec, intent, fields));
                        }
                    }
                    let mut view = admin.map(|a| {
                        AdminView::new(&a.title, spec.label.as_deref().unwrap_or("id"))
                            .columns(&a.columns.iter().map(String::as_str).collect::<Vec<_>>())
                            .filters(&a.filters.iter().map(String::as_str).collect::<Vec<_>>())
                    });
                    if let Some(v) = view.take() {
                        let fields = &crud_fields;
                        let name = noun(model);
                        let mut v = v;
                        for (op, form, title, submit) in [
                            (
                                Crud::Create,
                                "admin_create",
                                format!("New {name}"),
                                "Create",
                            ),
                            (Crud::Update, "admin_edit", format!("Edit {name}"), "Save"),
                        ] {
                            if !ops.contains(&op) {
                                continue;
                            }
                            let id = format!("{model}.{form}.form");
                            let presentation = FormPresentation::for_operation(
                                &id,
                                &format!("{model}.{}", op.as_str()),
                            )
                            .audience(Audience::Admin)
                            .title(&title)
                            .submit(submit);
                            app.form(form_fields(presentation, spec, intent, fields));
                            v = if op == Crud::Create {
                                v.create_form(&id)
                            } else {
                                v.edit_form(&id)
                            };
                        }
                        view = Some(v);
                    }
                    if let Some(admin) = admin {
                        for action in &admin.actions {
                            if let Some(EndpointSpec::Action {
                                accept,
                                forms: true,
                                ..
                            }) = intent.endpoints.iter().find(
                                |e| matches!(e, EndpointSpec::Action { id, .. } if id == action),
                            ) && !accept.is_empty()
                            {
                                let presentation = FormPresentation::for_operation(
                                    &format!("{action}.admin.form"),
                                    action,
                                )
                                .audience(Audience::Admin)
                                .title(&words(action.rsplit('.').next().unwrap_or(action)))
                                .submit("Confirm");
                                let fields: Vec<&str> = accept.iter().map(String::as_str).collect();
                                app.form(form_fields(presentation, spec, intent, &fields));
                            }
                        }
                    }
                    // Every CRUD endpoint is a resource: the conventional
                    // bindings, and the admin profile when one is declared.
                    let mut resource = app.resource(model.as_str(), spec.path());
                    if endpoint.idempotent() {
                        // Create, update and delete; actions carry their own.
                        resource = resource.idempotency_key();
                    }
                    for op in ops {
                        let permission = if op.writes() { write } else { read };
                        let built = crud_operation::<S>(
                            &self.resolved,
                            spec,
                            endpoint,
                            *op,
                            permission,
                            audiences,
                        );
                        resource = match op {
                            Crud::List => resource.list(built),
                            Crud::Get => resource.retrieve(built),
                            Crud::Create => resource.create(built),
                            Crud::Update => resource.update(built),
                            Crud::Delete => resource.delete(built),
                        };
                    }
                    let (Some(admin), Some(mut view)) = (admin, view) else {
                        continue;
                    };
                    for action in &admin.actions {
                        let endpoint = intent
                            .endpoints
                            .iter()
                            .find(|e| matches!(e, EndpointSpec::Action { id, .. } if id == action))
                            .expect("validated");
                        let EndpointSpec::Action { when, .. } = endpoint else {
                            unreachable!("an action")
                        };
                        let name = action.rsplit('.').next().unwrap_or(action);
                        let label = words(name);
                        // One starting state is something the shell can show
                        // the button for; the server still decides.
                        view = match when.as_slice() {
                            [(field, values)] if values.len() == 1 => {
                                view.action_when(name, &label, field, values[0].clone())
                            }
                            _ => view.action(name, &label),
                        };
                        if let EndpointSpec::Action {
                            accept,
                            forms: true,
                            ..
                        } = endpoint
                            && !accept.is_empty()
                        {
                            view.actions.last_mut().unwrap().form =
                                Some(format!("{action}.admin.form"));
                        }
                        let (built, http) =
                            action_operation::<S>(&self.resolved, endpoint, audiences);
                        resource = resource.action_at(name, built, http);
                    }
                    resource.admin_dyn(
                        view,
                        Arc::new(Permission::declared(
                            &self.resolved,
                            &admin.permission,
                            Reach::Show,
                        )),
                    );
                }
                EndpointSpec::Custom {
                    id,
                    model,
                    method,
                    path,
                    permission,
                    handler,
                    input,
                    idempotent,
                } => {
                    if id.split_once('.').map(|(a, _)| a) != Some(self.id.as_str()) {
                        continue;
                    }
                    let spec = intent
                        .models
                        .iter()
                        .find(|m| m.id == *model)
                        .expect("validated");
                    let mut contract = ContractDoc::input(id, "1");
                    for param in path_params(path) {
                        contract = contract.field(Field::uuid(&param));
                    }
                    for f in input {
                        contract = contract.field(field_contract(f, f.required()));
                    }
                    let mode = if *method == http::Method::GET {
                        Mode::Read
                    } else {
                        Mode::Write
                    };
                    let op = IntentOp::<S> {
                        info: OperationDescriptor::new(id.as_str(), "1", mode)
                            .audiences(&[Audience::Product])
                            .summary(format!("{id} (handler {})", handler.name)),
                        // A handler's `Ctx` sees the whole tenant, so only
                        // an unscoped grant admits it (ADR-040).
                        permission: Permission::declared(&self.resolved, permission, Reach::Tenant),
                        input: contract,
                        output: representation(spec),
                        model: spec.clone(),
                        resolved: self.resolved.clone(),
                        kind: Kind::Handler(handler.name.clone()),
                        _state: PhantomData,
                    };
                    let mut http = Http::new(method.clone(), path.clone());
                    if *idempotent {
                        http = http.idempotency_key();
                    }
                    app.dyn_operation(Arc::new(op), Some(http));
                }
                EndpointSpec::Action {
                    id,
                    model,
                    accept,
                    forms,
                    ..
                } => {
                    if id.split_once('.').map(|(a, _)| a) != Some(self.id.as_str()) {
                        continue;
                    }
                    let spec = intent
                        .models
                        .iter()
                        .find(|m| m.id == *model)
                        .expect("validated");
                    // An action in an admin view is registered with its
                    // model's resource, above.
                    if !intent.admin.iter().any(|a| a.actions.contains(id)) {
                        let (op, http) =
                            action_operation::<S>(&self.resolved, endpoint, &[Audience::Product]);
                        app.dyn_operation(op, Some(http));
                    }
                    if *forms {
                        let verb = words(id.rsplit('.').next().unwrap_or(id));
                        let form = FormPresentation::for_operation(&format!("{id}.form"), id)
                            .title(&format!("{verb} {}", noun(model)))
                            .submit(&verb);
                        let fields: Vec<&str> = accept.iter().map(String::as_str).collect();
                        app.form(form_fields(form, spec, intent, &fields));
                    }
                }
            }
        }
    }
}

/// `closed_at` → `Closed at`.
fn words(id: &str) -> String {
    let text = id.replace('_', " ");
    let mut chars = text.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// `crm.deal` → `deal`.
fn noun(model: &str) -> String {
    model.rsplit('.').next().unwrap_or(model).replace('_', " ")
}

/// A generated form's fields, in the order given, each with the widget its
/// type implies (ADR-026). Optional plain `uuid` fields have no widget and
/// are left out; required ones are refused when the file is read.
fn form_fields(
    mut form: FormPresentation,
    model: &ModelSpec,
    intent: &Intent,
    fields: &[&str],
) -> FormPresentation {
    for name in fields {
        let Some(f) = model.field(name) else { continue };
        let pointer = format!("/{}", f.id);
        let label = f.label.clone().unwrap_or_else(|| words(&f.id));
        let widget = match f.ty {
            FieldType::Text if f.max.is_none_or(|m| m > 200) => Widget::Textarea,
            FieldType::Text => Widget::Text,
            FieldType::Int => Widget::Integer,
            FieldType::Bool => Widget::Checkbox,
            FieldType::Enum => Widget::Select,
            FieldType::Decimal => Widget::Decimal,
            FieldType::Money => Widget::Money,
            FieldType::Date => Widget::Date,
            FieldType::Instant => Widget::DateTime,
            FieldType::Duration => Widget::Duration,
            FieldType::Uuid => continue,
            FieldType::Ref => {
                let to = f.to.as_deref().unwrap_or_default();
                let shown = intent
                    .models
                    .iter()
                    .find(|m| m.id == to)
                    .and_then(|m| m.label.clone())
                    .unwrap_or_default();
                let lookup = intent.endpoints.iter().any(|e| matches!(e, EndpointSpec::Crud { model, search: Some(_), ops, .. } if model == to && ops.contains(&Crud::Get)));
                form = if lookup {
                    form.relation_lookup(
                        &pointer,
                        &label,
                        &format!("{to}.list"),
                        &format!("{to}.get"),
                        &shown,
                        "q",
                    )
                } else {
                    form.relation(&pointer, &label, &format!("{to}.list"), &shown)
                };
                continue;
            }
        };
        form = form.field(&pointer, widget, &label);
    }
    form
}

fn path_params(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|s| s.strip_prefix('{')?.strip_suffix('}'))
        .map(str::to_owned)
        .collect()
}

fn model_of(spec: &ModelSpec, intent: &Intent) -> Model {
    let mut model = Model::new(&spec.id)
        .table(&spec.table)
        .field(Column::uuid("id").primary_key());
    if let Some(t) = &intent.tenancy {
        model = model.field(Column::uuid(&t.column()).references(&t.model_id()));
        // A unique constraint leads with the tenant column and serves
        // tenant-scoped lookups; otherwise add an index for them.
        if spec.unique.is_empty() {
            model = model.index(
                &format!("{}_{}_idx", spec.table, t.column()),
                &[&t.column()],
            );
        }
    }
    for f in &spec.fields {
        if f.ty == FieldType::Money {
            // Two columns: the amount and its currency (ADR-027).
            let (amount, currency) = money_columns(f);
            let mut amount = Column::decimal(&amount, f.precision, f.scale);
            let codes: Vec<&str> = f.currencies.iter().map(String::as_str).collect();
            let mut currency = if codes.is_empty() {
                Column::text(&currency).chars(3, 3)
            } else {
                Column::text(&currency).one_of(&codes)
            };
            if f.optional {
                amount = amount.nullable();
                currency = currency.nullable();
            }
            model = model.field(amount).field(currency);
            continue;
        }
        let mut column = match f.ty {
            FieldType::Text => Column::text(&f.id),
            FieldType::Int => Column::big_int(&f.id),
            FieldType::Bool => Column::boolean(&f.id),
            FieldType::Uuid => Column::uuid(&f.id),
            FieldType::Enum => {
                let values: Vec<&str> = f.values.iter().map(String::as_str).collect();
                Column::text(&f.id).one_of(&values)
            }
            FieldType::Decimal => Column::decimal(&f.id, f.precision, f.scale),
            FieldType::Money => continue,
            FieldType::Date => Column::date(&f.id),
            FieldType::Instant => Column::timestamptz(&f.id),
            FieldType::Duration => Column::duration(&f.id),
            FieldType::Ref => {
                // Writes check the row is in the caller's tenant; the
                // foreign key keeps it from disappearing (ADR-024).
                model = model.index(&format!("{}_{}_idx", spec.table, f.id), &[&f.id]);
                Column::uuid(&f.id).references(f.to.as_deref().unwrap_or_default())
            }
        };
        match f.ty {
            FieldType::Text if f.min.is_some() || f.max.is_some() => {
                column = column.chars(
                    f.min.unwrap_or(0).try_into().unwrap_or(0),
                    f.max
                        .unwrap_or(i64::from(u32::MAX))
                        .try_into()
                        .unwrap_or(u32::MAX),
                );
            }
            FieldType::Int if f.min.is_some() || f.max.is_some() => {
                column = column.range(f.min.unwrap_or(i64::MIN), f.max.unwrap_or(i64::MAX));
            }
            _ => {}
        }
        match &f.default {
            Some(Value::Bool(b)) => column = column.default_bool(*b),
            Some(Value::Number(n)) => column = column.default_int(n.as_i64().unwrap_or(0)),
            Some(Value::String(s)) => column = column.default_text(s),
            _ => {}
        }
        if f.optional {
            column = column.nullable();
        }
        model = model.field(column);
    }
    model = model
        .field(Column::big_int("version").at_least(1).default_int(1))
        .field(Column::timestamptz("created_at").default_now());
    for u in &spec.unique {
        let mut columns: Vec<String> = intent.tenancy.iter().map(|t| t.column()).collect();
        columns.extend(u.fields.iter().cloned());
        let columns: Vec<&str> = columns.iter().map(String::as_str).collect();
        model = model.unique(&u.name, &columns);
    }
    model
}

/// A unique violation on one of the model's declared sets becomes a 422
/// on its first field; anything else passes through.
fn unique_problem(model: &ModelSpec, err: sqlx::Error) -> Problem {
    let violated = err
        .as_database_error()
        .filter(|d| d.code().as_deref() == Some("23505"))
        .and_then(|d| d.constraint())
        .and_then(|name| model.unique.iter().find(|u| u.name == name));
    match violated {
        Some(u) => {
            let label = model
                .id
                .split_once('.')
                .map_or(model.id.as_str(), |(_, n)| n);
            Problem::validation(vec![trestle_core::FieldError::new(
                format!("/{}", u.fields[0]),
                "already_exists",
                format!(
                    "A {label} with this {} already exists.",
                    u.fields.join(" and ")
                ),
            )])
        }
        None => Problem::from(err),
    }
}

fn field_contract(f: &FieldSpec, required: bool) -> Field {
    let mut field = match f.ty {
        FieldType::Text => {
            let mut x = Field::text(&f.id);
            if f.min.is_some() || f.max.is_some() {
                x = x.chars(
                    f.min.unwrap_or(0).try_into().unwrap_or(0),
                    f.max
                        .unwrap_or(i64::from(u32::MAX))
                        .try_into()
                        .unwrap_or(u32::MAX),
                );
            }
            x
        }
        FieldType::Int => {
            let mut x = Field::integer(&f.id);
            if f.min.is_some() || f.max.is_some() {
                x = x.range(f.min.unwrap_or(i64::MIN), f.max.unwrap_or(i64::MAX));
            }
            x
        }
        FieldType::Bool => Field::boolean(&f.id),
        FieldType::Uuid => Field::uuid(&f.id),
        FieldType::Enum => {
            let values: Vec<&str> = f.values.iter().map(String::as_str).collect();
            let labels: Vec<(&str, &str)> = f
                .labels
                .iter()
                .map(|(v, l)| (v.as_str(), l.as_str()))
                .collect();
            Field::enumeration(&f.id, &values).labels(&labels)
        }
        FieldType::Decimal => Field::decimal(&f.id, f.precision, f.scale),
        FieldType::Money => {
            let codes: Vec<&str> = f.currencies.iter().map(String::as_str).collect();
            Field::money(&f.id, f.precision, f.scale).currencies(&codes)
        }
        FieldType::Date => Field::date(&f.id),
        FieldType::Instant => Field::instant(&f.id),
        FieldType::Duration => Field::duration(&f.id),
        FieldType::Ref => Field::reference(&f.id, f.to.as_deref().unwrap_or_default()),
    };
    if !required {
        field = field.optional();
    }
    if f.optional {
        field = field.nullable();
    }
    field
}

fn representation(model: &ModelSpec) -> Arc<ContractDoc> {
    let mut doc = ContractDoc::output(&model.id, "1").field(Field::uuid("id"));
    for f in &model.fields {
        let mut field = field_contract(f, true);
        if f.optional {
            field = field.nullable();
        }
        doc = doc.field(field);
    }
    let doc = doc
        .field(Field::integer("version"))
        .field(Field::text("created_at"));
    Arc::new(doc)
}

fn policy_id(permission: &str) -> String {
    format!("intent.permission.{permission}")
}

fn create_contract(model: &ModelSpec) -> ContractDoc {
    let mut doc = ContractDoc::input(&format!("{}.create", model.id), "1");
    for f in model.fields.iter().filter(|f| !f.managed()) {
        doc = doc.field(field_contract(f, f.required()));
    }
    doc
}

fn update_contract(model: &ModelSpec, versioned: bool) -> ContractDoc {
    let mut doc = ContractDoc::input(&format!("{}.update", model.id), "1").field(Field::uuid("id"));
    if versioned {
        doc = doc.field(Field::digits("expected_version"));
    }
    for f in model.fields.iter().filter(|f| !f.managed()) {
        doc = doc.field(field_contract(f, false));
    }
    doc
}

enum Kind {
    Crud(
        Crud,
        Vec<String>,
        Option<String>,
        (String, bool),
        BTreeMap<String, String>,
    ),
    Handler(String),
    Action(Arc<Action>),
}

/// A declared state transition (ADR-025), with its guards (ADR-036).
struct Action {
    set: Vec<(String, SetValue)>,
    when: Vec<(String, Vec<Value>)>,
    accept: Vec<String>,
    require: Vec<String>,
    check: Vec<CheckSpec>,
    exists: Vec<Exists>,
    /// `repeat = "unchanged"`: the `set` constants as `eq` checks on the
    /// current row; all true means there is nothing to do.
    unchanged: Option<Vec<CheckSpec>>,
}

/// An `exists` guard resolved to SQL names (ADR-036).
struct Exists {
    /// `crm.affiliation`, and `affiliation` for messages.
    model: String,
    label: String,
    table: String,
    /// Related column, its SQL type, and the field of the acting row.
    matches: Vec<(String, &'static str, String)>,
    /// Related columns that must be null.
    null: Vec<String>,
    /// Field errors point here: the first matched field of the row.
    pointer: String,
}

struct IntentOp<S> {
    info: OperationDescriptor,
    permission: Permission,
    input: ContractDoc,
    output: Arc<ContractDoc>,
    model: Arc<ModelSpec>,
    resolved: Arc<Resolved>,
    kind: Kind,
    _state: PhantomData<fn() -> S>,
}

/// A declared action (ADR-025): POST on one row with `If-Match`.
fn action_operation<S>(
    resolved: &Arc<Resolved>,
    endpoint: &EndpointSpec,
    audiences: &[Audience],
) -> (Arc<dyn DynOperation<S>>, Http)
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    let EndpointSpec::Action {
        id,
        model,
        path,
        permission,
        set,
        when,
        accept,
        require,
        check,
        exists,
        repeat,
        ..
    } = endpoint
    else {
        unreachable!("action endpoint")
    };
    let spec = resolved
        .intent
        .models
        .iter()
        .find(|m| m.id == *model)
        .expect("validated");
    let mut contract = ContractDoc::input(id, "1").field(Field::uuid("id"));
    for name in accept {
        let f = spec.field(name).expect("validated");
        contract = contract.field(field_contract(f, false));
    }
    let contract = contract.field(Field::digits("expected_version"));
    let mut effects = Effects::new().writes(model.as_str());
    for through in through_models(spec) {
        effects = effects.reads(through);
    }
    let mut guards = Vec::new();
    for x in exists {
        let related = resolved
            .intent
            .models
            .iter()
            .find(|m| m.id == x.model)
            .expect("validated");
        effects = effects.reads(related.id.as_str());
        let column = |m: &ModelSpec, name: &str| match m.field(name) {
            Some(f) if f.ty == FieldType::Money => money_columns(f).0,
            _ => name.to_owned(),
        };
        guards.push(Exists {
            model: related.id.clone(),
            label: related
                .id
                .split_once('.')
                .map_or(related.id.as_str(), |(_, n)| n)
                .replace('_', " "),
            table: related.table.clone(),
            matches: x
                .matches
                .iter()
                .map(|(theirs, ours)| {
                    let ty = related.field(theirs).map_or("uuid", |f| f.ty.sql());
                    (theirs.clone(), ty, ours.clone())
                })
                .collect(),
            null: x.null.iter().map(|n| column(related, n)).collect(),
            pointer: x
                .matches
                .first()
                .map(|(_, ours)| format!("/{ours}"))
                .unwrap_or_default(),
        });
    }
    let unchanged = (*repeat == Repeat::Unchanged).then(|| {
        set.iter()
            .filter_map(|(field, value)| match value {
                SetValue::Value(v) => Some(CheckSpec {
                    field: field.clone(),
                    op: Compare::Eq,
                    value: v.clone(),
                }),
                SetValue::Now => None,
            })
            .collect()
    });
    let mut info = OperationDescriptor::new(id.as_str(), "1", Mode::Write)
        .audiences(audiences)
        .summary(format!("{id}: an action on {model}"));
    // The model's write hooks are Rust that may do anything, so an action
    // that runs them declares nothing, like a hooked CRUD write (ADR-039).
    if spec.hooks.is_empty() {
        info = info.effects(effects);
    }
    let op = IntentOp::<S> {
        info,
        permission: Permission::declared(resolved, permission, Reach::Rows(spec.clone())),
        input: contract,
        output: representation(spec),
        model: spec.clone(),
        resolved: resolved.clone(),
        kind: Kind::Action(Arc::new(Action {
            set: set.clone(),
            when: when.clone(),
            accept: accept.clone(),
            require: require.clone(),
            check: check.clone(),
            exists: guards,
            unchanged,
        })),
        _state: PhantomData,
    };
    let mut http = Http::post(path.clone())
        .if_match("expected_version")
        .etag("version");
    if endpoint.idempotent() {
        http = http.idempotency_key();
    }
    (Arc::new(op), http)
}

/// A CRUD operation. Its binding is the resource convention
/// (`ResourceRegistration`), which `trestle.toml` documents.
fn crud_operation<S>(
    resolved: &Arc<Resolved>,
    model: &Arc<ModelSpec>,
    endpoint: &EndpointSpec,
    op: Crud,
    permission: &str,
    audiences: &[Audience],
) -> Arc<dyn DynOperation<S>>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    let EndpointSpec::Crud {
        filter,
        search,
        order,
        hooks,
        ..
    } = endpoint
    else {
        unreachable!("crud endpoint")
    };
    let rep = representation(model);
    let (input, output, summary): (ContractDoc, Arc<ContractDoc>, String) = match op {
        Crud::List => {
            let mut doc = ContractDoc::input(&format!("{}.list", model.id), "1");
            for name in filter {
                let f = model.field(name).expect("validated");
                doc = doc.field(field_contract(f, false));
            }
            if search.is_some() {
                doc = doc.field(Field::text("q").chars(1, 200).optional());
            }
            doc = doc
                .field(Field::integer("limit").range(1, MAX_LIMIT).optional())
                .field(Field::text("cursor").chars(1, 1024).optional());
            let page = ContractDoc::output(&format!("{}.page", model.id), "1")
                .field(Field::items("items", &rep, MAX_LIMIT as u32))
                .field(Field::text("next_cursor").nullable());
            (doc, Arc::new(page), format!("List {}.", model.id))
        }
        Crud::Get => (
            ContractDoc::input(&format!("{}.get", model.id), "1").field(Field::uuid("id")),
            rep.clone(),
            format!("Get one {}.", model.id),
        ),
        Crud::Create => (
            create_contract(model),
            rep.clone(),
            format!("Create a {}.", model.id),
        ),
        Crud::Update => (
            update_contract(model, true),
            rep.clone(),
            format!("Change a {}.", model.id),
        ),
        Crud::Delete => (
            ContractDoc::input(&format!("{}.delete", model.id), "1")
                .field(Field::uuid("id"))
                .field(Field::digits("expected_version")),
            rep.clone(),
            format!("Delete a {}.", model.id),
        ),
    };
    // Generated CRUD touches only its model. A hook is Rust code that may
    // do anything, so a write that runs one declares nothing (ADR-018),
    // and that includes the model's write hooks (ADR-039).
    let hooked = (op.writes() && !model.hooks.is_empty())
        || match op {
            Crud::List | Crud::Get => &[][..],
            Crud::Create => &["validate", "before_create", "after_create"][..],
            Crud::Update => &["validate", "before_update", "after_update"][..],
            Crud::Delete => &["before_delete"][..],
        }
        .iter()
        .any(|point| hooks.contains_key(*point));
    let effects = match (op.writes(), hooked) {
        (false, _) => Some(Effects::new().reads(model.id.as_str())),
        (true, false) => Some(Effects::new().writes(model.id.as_str())),
        (true, true) => None,
    }
    // A scoped grant reads the scopes' `through` models (ADR-040).
    .map(|effects| {
        through_models(model)
            .into_iter()
            .fold(effects, |effects, through| effects.reads(through))
    });
    let hooks = hooks
        .iter()
        .map(|(k, v)| (k.clone(), v.name.clone()))
        .collect();
    let mut info = OperationDescriptor::new(
        format!("{}.{}", model.id, op.as_str()),
        "1",
        if op.writes() { Mode::Write } else { Mode::Read },
    )
    .audiences(audiences)
    .summary(summary);
    if let Some(effects) = effects {
        info = info.effects(effects);
    }
    let built = IntentOp::<S> {
        info,
        permission: Permission::declared(resolved, permission, Reach::Rows(model.clone())),
        input,
        output,
        model: model.clone(),
        resolved: resolved.clone(),
        kind: Kind::Crud(op, filter.clone(), search.clone(), order.clone(), hooks),
        _state: PhantomData,
    };
    Arc::new(built)
}

/// The models a model's row scopes read (ADR-040), each once.
fn through_models(model: &ModelSpec) -> BTreeSet<&str> {
    model
        .scopes
        .values()
        .filter_map(|s| s.through.as_ref().map(|x| x.model.as_str()))
        .filter(|m| *m != model.id)
        .collect()
}

/// The admitted request context of an intent operation.
struct IntentContext {
    actor: ActorId,
    tenant: Option<Uuid>,
    role: Option<String>,
}

fn ctx_of(ctx: &DynContext) -> Result<&IntentContext, Problem> {
    ctx.downcast_ref::<IntentContext>()
        .ok_or_else(|| internal("intent operation got a foreign context"))
}

async fn membership_role(
    conn: &mut PgConnection,
    resolved: &Resolved,
    tenant: Uuid,
    actor: ActorId,
    lock: bool,
) -> Result<Option<String>, Problem> {
    let t = resolved.intent.tenancy.as_ref().expect("tenancy");
    let sql = format!(
        "SELECT role FROM {} WHERE {} = $1 AND actor_id = $2{}",
        t.membership_table(),
        t.column(),
        if lock { " FOR SHARE" } else { "" }
    );
    let role: Option<String> = sqlx::query_scalar(&sql)
        .bind(tenant)
        .bind(actor.0)
        .fetch_optional(conn)
        .await?;
    Ok(role)
}

/// What `role` (in the caller's tenant) holds of `permission` (ADR-040).
enum Grant<'a> {
    None,
    /// Every row of the tenant.
    All,
    /// The rows of these scopes, on the models that declare them.
    Scopes(&'a BTreeSet<String>),
}

fn grant<'a>(resolved: &'a Resolved, permission: &str, role: Option<&str>) -> Grant<'a> {
    let intent = &resolved.intent;
    if permission == AUTHENTICATED {
        return Grant::All;
    }
    let Some(role) = role else {
        return Grant::None;
    };
    if intent
        .roles
        .get(role)
        .is_some_and(|p| p.contains(permission))
    {
        Grant::All
    } else if let Some(scopes) = intent.scoped.get(role).and_then(|g| g.get(permission)) {
        Grant::Scopes(scopes)
    } else {
        Grant::None
    }
}

/// What a [`Permission`] guards, which decides what a scoped grant of it
/// admits (ADR-040).
#[derive(Clone)]
enum Reach {
    /// A generated operation on rows of this model (CRUD, actions): a
    /// scoped grant admits it when the model declares one of the scopes,
    /// and the operation sees only the rows they admit.
    Rows(Arc<ModelSpec>),
    /// Showing an admin or application view, which grants nothing: any
    /// grant admits it.
    Show,
    /// Code that sees the whole tenant (handlers, Rust operations): only
    /// an unscoped grant admits it.
    Tenant,
}

/// What every intent operation reads from a request: the actor, and the
/// tenant header with `[tenancy]`.
fn context_of(resolved: &Resolved) -> ContextDescriptor {
    let ctx = ContextDescriptor::authenticated();
    if resolved.intent.tenancy.is_some() {
        ctx.header(
            "Trestle-Tenant",
            "Tenant to act in. Must be one of the caller's memberships; grants nothing by itself.",
            Some("uuid"),
        )
    } else {
        ctx
    }
}

/// The caller's actor, tenant and role there; a tenant without membership
/// is refused like an unknown one.
async fn resolve<S>(
    resolved: &Resolved,
    invocation: &Invocation,
    state: &S,
) -> Result<DynContext, Problem>
where
    PgPool: FromRef<S>,
{
    let actor = invocation.actor()?;
    if resolved.intent.tenancy.is_none() {
        return Ok(Arc::new(IntentContext {
            actor,
            tenant: None,
            role: None,
        }) as DynContext);
    }
    let tenant = invocation.tenant().ok_or_else(|| {
        Problem::malformed(
            "tenant_required",
            "Select a tenant with the Trestle-Tenant header.",
        )
    })?;
    let pool = PgPool::from_ref(state);
    let mut conn = pool.acquire().await?;
    // Unknown tenants and tenants without membership look the same.
    let role = membership_role(&mut conn, resolved, tenant, actor, false)
        .await?
        .ok_or_else(|| Problem::forbidden("tenant_not_permitted"))?;
    Ok(Arc::new(IntentContext {
        actor,
        tenant: Some(tenant),
        role: Some(role),
    }) as DynContext)
}

/// A permission from `[roles]`, checked the way intent operations check it
/// (ADR-031): the caller's actor, their role in the selected tenant (a
/// tenant without membership is refused like an unknown one), and the
/// role's permissions. Rust operations in an intent project use it as their
/// policy, and it is the `DynPolicy` behind `[[admin]]` (ADR-029), where it
/// shows a view and grants nothing.
///
/// A grant limited to row scopes (`"crm.sell@own"`, ADR-040) does not admit
/// a Rust operation: its [`Ctx`] sees every row of the tenant. Only an
/// unscoped grant of the permission does.
#[derive(Clone)]
pub struct Permission {
    id: String,
    permission: String,
    resolved: Arc<Resolved>,
    reach: Reach,
}

impl Permission {
    /// `permission` as the intent grants it: [`AUTHENTICATED`], or a
    /// permission some role in `[roles]` holds.
    pub fn new(resolved: &Arc<Resolved>, permission: &str) -> Result<Self, String> {
        let intent = &resolved.intent;
        let granted = permission == AUTHENTICATED
            || intent
                .roles
                .values()
                .any(|perms| perms.contains(permission))
            || intent.scoped.values().any(|g| g.contains_key(permission));
        if granted {
            Ok(Self::declared(resolved, permission, Reach::Tenant))
        } else {
            Err(format!(
                "permission {permission:?} is not granted by any role in {}",
                resolved.file
            ))
        }
    }

    /// A permission the intent file names, already checked when it was read.
    fn declared(resolved: &Arc<Resolved>, permission: &str, reach: Reach) -> Self {
        Self {
            id: policy_id(permission),
            permission: permission.to_owned(),
            resolved: resolved.clone(),
            reach,
        }
    }

    /// Whether `role` holds the permission for what it guards.
    fn admits(&self, role: Option<&str>) -> bool {
        match (grant(&self.resolved, &self.permission, role), &self.reach) {
            (Grant::None, _) => false,
            (Grant::All, _) | (Grant::Scopes(_), Reach::Show) => true,
            (Grant::Scopes(_), Reach::Tenant) => false,
            (Grant::Scopes(scopes), Reach::Rows(model)) => {
                scopes.iter().any(|s| model.scopes.contains_key(s))
            }
        }
    }

    /// Whether the grant of role `now` admits every row the grant of role
    /// `then` admitted: a write decided under `then` must not run with
    /// more than the membership now gives (ADR-040).
    fn covers(&self, now: Option<&str>, then: Option<&str>) -> bool {
        if now == then {
            return true;
        }
        let declared = |s: &&String| match &self.reach {
            Reach::Rows(model) => model.scopes.contains_key(*s),
            Reach::Show | Reach::Tenant => true,
        };
        match (
            grant(&self.resolved, &self.permission, now),
            grant(&self.resolved, &self.permission, then),
        ) {
            (_, Grant::None) | (Grant::All, _) => true,
            (Grant::None, _) | (Grant::Scopes(_), Grant::All) => false,
            (Grant::Scopes(now), Grant::Scopes(then)) => {
                then.iter().filter(declared).all(|s| now.contains(s))
            }
        }
    }

    /// The rows the resolved caller's grant admits on the model this
    /// permission guards: `None` for every row (ADR-040).
    fn rows(&self, ctx: &DynContext) -> Result<Option<RowFilter>, Problem> {
        let ctx = ctx_of(ctx)?;
        let Reach::Rows(model) = &self.reach else {
            return Ok(None);
        };
        match grant(&self.resolved, &self.permission, ctx.role.as_deref()) {
            Grant::All => Ok(None),
            Grant::None => Err(Problem::forbidden("permission_denied")),
            Grant::Scopes(names) => {
                let scopes: Vec<ScopeSpec> = names
                    .iter()
                    .filter_map(|n| model.scopes.get(n).cloned())
                    .collect();
                if scopes.is_empty() {
                    return Err(Problem::forbidden("permission_denied"));
                }
                Ok(Some(RowFilter {
                    actor: ctx.actor.0,
                    scopes,
                }))
            }
        }
    }

    /// The policy ID: `intent.permission.<permission>`.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What the check reads from a request: the actor, and the tenant
    /// header with `[tenancy]`.
    pub fn context(&self) -> ContextDescriptor {
        context_of(&self.resolved)
    }

    /// The caller's actor, tenant and role there, for [`Permission::admit`],
    /// [`Permission::recheck`] and [`Ctx::run`].
    pub async fn resolve<S>(
        &self,
        invocation: &Invocation,
        state: &S,
    ) -> Result<DynContext, Problem>
    where
        PgPool: FromRef<S>,
    {
        resolve(&self.resolved, invocation, state).await
    }

    /// Whether the resolved caller holds the permission.
    pub fn admit(&self, ctx: &DynContext) -> Result<(), Problem> {
        let ctx = ctx_of(ctx)?;
        if self.admits(ctx.role.as_deref()) {
            Ok(())
        } else {
            Err(Problem::forbidden("permission_denied"))
        }
    }

    /// The same check inside a write's transaction, on the membership as it
    /// is now (locked `FOR SHARE` until commit). A role changed since the
    /// caller was resolved must still grant every row the old one did.
    pub async fn recheck(&self, conn: &mut PgConnection, ctx: &DynContext) -> Result<(), Problem> {
        let ctx = ctx_of(ctx)?;
        let Some(tenant) = ctx.tenant else {
            return Ok(());
        };
        let role = membership_role(conn, &self.resolved, tenant, ctx.actor, true)
            .await?
            .ok_or_else(|| Problem::forbidden("tenant_not_permitted"))?;
        if self.admits(Some(&role)) && self.covers(Some(&role), ctx.role.as_deref()) {
            Ok(())
        } else {
            Err(Problem::forbidden("permission_denied"))
        }
    }
}

impl<S> DynPolicy<S> for Permission
where
    S: Send + Sync + 'static,
    PgPool: FromRef<S>,
{
    fn id(&self) -> &str {
        &self.id
    }

    fn context(&self) -> ContextDescriptor {
        Permission::context(self)
    }

    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
        state: &'a S,
    ) -> BoxFuture<'a, Result<DynContext, Problem>> {
        Box::pin(self.resolve(invocation, state))
    }

    fn admit(&self, ctx: &DynContext) -> Result<(), Problem> {
        Permission::admit(self, ctx)
    }
}

impl<S> DynOperation<S> for IntentOp<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    fn info(&self) -> &OperationDescriptor {
        &self.info
    }

    fn policy_id(&self) -> &str {
        self.permission.id()
    }

    fn context(&self) -> ContextDescriptor {
        context_of(&self.resolved)
    }

    fn input(&self) -> &ContractDoc {
        &self.input
    }

    fn output(&self) -> &ContractDoc {
        &self.output
    }

    fn resolve_context<'a>(
        &'a self,
        invocation: &'a Invocation,
        state: &'a S,
    ) -> BoxFuture<'a, Result<DynContext, Problem>> {
        Box::pin(resolve(&self.resolved, invocation, state))
    }

    fn admit(&self, ctx: &DynContext) -> Result<(), Problem> {
        self.permission.admit(ctx)
    }

    fn recheck<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        ctx: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(self.permission.recheck(conn, ctx))
    }

    fn run(
        self: Arc<Self>,
        conn: DynConn,
        ctx: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)> {
        Box::pin(async move {
            let resolved = self.resolved.clone();
            let rows = match self.permission.rows(&ctx) {
                Ok(rows) => rows,
                Err(problem) => return (conn, Err(problem)),
            };
            Ctx::run(&resolved, conn, &ctx, async |handle| {
                self.dispatch(&handle, input, rows.as_ref()).await
            })
            .await
        })
    }
}

impl<S> IntentOp<S> {
    async fn hook(
        &self,
        ctx: &Ctx,
        hooks: &BTreeMap<String, String>,
        point: &str,
        row: Row,
    ) -> Result<Row, Problem> {
        match hooks.get(point) {
            None => Ok(row),
            Some(name) => {
                let hook = self
                    .resolved
                    .handlers
                    .hooks
                    .get(name)
                    .ok_or_else(|| internal(&format!("hook {name:?} is not registered")))?;
                let out = hook(ctx.clone(), row).await?;
                if out.model.id != self.model.id {
                    return Err(internal(&format!(
                        "hook {name:?} returned a {} row",
                        out.model.id
                    )));
                }
                Ok(out)
            }
        }
    }

    /// Runs the operation. `rows` is the scoped grant's narrowing
    /// (ADR-040): the rows it reads and changes are found through it, so
    /// one outside it is a 404, and a row it writes must still be inside
    /// it afterwards ([`inside`]).
    async fn dispatch(
        &self,
        ctx: &Ctx,
        input: Value,
        rows: Option<&RowFilter>,
    ) -> Result<Value, Problem> {
        let model = &self.model;
        let Value::Object(mut input) = input else {
            return Err(Problem::malformed(
                "invalid_body",
                "Expected a JSON object.",
            ));
        };
        match &self.kind {
            Kind::Action(action) => {
                let id = input
                    .remove("id")
                    .and_then(|v| v.as_str().and_then(|s| s.parse::<Uuid>().ok()))
                    .ok_or_else(Problem::not_found)?;
                let version = match input.remove("expected_version") {
                    None | Some(Value::Null) => return Err(Problem::precondition_required()),
                    Some(v) => v
                        .as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .ok_or_else(|| {
                            Problem::malformed(
                                "invalid_version",
                                "The version in If-Match is not a valid version.",
                            )
                        })?,
                };
                let model = &self.model;
                // Everything up to the write runs on the locked row; the
                // write itself, with the model's write hooks, follows
                // (ADR-039).
                enum Planned {
                    Unchanged(Row),
                    Write(Row, Map<String, Value>),
                }
                let planned = ctx
                    .within(rows, async |conn, scope| {
                        let row = fetch(conn, model, scope, id, true)
                            .await?
                            .ok_or_else(Problem::not_found)?;
                        let current = &row.data;
                        if current.get("version").and_then(Value::as_i64) != Some(version) {
                            return Err(Problem::precondition_failed());
                        }
                        // Asking for the state the row already has (ADR-036).
                        if let Some(unchanged) = &action.unchanged
                            && compare(conn, model, current, unchanged)
                                .await?
                                .into_iter()
                                .all(|met| met)
                        {
                            return Ok(Planned::Unchanged(row));
                        }
                        for (field, allowed) in &action.when {
                            if !allowed.contains(current.get(field).unwrap_or(&Value::Null)) {
                                let list: Vec<String> =
                                    allowed.iter().map(Value::to_string).collect();
                                return Err(Problem::conflict("invalid_state").with_detail(
                                    &format!("This needs {field} to be {}.", list.join(" or ")),
                                ));
                            }
                        }
                        let mut values: Map<String, Value> = input
                            .iter()
                            .filter(|(k, _)| action.accept.contains(k))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        for (field, value) in &action.set {
                            let value = match value {
                                SetValue::Value(v) => v.clone(),
                                SetValue::Now => {
                                    let now: String = sqlx::query_scalar(&format!(
                                        "SELECT {}",
                                        trestle_historical::sql::iso_instant("now()")
                                    ))
                                    .fetch_one(&mut *conn)
                                    .await?;
                                    Value::String(now)
                                }
                            };
                            values.insert(field.clone(), value);
                        }
                        guard(conn, model, scope, action, current, &values).await?;
                        Ok(Planned::Write(row, values))
                    })
                    .await?;
                let row = match planned {
                    Planned::Unchanged(row) => row,
                    Planned::Write(current, values) => {
                        let change = Change::Update {
                            id,
                            version: Some(version),
                        };
                        let row = ctx
                            .write(model, &self.info.id, change, Some(current), values)
                            .await?;
                        inside(ctx, model, rows, &row).await?;
                        row
                    }
                };
                Ok(row.to_json())
            }
            Kind::Handler(name) => {
                let handler = self
                    .resolved
                    .handlers
                    .handlers
                    .get(name)
                    .ok_or_else(|| internal(&format!("handler {name:?} is not registered")))?;
                let row = handler(ctx.clone(), Input { data: input }).await?;
                Ok(row.to_json())
            }
            Kind::Crud(op, filter, search, order, hooks) => {
                let id = input
                    .remove("id")
                    .and_then(|v| v.as_str().and_then(|s| s.parse::<Uuid>().ok()));
                // A version that is present but unusable (for example past
                // i64) is refused; it never turns into "no precondition".
                let version = match input.remove("expected_version") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_str().and_then(|s| s.parse::<i64>().ok()).ok_or_else(
                        || {
                            Problem::malformed(
                                "invalid_version",
                                "The version in If-Match is not a valid version.",
                            )
                        },
                    )?),
                };
                if matches!(op, Crud::Update | Crud::Delete) && version.is_none() {
                    return Err(Problem::precondition_required());
                }
                match op {
                    Crud::List => {
                        let limit = input
                            .remove("limit")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(50)
                            .clamp(1, MAX_LIMIT);
                        let cursor = input.remove("cursor");
                        let term = input
                            .remove("q")
                            .and_then(|v| v.as_str().map(|s| s.trim().to_lowercase()))
                            .filter(|s| !s.is_empty());
                        let filter: Map<String, Value> = input
                            .into_iter()
                            .filter(|(k, _)| filter.contains(k))
                            .collect();
                        // A cursor is valid only for exactly this query.
                        let shape = json!({
                            "op": &*self.info.id,
                            "version": &*self.info.version,
                            "projection": trestle_protocol::digest(&self.output.json_schema()),
                            "tenant": ctx.tenant(),
                            "actor": ctx.actor().0,
                            "search": term,
                            "limit": limit,
                            "filter": filter,
                            "order": [order.0, order.1],
                        });
                        let after = match cursor.as_ref().and_then(Value::as_str) {
                            Some(c) => Some(position(&decode_cursor(&shape, c)?)?),
                            None => None,
                        };
                        let mut items = ctx
                            .within(rows, async |conn, scope| {
                                list(
                                    conn,
                                    model,
                                    scope,
                                    &filter,
                                    search.as_deref().zip(term.as_deref()),
                                    (order.0.as_str(), order.1),
                                    limit + 1,
                                    after.as_ref(),
                                )
                                .await
                            })
                            .await?;
                        let next = if items.len() as i64 > limit {
                            items.truncate(limit as usize);
                            items.last().map(|last| {
                                let row = last.to_json();
                                encode_cursor(&shape, &json!([row[order.0.as_str()], row["id"]]))
                            })
                        } else {
                            None
                        };
                        Ok(json!({
                            "items": items.iter().map(Row::to_json).collect::<Vec<_>>(),
                            "next_cursor": next,
                        }))
                    }
                    Crud::Get => {
                        let id = id.ok_or_else(Problem::not_found)?;
                        let row = ctx
                            .within(rows, async |conn, scope| {
                                fetch(conn, model, scope, id, false).await
                            })
                            .await?
                            .ok_or_else(Problem::not_found)?;
                        Ok(row.to_json())
                    }
                    Crud::Create => {
                        let mut row = Row::new(model.clone(), input);
                        row = self.hook(ctx, hooks, "validate", row).await?;
                        row = self.hook(ctx, hooks, "before_create", row).await?;
                        let values = crud_writes(model, &row.data);
                        let created = ctx
                            .write(model, &self.info.id, Change::Create, None, values)
                            .await?;
                        inside(ctx, model, rows, &created).await?;
                        let created = self.hook(ctx, hooks, "after_create", created).await?;
                        Ok(created.to_json())
                    }
                    Crud::Update => {
                        let id = id.ok_or_else(Problem::not_found)?;
                        let current = ctx
                            .within(rows, async |conn, scope| {
                                fetch(conn, model, scope, id, true).await
                            })
                            .await?
                            .ok_or_else(Problem::not_found)?;
                        if version.is_some_and(|v| {
                            Some(v) != current.data.get("version").and_then(Value::as_i64)
                        }) {
                            return Err(Problem::precondition_failed());
                        }
                        let mut merged = current.clone();
                        for (k, v) in input {
                            merged.data.insert(k, v);
                        }
                        merged = self.hook(ctx, hooks, "validate", merged).await?;
                        merged = self.hook(ctx, hooks, "before_update", merged).await?;
                        let changes = crud_writes(model, &merged.data);
                        let updated = ctx
                            .write(
                                model,
                                &self.info.id,
                                Change::Update { id, version },
                                Some(current),
                                changes,
                            )
                            .await?;
                        inside(ctx, model, rows, &updated).await?;
                        let updated = self.hook(ctx, hooks, "after_update", updated).await?;
                        Ok(updated.to_json())
                    }
                    Crud::Delete => {
                        let id = id.ok_or_else(Problem::not_found)?;
                        let current = ctx
                            .within(rows, async |conn, scope| {
                                fetch(conn, model, scope, id, true).await
                            })
                            .await?
                            .ok_or_else(Problem::not_found)?;
                        if version.is_some_and(|v| {
                            Some(v) != current.data.get("version").and_then(Value::as_i64)
                        }) {
                            return Err(Problem::precondition_failed());
                        }
                        self.hook(ctx, hooks, "before_delete", current.clone())
                            .await?;
                        let deleted = ctx
                            .write(
                                model,
                                &self.info.id,
                                Change::Delete { id, version },
                                Some(current),
                                Map::new(),
                            )
                            .await?;
                        Ok(deleted.to_json())
                    }
                }
            }
        }
    }
}

/// A row a scoped grant just wrote (created, updated or acted on) must be
/// inside one of its scopes as written, with its `through` row locked
/// `FOR SHARE` (ADR-040). Otherwise the write is refused with a 403
/// `out_of_scope` and rolls back: a salesperson can't create a record
/// owned by someone else, or hand one over.
async fn inside(
    ctx: &Ctx,
    model: &Arc<ModelSpec>,
    rows: Option<&RowFilter>,
    row: &Row,
) -> Result<(), Problem> {
    let Some(rows) = rows else {
        return Ok(());
    };
    let id = row
        .id()
        .ok_or_else(|| internal("a written row has no id"))?;
    let found = ctx
        .within(Some(rows), async |conn, scope| {
            fetch(conn, model, scope, id, true).await
        })
        .await?;
    match found {
        Some(_) => Ok(()),
        None => Err(Problem::forbidden("out_of_scope")
            .with_detail("This change would put the record outside the records you may change.")),
    }
}

/// An action's `require`, `check` and `exists` (ADR-036), on the row as it
/// would be after the write. Every unmet condition is reported in one 409
/// `requirement_unmet`; a null value never meets a condition.
async fn guard(
    conn: &mut PgConnection,
    model: &ModelSpec,
    scope: &Scope,
    action: &Action,
    current: &Map<String, Value>,
    values: &Map<String, Value>,
) -> Result<(), Problem> {
    if action.require.is_empty() && action.check.is_empty() && action.exists.is_empty() {
        return Ok(());
    }
    let mut after = current.clone();
    for (k, v) in values {
        after.insert(k.clone(), v.clone());
    }
    let value = |field: &str| after.get(field).filter(|v| !v.is_null());
    let mut errors = Vec::new();
    for field in &action.require {
        if value(field).is_none() {
            errors.push(trestle_core::FieldError::new(
                format!("/{field}"),
                "required",
                format!("This needs {field}."),
            ));
        }
    }
    let met = compare(conn, model, &after, &action.check).await?;
    for (check, met) in action.check.iter().zip(met) {
        if met {
            continue;
        }
        let field = &check.field;
        let shown = match &check.value {
            Value::Object(m) => format!(
                "{} {}",
                m.get("amount").and_then(Value::as_str).unwrap_or_default(),
                m.get("currency")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let params = json!({ "op": check.op.as_str(), "value": check.value });
        let theirs = check.value.get("currency").and_then(Value::as_str);
        let ours = value(field)
            .and_then(|v| v.get("currency"))
            .and_then(Value::as_str);
        let error = match (theirs, ours) {
            (Some(theirs), Some(ours)) if theirs != ours => trestle_core::FieldError::new(
                format!("/{field}"),
                "currency_mismatch",
                format!("{field} is in {ours}, so it cannot be compared with {shown}."),
            ),
            _ => trestle_core::FieldError::new(
                format!("/{field}"),
                "check",
                format!("{field} must be {} {shown}.", compare_words(check.op)),
            ),
        };
        errors.push(error.with_params(params));
    }
    for x in &action.exists {
        if !exists(conn, scope, x, &after).await? {
            let matched: Vec<String> = x
                .matches
                .iter()
                .map(|(theirs, _, ours)| format!("{theirs} = {ours}"))
                .collect();
            errors.push(
                trestle_core::FieldError::new(
                    x.pointer.clone(),
                    "exists",
                    format!(
                        "This needs a matching {} ({}).",
                        x.label,
                        matched.join(", ")
                    ),
                )
                .with_params(json!({ "model": x.model })),
            );
        }
    }
    if errors.is_empty() {
        return Ok(());
    }
    let detail: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
    Err(Problem::conflict("requirement_unmet")
        .with_detail(&detail.join(" "))
        .with_errors(errors))
}

fn compare_words(op: Compare) -> &'static str {
    match op {
        Compare::Eq => "equal to",
        Compare::Ne => "other than",
        Compare::Lt => "less than",
        Compare::Le => "at most",
        Compare::Gt => "more than",
        Compare::Ge => "at least",
    }
}

/// Evaluates comparisons on a row in one query, in PostgreSQL's types
/// (numeric, date, timestamptz, interval), so `"10.5"` equals `"10.50"`
/// and offsets don't matter. A null side is never met.
async fn compare(
    conn: &mut PgConnection,
    model: &ModelSpec,
    row: &Map<String, Value>,
    checks: &[CheckSpec],
) -> Result<Vec<bool>, Problem> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    let text = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut binds = Vec::new();
    let mut exprs = Vec::new();
    for c in checks {
        let f = model
            .field(&c.field)
            .ok_or_else(|| internal(&format!("{:?} is not a field of {}", c.field, model.id)))?;
        let op = c.op.sql();
        let expr = if f.ty == FieldType::Money {
            let (amount, currency) = match &c.value {
                Value::Object(m) => (
                    m.get("amount").map(text).unwrap_or_default(),
                    m.get("currency").map(text),
                ),
                other => (text(other), None),
            };
            binds.push(amount);
            let mut expr = format!(
                "($1::jsonb->'{0}'->>'amount')::numeric {op} ${1}::numeric",
                f.id,
                binds.len() + 1
            );
            if let Some(currency) = currency {
                binds.push(currency);
                expr = format!(
                    "{expr} AND ($1::jsonb->'{}'->>'currency') = ${}",
                    f.id,
                    binds.len() + 1
                );
            }
            expr
        } else {
            binds.push(text(&c.value));
            let ty = f.ty.sql();
            format!(
                "($1::jsonb->>'{}')::{ty} {op} ${}::{ty}",
                f.id,
                binds.len() + 1
            )
        };
        exprs.push(format!("COALESCE(({expr}), false)"));
    }
    let sql = format!("SELECT ARRAY[{}]::boolean[]", exprs.join(", "));
    let mut q =
        sqlx::query_scalar::<_, Vec<bool>>(&sql).bind(Value::Object(row.clone()).to_string());
    for b in binds {
        q = q.bind(b);
    }
    Ok(q.fetch_one(&mut *conn).await?)
}

/// Whether a related row in the caller's tenant matches `row`. The row
/// found is locked `FOR SHARE` until the action commits, so no concurrent
/// update (setting `ended_at`, say) or delete can undo the condition
/// before then; one waiting on such an update rechecks the new version.
async fn exists(
    conn: &mut PgConnection,
    scope: &Scope,
    x: &Exists,
    row: &Map<String, Value>,
) -> Result<bool, Problem> {
    let mut binds = Vec::new();
    for (_, _, ours) in &x.matches {
        match row.get(ours) {
            None | Some(Value::Null) => return Ok(false),
            Some(Value::String(s)) => binds.push(s.clone()),
            Some(other) => binds.push(other.to_string()),
        }
    }
    let mut conditions = Vec::new();
    let mut next = 1;
    if let Some((col, _)) = &scope.tenant {
        conditions.push(format!("t.{col} = $1"));
        next = 2;
    }
    for (i, (theirs, ty, _)) in x.matches.iter().enumerate() {
        conditions.push(format!("t.{theirs} = ${}::{ty}", next + i));
    }
    for column in &x.null {
        conditions.push(format!("t.{column} IS NULL"));
    }
    let sql = format!(
        "SELECT 1 FROM {} t WHERE {} LIMIT 1 FOR SHARE",
        x.table,
        conditions.join(" AND ")
    );
    let mut q = sqlx::query_scalar::<_, i32>(&sql);
    if let Some((_, t)) = &scope.tenant {
        q = q.bind(*t);
    }
    for b in binds {
        q = q.bind(b);
    }
    Ok(q.fetch_optional(&mut *conn).await?.is_some())
}

// SQL. Identifiers are validated lowercase names from the intent; values
// are always bound.

fn representation_sql(model: &ModelSpec) -> String {
    let mut parts = vec!["'id', t.id".to_owned()];
    for f in &model.fields {
        if f.ty == FieldType::Money {
            let (amount, currency) = money_columns(f);
            parts.push(format!(
                "'{}', CASE WHEN t.{amount} IS NULL THEN NULL \
                 ELSE jsonb_build_object('amount', t.{amount}::text, 'currency', t.{currency}) END",
                f.id
            ));
            continue;
        }
        // Decimals, dates, instants and durations render in their contract
        // form (ADR-023): a decimal never becomes a JSON number.
        let column = format!("t.{}", f.id);
        let value = trestle_historical::sql::text_expr(column_type(f), &column).unwrap_or(column);
        parts.push(format!("'{}', {value}", f.id));
    }
    parts.push("'version', t.version".into());
    parts.push("'created_at', t.created_at".into());
    format!("jsonb_build_object({})::text", parts.join(", "))
}

fn column_type(f: &FieldSpec) -> trestle_historical::schema::ColumnType {
    use trestle_historical::schema::ColumnType;
    match f.ty {
        FieldType::Decimal | FieldType::Money => ColumnType::Numeric {
            precision: f.precision,
            scale: f.scale,
        },
        FieldType::Date => ColumnType::Date,
        FieldType::Instant => ColumnType::TimestampTz,
        FieldType::Duration => ColumnType::Interval,
        FieldType::Int => ColumnType::BigInt,
        FieldType::Bool => ColumnType::Boolean,
        FieldType::Uuid | FieldType::Ref => ColumnType::Uuid,
        FieldType::Text | FieldType::Enum => ColumnType::Text,
    }
}

fn parse_rows(model: &Arc<ModelSpec>, texts: Vec<String>) -> Result<Vec<Row>, Problem> {
    texts
        .into_iter()
        .map(|t| match serde_json::from_str::<Value>(&t) {
            Ok(Value::Object(map)) => Ok(Row::new(model.clone(), map)),
            _ => Err(internal("row did not render as an object")),
        })
        .collect()
}

async fn fetch(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    id: Uuid,
    lock: bool,
) -> Result<Option<Row>, Problem> {
    let (tenant_sql, tenant) = match &scope.tenant {
        Some((col, t)) => (format!("{col} = $2 AND "), Some(*t)),
        None => (String::new(), None),
    };
    // A row outside the caller's scopes is missing, exactly like another
    // tenant's (ADR-040).
    let rows_sql = scope
        .rows
        .as_ref()
        .map(|r| {
            let n = if tenant.is_some() { 3 } else { 2 };
            let tenant = scope.tenant.as_ref().map(|(c, _)| c.as_str());
            format!(" AND {}", r.sql(tenant, n, lock))
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT {} FROM {} t WHERE {tenant_sql}t.id = $1{rows_sql}{}",
        representation_sql(model),
        model.table,
        if lock { " FOR UPDATE OF t" } else { "" }
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql).bind(id);
    if let Some(t) = tenant {
        q = q.bind(t);
    }
    if let Some(r) = &scope.rows {
        q = q.bind(r.actor);
    }
    let text = q.fetch_optional(conn).await?;
    Ok(parse_rows(model, text.into_iter().collect())?.pop())
}

/// `[order key, id]` from a cursor.
fn position(value: &Value) -> Result<(Value, Uuid), Problem> {
    let invalid = || Problem::malformed("invalid_cursor", "The cursor is not valid.");
    let id = value[1]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(invalid)?;
    match &value[0] {
        Value::Null => Err(invalid()),
        key => Ok((key.clone(), id)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn list(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    filter: &Map<String, Value>,
    search: Option<(&str, &str)>,
    order: (&str, bool),
    limit: i64,
    after: Option<&(Value, Uuid)>,
) -> Result<Vec<Row>, Problem> {
    let mut conditions = Vec::new();
    let mut next = 2;
    if let Some((col, _)) = &scope.tenant {
        conditions.push(format!("t.{col} = ${next}"));
        next += 1;
    }
    // Only the rows the caller's scopes admit, so pages, cursors and
    // filters never reveal another (ADR-040).
    if let Some(rows) = &scope.rows {
        let tenant = scope.tenant.as_ref().map(|(c, _)| c.as_str());
        conditions.push(rows.sql(tenant, next, false));
        next += 1;
    }
    if after.is_some() {
        // Row comparison: strictly past the previous page's last row.
        let ty = match model.field(order.0) {
            Some(f) => f.ty.sql(),
            None => "timestamptz",
        };
        let op = if order.1 { "<" } else { ">" };
        conditions.push(format!(
            "(t.{}, t.id) {op} (${next}::{ty}, ${})",
            order.0,
            next + 1
        ));
    }
    for (name, _) in filter.iter() {
        let f = model
            .field(name)
            .ok_or_else(|| internal(&format!("{name:?} is not a field of {}", model.id)))?;
        conditions.push(format!("t.{name} = ($1::jsonb->>'{name}')::{}", f.ty.sql()));
    }
    if let Some((field, _)) = search {
        conditions.push(format!(
            "strpos(lower(t.{field}), $1::jsonb->>'__search') > 0"
        ));
    }
    let where_ = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let dir = if order.1 { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT {} FROM {} t {where_} ORDER BY t.{} {dir}, t.id {dir} LIMIT {}",
        representation_sql(model),
        model.table,
        order.0,
        limit.clamp(1, MAX_LIMIT + 1)
    );
    let mut values = filter.clone();
    if let Some((_, term)) = search {
        values.insert("__search".into(), json!(term));
    }
    let mut q = sqlx::query_scalar::<_, String>(&sql).bind(Value::Object(values).to_string());
    if let Some((_, t)) = &scope.tenant {
        q = q.bind(*t);
    }
    if let Some(rows) = &scope.rows {
        q = q.bind(rows.actor);
    }
    if let Some((key, id)) = after {
        let key = match key {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        q = q.bind(key).bind(*id);
    }
    parse_rows(model, q.fetch_all(conn).await?)
}

/// `<id>_amount` and `<id>_currency`.
fn money_columns(f: &FieldSpec) -> (String, String) {
    (format!("{}_amount", f.id), format!("{}_currency", f.id))
}

/// Field values keyed by column: a money value becomes its two columns.
fn flatten(model: &ModelSpec, mut values: Map<String, Value>) -> Map<String, Value> {
    for f in model.fields.iter().filter(|f| f.ty == FieldType::Money) {
        let Some(value) = values.remove(&f.id) else {
            continue;
        };
        let (amount, currency) = money_columns(f);
        values.insert(amount, value.get("amount").cloned().unwrap_or(Value::Null));
        values.insert(
            currency,
            value.get("currency").cloned().unwrap_or(Value::Null),
        );
    }
    values
}

/// The columns of `model` that `values` (flattened) sets.
fn writable(model: &ModelSpec, values: &Map<String, Value>) -> Vec<String> {
    model
        .fields
        .iter()
        .flat_map(|f| match f.ty {
            FieldType::Money => {
                let (amount, currency) = money_columns(f);
                vec![amount, currency]
            }
            _ => vec![f.id.clone()],
        })
        .filter(|column| values.contains_key(column))
        .collect()
}

/// Every `ref` value names a row of its model in the caller's tenant. The
/// row is locked `FOR KEY SHARE` until the write commits, so it cannot be
/// deleted in between; another tenant's row is reported like a missing one.
async fn check_references(
    conn: &mut PgConnection,
    model: &ModelSpec,
    scope: &Scope,
    values: &Map<String, Value>,
) -> Result<(), Problem> {
    let mut errors = Vec::new();
    for f in &model.fields {
        let (Some(to), Some(table)) = (&f.to, &f.to_table) else {
            continue;
        };
        let Some(Value::String(text)) = values.get(&f.id) else {
            continue;
        };
        let id = Uuid::parse_str(text).map_err(|_| internal("ref value is not a UUID"))?;
        let (tenant_sql, tenant) = match &scope.tenant {
            Some((col, t)) => (format!(" AND {col} = $2"), Some(*t)),
            None => (String::new(), None),
        };
        let sql = format!("SELECT 1 FROM {table} WHERE id = $1{tenant_sql} FOR KEY SHARE");
        let mut q = sqlx::query_scalar::<_, i32>(&sql).bind(id);
        if let Some(t) = tenant {
            q = q.bind(t);
        }
        if q.fetch_optional(&mut *conn).await?.is_none() {
            let label = to.split_once('.').map_or(to.as_str(), |(_, n)| n);
            errors.push(trestle_core::FieldError::new(
                format!("/{}", f.id),
                "not_found",
                format!("No {label} has this ID."),
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Problem::validation(errors))
    }
}

async fn insert(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    values: Map<String, Value>,
) -> Result<Row, Problem> {
    check_references(conn, model, scope, &values).await?;
    let mut values = flatten(model, values);
    let mut cols = vec!["id".to_owned()];
    values.insert("id".into(), json!(Uuid::now_v7()));
    if let Some((col, t)) = &scope.tenant {
        cols.push(col.clone());
        values.insert(col.clone(), json!(t));
    }
    cols.extend(writable(model, &values));
    let list = cols.join(", ");
    let sql = format!(
        "INSERT INTO {table} AS t ({list}) SELECT {list} FROM jsonb_populate_record(NULL::{table}, $1::jsonb) \
         RETURNING {rep}",
        table = model.table,
        rep = representation_sql(model)
    );
    let text: String = sqlx::query_scalar(&sql)
        .bind(Value::Object(values).to_string())
        .fetch_one(conn)
        .await
        .map_err(|e| unique_problem(model, e))?;
    parse_rows(model, vec![text])?
        .pop()
        .ok_or_else(|| internal("insert returned nothing"))
}

async fn update(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    id: Uuid,
    values: Map<String, Value>,
    expected_version: Option<i64>,
) -> Result<Option<Row>, Problem> {
    check_references(conn, model, scope, &values).await?;
    let values = flatten(model, values);
    let cols = writable(model, &values);
    let mut set = vec!["version = t.version + 1".to_owned()];
    if !cols.is_empty() {
        let list = cols.join(", ");
        set.push(format!(
            "({list}) = (SELECT {list} FROM jsonb_populate_record(NULL::{}, $1::jsonb))",
            model.table
        ));
    }
    let mut conditions = vec!["t.id = $2".to_owned()];
    if let Some((col, _)) = &scope.tenant {
        conditions.push(format!("t.{col} = $3"));
    }
    if expected_version.is_some() {
        conditions.push("t.version = $4".into());
    }
    let sql = format!(
        "UPDATE {} AS t SET {} WHERE {} RETURNING {}",
        model.table,
        set.join(", "),
        conditions.join(" AND "),
        representation_sql(model)
    );
    let tenant = scope.tenant.as_ref().map(|(_, t)| *t).unwrap_or_default();
    let text: Option<String> = sqlx::query_scalar(&sql)
        .bind(Value::Object(values).to_string())
        .bind(id)
        .bind(tenant)
        .bind(expected_version.unwrap_or_default())
        .fetch_optional(conn)
        .await
        .map_err(|e| unique_problem(model, e))?;
    Ok(parse_rows(model, text.into_iter().collect())?.pop())
}

async fn delete(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    id: Uuid,
    expected_version: Option<i64>,
) -> Result<Option<Row>, Problem> {
    let mut conditions = vec!["t.id = $1".to_owned()];
    if let Some((col, _)) = &scope.tenant {
        conditions.push(format!("t.{col} = $2"));
    }
    if expected_version.is_some() {
        conditions.push("t.version = $3".into());
    }
    let sql = format!(
        "DELETE FROM {} AS t WHERE {} RETURNING {}",
        model.table,
        conditions.join(" AND "),
        representation_sql(model)
    );
    let tenant = scope.tenant.as_ref().map(|(_, t)| *t).unwrap_or_default();
    let text: Option<String> = sqlx::query_scalar(&sql)
        .bind(id)
        .bind(tenant)
        .bind(expected_version.unwrap_or_default())
        .fetch_optional(conn)
        .await
        .map_err(|e| {
            // foreign_key_violation: another row's ref points here.
            if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23503") {
                Problem::conflict("still_referenced")
                    .with_detail("Other records refer to this one; change or remove them first.")
            } else {
                Problem::from(e)
            }
        })?;
    Ok(parse_rows(model, text.into_iter().collect())?.pop())
}
