//! Running an intent: models, contracts and operations built from the file,
//! executed through the same executor as hand-written operations.
//!
//! Every query is tenant-scoped in SQL before anything else (`[tenancy]`),
//! writes recheck membership inside their transaction (`FOR SHARE`), and
//! identifiers come only from the validated intent; values are always bound.

use std::collections::BTreeMap;
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
    BoxFuture, ContextDescriptor, DynConn, DynContext, DynOperation, Effects, Mode,
    OperationDescriptor,
};
use trestle_core::registry::{App as CoreApp, AppBuilder, Http};
use trestle_historical::schema::{Column, Model};
use trestle_protocol::{Audience, ContractDoc, Field};
use uuid::Uuid;

use super::spec::{AUTHENTICATED, Crud, EndpointSpec, FieldSpec, FieldType, Intent, ModelSpec};

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

    /// Sets a declared field (hooks use this to adjust a record).
    pub fn set(&mut self, field: &str, value: impl Serialize) -> Result<(), Problem> {
        self.known(field)?;
        if matches!(field, "id" | "version" | "created_at") {
            return Err(internal(&format!("field {field:?} is managed by Trestle")));
        }
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
#[derive(Clone)]
pub struct Ctx {
    inner: Arc<CtxInner>,
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

    async fn with<T>(
        &self,
        f: impl AsyncFnOnce(&mut PgConnection, &Scope) -> Result<T, Problem>,
    ) -> Result<T, Problem> {
        let mut guard = self.inner.conn.lock().await;
        let conn = guard
            .as_mut()
            .ok_or_else(|| internal("a handler used its context after the operation ended"))?;
        let scope = self.inner.intent.scope(self.inner.tenant);
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

    /// Records matching `filter` (field equality), newest first.
    pub async fn list(&self, model: &str, filter: Value) -> Result<Vec<Row>, Problem> {
        let model = &self.model(model)?;
        let filter = filter
            .as_object()
            .cloned()
            .ok_or_else(|| internal("a list filter must be a JSON object"))?;
        self.with(async |conn, scope| {
            list(conn, model, scope, &filter, ("created_at", true), MAX_LIMIT).await
        })
        .await
    }

    /// Creates a record. Values are validated against the model's create
    /// contract, as if they came from a request.
    pub async fn create(&self, model: &str, values: Value) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        let contract = create_contract(model);
        let values = contract.validate(&values).map_err(Problem::from)?;
        let values = values.as_object().cloned().unwrap_or_default();
        self.with(async |conn, scope| insert(conn, model, scope, values).await)
            .await
    }

    /// Changes fields of a record (no version check), or 404.
    pub async fn update(&self, model: &str, id: Uuid, values: Value) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        let mut instance = values
            .as_object()
            .cloned()
            .ok_or_else(|| internal("update values must be a JSON object"))?;
        instance.insert("id".into(), json!(id));
        let contract = update_contract(model, false);
        let values = contract
            .validate(&Value::Object(instance))
            .map_err(Problem::from)?;
        let mut values = values.as_object().cloned().unwrap_or_default();
        values.remove("id");
        self.with(async |conn, scope| {
            update(conn, model, scope, id, values, None)
                .await?
                .ok_or_else(Problem::not_found)
        })
        .await
    }

    /// Deletes a record, or 404.
    pub async fn delete(&self, model: &str, id: Uuid) -> Result<Row, Problem> {
        self.check_writable()?;
        let model = &self.model(model)?;
        self.with(async |conn, scope| {
            delete(conn, model, scope, id, None)
                .await?
                .ok_or_else(Problem::not_found)
        })
        .await
    }
}

/// A Rust handler for an endpoint (`handler = "notes::pin"`).
pub type HandlerFn =
    Arc<dyn Fn(Ctx, Input) -> BoxFuture<'static, Result<Row, Problem>> + Send + Sync>;
/// A Rust hook on a CRUD endpoint (`hooks = { validate = "notes::check" }`).
pub type HookFn = Arc<dyn Fn(Ctx, Row) -> BoxFuture<'static, Result<Row, Problem>> + Send + Sync>;

/// Handlers and hooks registered by name in the project binary.
#[derive(Clone, Default)]
pub struct Handlers {
    pub(crate) handlers: BTreeMap<String, HandlerFn>,
    pub(crate) hooks: BTreeMap<String, HookFn>,
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
}

/// An intent with its handlers linked, ready to install.
pub struct Resolved {
    pub intent: Intent,
    pub file: String,
    handlers: Handlers,
}

/// Tenant scope of one request.
struct Scope {
    tenant: Option<(String, Uuid)>,
}

impl Resolved {
    fn scope(&self, tenant: Option<Uuid>) -> Scope {
        Scope {
            tenant: match (&self.intent.tenancy, tenant) {
                (Some(t), Some(id)) => Some((t.column(), id)),
                _ => None,
            },
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
            if let EndpointSpec::Custom { id, .. } = e
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
        for model in intent.models.iter().filter(|m| m.app == self.id) {
            app.model(model_of(model, intent));
        }
        for endpoint in &intent.endpoints {
            match endpoint {
                EndpointSpec::Crud {
                    model,
                    ops,
                    read,
                    write,
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
                    for op in ops {
                        let permission = if op.writes() { write } else { read };
                        let (built, http) =
                            crud_operation::<S>(&self.resolved, spec, endpoint, *op, permission);
                        app.dyn_operation(built, Some(http));
                    }
                }
                EndpointSpec::Custom {
                    id,
                    model,
                    method,
                    path,
                    permission,
                    handler,
                    input,
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
                        policy: policy_id(permission),
                        permission: permission.clone(),
                        input: contract,
                        output: representation(spec),
                        model: spec.clone(),
                        resolved: self.resolved.clone(),
                        kind: Kind::Handler(handler.name.clone()),
                        _state: PhantomData,
                    };
                    let http = Http::new(method.clone(), path.clone());
                    app.dyn_operation(Arc::new(op), Some(http));
                }
            }
        }
    }
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
            FieldType::Date => Column::date(&f.id),
            FieldType::Instant => Column::timestamptz(&f.id),
            FieldType::Duration => Column::duration(&f.id),
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
            Field::enumeration(&f.id, &values)
        }
        FieldType::Decimal => Field::decimal(&f.id, f.precision, f.scale),
        FieldType::Date => Field::date(&f.id),
        FieldType::Instant => Field::instant(&f.id),
        FieldType::Duration => Field::duration(&f.id),
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
    for f in &model.fields {
        doc = doc.field(field_contract(f, f.required()));
    }
    doc
}

fn update_contract(model: &ModelSpec, versioned: bool) -> ContractDoc {
    let mut doc = ContractDoc::input(&format!("{}.update", model.id), "1").field(Field::uuid("id"));
    if versioned {
        doc = doc.field(Field::digits("expected_version"));
    }
    for f in &model.fields {
        doc = doc.field(field_contract(f, false));
    }
    doc
}

enum Kind {
    Crud(Crud, Vec<String>, (String, bool), BTreeMap<String, String>),
    Handler(String),
}

struct IntentOp<S> {
    info: OperationDescriptor,
    policy: String,
    permission: String,
    input: ContractDoc,
    output: Arc<ContractDoc>,
    model: Arc<ModelSpec>,
    resolved: Arc<Resolved>,
    kind: Kind,
    _state: PhantomData<fn() -> S>,
}

fn crud_operation<S>(
    resolved: &Arc<Resolved>,
    model: &Arc<ModelSpec>,
    endpoint: &EndpointSpec,
    op: Crud,
    permission: &str,
) -> (Arc<dyn DynOperation<S>>, Http)
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    let EndpointSpec::Crud {
        filter,
        order,
        hooks,
        ..
    } = endpoint
    else {
        unreachable!("crud endpoint")
    };
    let collection = model.path();
    let item = format!("{collection}/{{id}}");
    let rep = representation(model);
    let (input, output, http, summary): (ContractDoc, Arc<ContractDoc>, Http, String) =
        match op {
            Crud::List => {
                let mut doc = ContractDoc::input(&format!("{}.list", model.id), "1");
                for name in filter {
                    let f = model.field(name).expect("validated");
                    doc = doc.field(field_contract(f, false));
                }
                doc = doc.field(Field::integer("limit").range(1, MAX_LIMIT).optional());
                let page = ContractDoc::output(&format!("{}.page", model.id), "1")
                    .field(Field::items("items", &rep, MAX_LIMIT as u32));
                (
                    doc,
                    Arc::new(page),
                    Http::get(collection.clone()),
                    format!("List {}.", model.id),
                )
            }
            Crud::Get => (
                ContractDoc::input(&format!("{}.get", model.id), "1").field(Field::uuid("id")),
                rep.clone(),
                Http::get(item.clone()).etag("version"),
                format!("Get one {}.", model.id),
            ),
            Crud::Create => (
                create_contract(model),
                rep.clone(),
                Http::post(collection.clone())
                    .created(item.clone())
                    .etag("version"),
                format!("Create a {}.", model.id),
            ),
            Crud::Update => (
                update_contract(model, true),
                rep.clone(),
                Http::patch(item.clone())
                    .if_match("expected_version")
                    .etag("version"),
                format!("Change a {}.", model.id),
            ),
            Crud::Delete => (
                ContractDoc::input(&format!("{}.delete", model.id), "1")
                    .field(Field::uuid("id"))
                    .field(Field::digits("expected_version")),
                rep.clone(),
                Http::delete(item.clone())
                    .if_match("expected_version")
                    .no_content(),
                format!("Delete a {}.", model.id),
            ),
        };
    // Generated CRUD touches only its model. A hook is Rust code that may
    // do anything, so a write that runs one declares nothing (ADR-018).
    let hooked = match op {
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
    };
    let hooks = hooks
        .iter()
        .map(|(k, v)| (k.clone(), v.name.clone()))
        .collect();
    let mut info = OperationDescriptor::new(
        format!("{}.{}", model.id, op.as_str()),
        "1",
        if op.writes() { Mode::Write } else { Mode::Read },
    )
    .audiences(&[Audience::Product])
    .summary(summary);
    if let Some(effects) = effects {
        info = info.effects(effects);
    }
    let built = IntentOp::<S> {
        info,
        policy: policy_id(permission),
        permission: permission.to_owned(),
        input,
        output,
        model: model.clone(),
        resolved: resolved.clone(),
        kind: Kind::Crud(op, filter.clone(), order.clone(), hooks),
        _state: PhantomData,
    };
    (Arc::new(built), http)
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

impl<S> IntentOp<S> {
    fn permits(&self, role: Option<&str>) -> bool {
        self.permission == AUTHENTICATED
            || role.is_some_and(|r| {
                self.resolved
                    .intent
                    .roles
                    .get(r)
                    .is_some_and(|perms| perms.contains(&self.permission))
            })
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
        &self.policy
    }

    fn context(&self) -> ContextDescriptor {
        let ctx = ContextDescriptor::authenticated();
        if self.resolved.intent.tenancy.is_some() {
            ctx.header(
                "Trestle-Tenant",
                "Tenant to act in. Must be one of the caller's memberships; grants nothing by itself.",
                Some("uuid"),
            )
        } else {
            ctx
        }
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
        Box::pin(async move {
            let actor = invocation.actor()?;
            if self.resolved.intent.tenancy.is_none() {
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
            let role = membership_role(&mut conn, &self.resolved, tenant, actor, false)
                .await?
                .ok_or_else(|| Problem::forbidden("tenant_not_permitted"))?;
            Ok(Arc::new(IntentContext {
                actor,
                tenant: Some(tenant),
                role: Some(role),
            }) as DynContext)
        })
    }

    fn admit(&self, ctx: &DynContext) -> Result<(), Problem> {
        let ctx = ctx_of(ctx)?;
        if self.permits(ctx.role.as_deref()) {
            Ok(())
        } else {
            Err(Problem::forbidden("permission_denied"))
        }
    }

    fn recheck<'a>(
        &'a self,
        conn: &'a mut PgConnection,
        ctx: &'a DynContext,
    ) -> BoxFuture<'a, Result<(), Problem>> {
        Box::pin(async move {
            let ctx = ctx_of(ctx)?;
            let Some(tenant) = ctx.tenant else {
                return Ok(());
            };
            let role = membership_role(conn, &self.resolved, tenant, ctx.actor, true)
                .await?
                .ok_or_else(|| Problem::forbidden("tenant_not_permitted"))?;
            if self.permits(Some(&role)) {
                Ok(())
            } else {
                Err(Problem::forbidden("permission_denied"))
            }
        })
    }

    fn run(
        self: Arc<Self>,
        conn: DynConn,
        ctx: DynContext,
        input: Value,
    ) -> BoxFuture<'static, (DynConn, Result<Value, Problem>)> {
        Box::pin(async move {
            let (actor, tenant) = match ctx_of(&ctx) {
                Ok(c) => (c.actor, c.tenant),
                Err(p) => return (conn, Err(p)),
            };
            let handle = Ctx {
                inner: Arc::new(CtxInner {
                    conn: Mutex::new(Some(conn)),
                    actor,
                    tenant,
                    intent: self.resolved.clone(),
                    writes: self.info.mode == Mode::Write,
                }),
            };
            let result = self.dispatch(&handle, input).await;
            let conn = handle.inner.conn.lock().await.take();
            match conn {
                Some(conn) => (conn, result),
                None => unreachable!("the connection is only taken here"),
            }
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

    async fn dispatch(&self, ctx: &Ctx, input: Value) -> Result<Value, Problem> {
        let model = &self.model;
        let Value::Object(mut input) = input else {
            return Err(Problem::malformed(
                "invalid_body",
                "Expected a JSON object.",
            ));
        };
        match &self.kind {
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
            Kind::Crud(op, filter, order, hooks) => {
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
                        let limit = input.remove("limit").and_then(|v| v.as_i64()).unwrap_or(50);
                        let filter: Map<String, Value> = input
                            .into_iter()
                            .filter(|(k, _)| filter.contains(k))
                            .collect();
                        let rows = ctx
                            .with(async |conn, scope| {
                                list(
                                    conn,
                                    model,
                                    scope,
                                    &filter,
                                    (order.0.as_str(), order.1),
                                    limit,
                                )
                                .await
                            })
                            .await?;
                        Ok(json!({ "items": rows.iter().map(Row::to_json).collect::<Vec<_>>() }))
                    }
                    Crud::Get => {
                        let id = id.ok_or_else(Problem::not_found)?;
                        let row = ctx
                            .with(async |conn, scope| fetch(conn, model, scope, id, false).await)
                            .await?
                            .ok_or_else(Problem::not_found)?;
                        Ok(row.to_json())
                    }
                    Crud::Create => {
                        let mut row = Row::new(model.clone(), input);
                        row = self.hook(ctx, hooks, "validate", row).await?;
                        row = self.hook(ctx, hooks, "before_create", row).await?;
                        let created = ctx
                            .with(async |conn, scope| {
                                insert(conn, model, scope, row.data.clone()).await
                            })
                            .await?;
                        let created = self.hook(ctx, hooks, "after_create", created).await?;
                        Ok(created.to_json())
                    }
                    Crud::Update => {
                        let id = id.ok_or_else(Problem::not_found)?;
                        let current = ctx
                            .with(async |conn, scope| fetch(conn, model, scope, id, true).await)
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
                        let changes: Map<String, Value> = model
                            .fields
                            .iter()
                            .filter_map(|f| {
                                merged.data.get(&f.id).map(|v| (f.id.clone(), v.clone()))
                            })
                            .collect();
                        let updated = ctx
                            .with(async |conn, scope| {
                                update(conn, model, scope, id, changes, version).await
                            })
                            .await?
                            .ok_or_else(Problem::precondition_failed)?;
                        let updated = self.hook(ctx, hooks, "after_update", updated).await?;
                        Ok(updated.to_json())
                    }
                    Crud::Delete => {
                        let id = id.ok_or_else(Problem::not_found)?;
                        let current = ctx
                            .with(async |conn, scope| fetch(conn, model, scope, id, true).await)
                            .await?
                            .ok_or_else(Problem::not_found)?;
                        if version.is_some_and(|v| {
                            Some(v) != current.data.get("version").and_then(Value::as_i64)
                        }) {
                            return Err(Problem::precondition_failed());
                        }
                        self.hook(ctx, hooks, "before_delete", current).await?;
                        let deleted = ctx
                            .with(async |conn, scope| delete(conn, model, scope, id, version).await)
                            .await?
                            .ok_or_else(Problem::precondition_failed)?;
                        Ok(deleted.to_json())
                    }
                }
            }
        }
    }
}

// SQL. Identifiers are validated lowercase names from the intent; values
// are always bound.

fn representation_sql(model: &ModelSpec) -> String {
    let mut parts = vec!["'id', t.id".to_owned()];
    for f in &model.fields {
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
        FieldType::Decimal => ColumnType::Numeric {
            precision: f.precision,
            scale: f.scale,
        },
        FieldType::Date => ColumnType::Date,
        FieldType::Instant => ColumnType::TimestampTz,
        FieldType::Duration => ColumnType::Interval,
        FieldType::Int => ColumnType::BigInt,
        FieldType::Bool => ColumnType::Boolean,
        FieldType::Uuid => ColumnType::Uuid,
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
    let sql = format!(
        "SELECT {} FROM {} t WHERE {tenant_sql}t.id = $1{}",
        representation_sql(model),
        model.table,
        if lock { " FOR UPDATE" } else { "" }
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql).bind(id);
    if let Some(t) = tenant {
        q = q.bind(t);
    }
    let text = q.fetch_optional(conn).await?;
    Ok(parse_rows(model, text.into_iter().collect())?.pop())
}

async fn list(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    filter: &Map<String, Value>,
    order: (&str, bool),
    limit: i64,
) -> Result<Vec<Row>, Problem> {
    let mut conditions = Vec::new();
    if let Some((col, _)) = &scope.tenant {
        conditions.push(format!("t.{col} = $2"));
    }
    for (name, _) in filter.iter() {
        let f = model
            .field(name)
            .ok_or_else(|| internal(&format!("{name:?} is not a field of {}", model.id)))?;
        conditions.push(format!("t.{name} = ($1::jsonb->>'{name}')::{}", f.ty.sql()));
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
        limit.clamp(1, MAX_LIMIT)
    );
    let mut q =
        sqlx::query_scalar::<_, String>(&sql).bind(Value::Object(filter.clone()).to_string());
    if let Some((_, t)) = &scope.tenant {
        q = q.bind(*t);
    }
    parse_rows(model, q.fetch_all(conn).await?)
}

fn writable(model: &ModelSpec, values: &Map<String, Value>) -> Vec<String> {
    model
        .fields
        .iter()
        .filter(|f| values.contains_key(&f.id))
        .map(|f| f.id.clone())
        .collect()
}

async fn insert(
    conn: &mut PgConnection,
    model: &Arc<ModelSpec>,
    scope: &Scope,
    mut values: Map<String, Value>,
) -> Result<Row, Problem> {
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
        .await?;
    Ok(parse_rows(model, text.into_iter().collect())?.pop())
}
