//! App composition and the registry (design doc §3.2, §4, §7.3).
//!
//! Apps register explicitly; nothing is discovered and installing an app
//! publishes only what it registers (I01). Registration is pure: no
//! database, network or tasks (I09). [`Project::assemble`] validates the
//! whole composition and reports every problem at once; the resulting
//! [`Registry`] is immutable and serves routes, exports and inspection.

mod export;
mod meta;
mod resource;
mod routes;
mod sdk;
mod view;
pub use view::{
    PresentationOverride, PublishedView, SectionDefinition, UI_MANIFEST_PATH, UI_VIEW_PATH,
    ViewDefinition,
};

pub use meta::{
    ADMIN_INTERACTION_PATH, ADMIN_MANIFEST_PATH, Availability, INTERACTION_PATH, MANIFEST_PATH,
};
pub use resource::{
    AdminAction, AdminView, EXPECTED_VERSION_FIELD, RegisteredResource, ResourceOp,
    ResourceRegistration, VERSION_FIELD,
};
pub use routes::Http;

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::extract::FromRef;
use axum::routing::MethodRouter;
use http::Method;
use serde_json::Value;
use sqlx::PgPool;
use trestle_historical::schema::{self as persistence, Model, SchemaState};
use trestle_historical::{DataStep, MigrationRef};
use trestle_protocol::interaction::{HttpBinding, OperationRef, form_interaction};
use trestle_protocol::{
    Audience, Contract, ContractDoc, ContractKind, FieldType, FormPresentation, limits,
    valid_stable_id,
};

use crate::Problem;
use crate::health::{self, LIVE_PATH, READY_PATH};
use crate::identity::Identity;
use crate::invocation::{FromInvocation, Invocation};
use crate::jobs::JobHandler;
use crate::operation::{
    BoxFuture, ContextDescriptor, DynOperation, Operation, OperationDescriptor, Policy,
    RequestContext, execute_dyn_keyed, execute_keyed, invoke, invoke_dyn, validate,
};

/// A cohesive feature: lessons, schools, billing. Dyn-compatible so a
/// project can hold heterogeneous apps.
pub trait App<S>: Send + Sync + 'static {
    /// Stable lowercase identifier; also the namespace of everything the
    /// app registers (`lessons` owns `lessons.create`, `lessons.view`, …).
    fn id(&self) -> &str;

    fn register(&self, app: &mut AppBuilder<S>);
}

/// A boxed app is an app, so a project can hold apps before assembly.
impl<S: 'static> App<S> for Box<dyn App<S>> {
    fn id(&self) -> &str {
        (**self).id()
    }

    fn register(&self, app: &mut AppBuilder<S>) {
        (**self).register(app);
    }
}

pub struct AppBuilder<S> {
    app: Arc<str>,
    requires: Vec<Arc<str>>,
    schema: Vec<MigrationRef>,
    models: Vec<Model>,
    data_steps: Vec<(&'static str, DataStep)>,
    operations: Vec<RegisteredOperation<S>>,
    forms: Vec<RegisteredForm>,
    resources: Vec<RegisteredResource<S>>,
    views: Vec<view::ViewRegistration<S>>,
    view_overrides: Vec<PresentationOverride>,
    unmanaged: Vec<UnmanagedRoute<S>>,
    jobs: Vec<RegisteredJob<S>>,
}

/// A handler for jobs of one kind (ADR-015).
pub struct RegisteredJob<S> {
    pub app: Arc<str>,
    pub kind: Arc<str>,
    handler: Arc<dyn JobHandler<S>>,
}

impl<S> AppBuilder<S> {
    fn new(app: &str) -> Self {
        Self {
            app: Arc::from(app),
            requires: Vec::new(),
            schema: Vec::new(),
            models: Vec::new(),
            data_steps: Vec::new(),
            operations: Vec::new(),
            forms: Vec::new(),
            resources: Vec::new(),
            views: Vec::new(),
            view_overrides: Vec::new(),
            unmanaged: Vec::new(),
            jobs: Vec::new(),
        }
    }

    /// Registers the handler for jobs of `kind` (`app.kind`), enqueued with
    /// [`crate::jobs::NewJob::handler`]. Registration is pure: nothing runs
    /// until a worker claims a job.
    pub fn job(&mut self, kind: impl Into<Arc<str>>, handler: impl JobHandler<S>) -> &mut Self {
        self.jobs.push(RegisteredJob {
            app: self.app.clone(),
            kind: kind.into(),
            handler: Arc::new(handler),
        });
        self
    }

    /// Starts a resource at `path` (e.g. `/api/v1/lessons`). Verbs are
    /// opt-in; see [`ResourceRegistration`].
    pub fn resource(
        &mut self,
        id: impl Into<Arc<str>>,
        path: impl Into<Arc<str>>,
    ) -> ResourceRegistration<'_, S> {
        self.resources.push(RegisteredResource {
            app: self.app.clone(),
            id: id.into(),
            path: path.into(),
            list: None,
            create: None,
            retrieve: None,
            update: None,
            delete: None,
            actions: Vec::new(),
            admin: None,
        });
        let index = self.resources.len() - 1;
        ResourceRegistration {
            builder: self,
            index,
            idempotency_key: false,
        }
    }

    /// Declares a lifecycle dependency. Using another app's contracts,
    /// operations or forms requires one.
    pub fn requires(&mut self, app: impl Into<Arc<str>>) -> &mut Self {
        self.requires.push(app.into());
        self
    }

    /// A migration that must be applied before this app admits requests.
    pub fn schema(&mut self, migration: MigrationRef) -> &mut Self {
        self.schema.push(migration);
        self
    }

    /// Registers a migration data step by the name manifests use
    /// (`"data": "<name>"`). `step` is the step's own constructor, whose
    /// `source` is frozen into the migration digest. Loading fails if a
    /// manifest names a step nobody registered, or a step is never used.
    pub fn data_step(&mut self, name: &'static str, step: fn() -> DataStep) -> &mut Self {
        self.data_steps.push((name, step()));
        self
    }

    /// Declares a persisted model (desired state, §13). Installing a model
    /// publishes nothing (I01); it only feeds `makemigrations` and `verify`.
    pub fn model(&mut self, model: Model) -> &mut Self {
        self.models.push(model);
        self
    }

    /// Registers an operation. It is invocable ([`Registry::invoke`]) and
    /// inspectable; it has no HTTP route unless [`OperationRegistration::http`]
    /// is called.
    pub fn operation<O: Operation>(&mut self, op: O) -> OperationRegistration<'_, S, O>
    where
        S: Send + Sync + 'static,
        O::Context: FromInvocation<S>,
        PgPool: FromRef<S>,
    {
        let op = Arc::new(op);
        let invoker = {
            let op = op.clone();
            erase_invoker(move |state: &S, invocation, input| {
                let op = op.clone();
                Box::pin(async move {
                    let Some(key) = invocation.idempotency_key() else {
                        let output = invoke(state, &*op, invocation, input).await?;
                        return serde_json::to_value(output).map_err(|err| {
                            tracing::error!(operation = O::INFO.id, error = %err, "output does not serialize");
                            Problem::internal()
                        });
                    };
                    let ctx = O::Context::resolve(invocation, state).await?;
                    invocation.check_operation_version(O::INFO.version)?;
                    let input = validate::<O::Input>(input)?;
                    let keyed =
                        execute_keyed(&PgPool::from_ref(state), &*op, &ctx, input, invocation, key)
                            .await?;
                    from_body(O::INFO.id, &keyed.body)
                })
            })
        };
        let entry = RegisteredOperation {
            app: self.app.clone(),
            info: match O::EFFECTS {
                Some(effects) => OperationDescriptor::from(O::INFO).effects(effects.into()),
                None => O::INFO.into(),
            },
            policy: Arc::from(op.policy().id()),
            context: O::Context::describe(),
            input: Arc::new(O::Input::contract().clone()),
            output: Arc::new(O::Output::contract().clone()),
            binding: None,
            route: None,
            availability: None,
            accepted_revisions: Arc::new(OnceLock::new()),
            conformance: conformance::<O>(),
            invoker,
        };
        self.operations.push(entry);
        let index = self.operations.len() - 1;
        OperationRegistration {
            builder: self,
            op,
            index,
        }
    }

    /// Registers an operation assembled at runtime (see [`DynOperation`]),
    /// optionally exposed through a managed route.
    pub fn dyn_operation(&mut self, op: Arc<dyn DynOperation<S>>, http: Option<Http>) -> &mut Self
    where
        S: Clone + Send + Sync + 'static,
        PgPool: FromRef<S>,
        Identity: FromRef<S>,
    {
        let context = op.context();
        let invoker = {
            let op = op.clone();
            erase_invoker(move |state: &S, invocation, input| {
                let op = op.clone();
                Box::pin(async move {
                    let Some(key) = invocation.idempotency_key() else {
                        return invoke_dyn(state, op, invocation, input).await;
                    };
                    let ctx = op.resolve_context(invocation, state).await?;
                    invocation.check_operation_version(&op.info().version)?;
                    let id = op.info().id.clone();
                    let keyed = execute_dyn_keyed(
                        &PgPool::from_ref(state),
                        op,
                        ctx,
                        input,
                        invocation,
                        key,
                    )
                    .await?;
                    from_body(&id, &keyed.body)
                })
            })
        };
        let mut entry = RegisteredOperation {
            app: self.app.clone(),
            info: op.info().clone(),
            policy: Arc::from(op.policy_id()),
            context,
            input: Arc::new(op.input().clone()),
            output: Arc::new(op.output().clone()),
            binding: None,
            route: None,
            availability: None,
            accepted_revisions: Arc::new(OnceLock::new()),
            conformance: Vec::new(),
            invoker,
        };
        if let Some(http) = http {
            let binding = http.binding(&entry.context, &entry.input);
            entry.availability = Some(meta::dyn_availability::<S>(op.clone()));
            entry.route = Some(routes::managed_dyn::<S>(
                op,
                &http,
                binding.clone(),
                entry.accepted_revisions.clone(),
            ));
            entry.binding = Some(binding);
        }
        self.operations.push(entry);
        self
    }

    /// Publishes a form for an operation. Its revision is derived from the
    /// descriptor; see [`FormRegistration::retain`] for open-form skew.
    pub fn form(&mut self, form: FormPresentation) -> FormRegistration<'_> {
        self.forms.push(RegisteredForm {
            form,
            retained: Vec::new(),
        });
        FormRegistration {
            form: self.forms.last_mut().expect("just pushed"),
        }
    }

    /// Mounts an ordinary Axum route that the registry does not manage. It
    /// gains no contract, export or authorization from being mounted; the
    /// waiver says why that is acceptable and is shown in route inventory.
    pub fn unmanaged(
        &mut self,
        method: Method,
        path: &'static str,
        route: MethodRouter<S>,
        waiver: &'static str,
    ) -> &mut Self {
        self.unmanaged.push(UnmanagedRoute {
            app: self.app.clone(),
            method,
            path,
            route,
            waiver,
        });
        self
    }
}

pub struct FormRegistration<'a> {
    form: &'a mut RegisteredForm,
}

impl FormRegistration<'_> {
    /// Keeps accepting submissions from an earlier revision of this form
    /// (§11.4). Only retain a revision whose submissions mean the same thing
    /// under the current operation; the pilot's snapshot tests verify the
    /// current input contract still accepts everything that revision could
    /// send. Unretained old revisions get 409 `interaction_stale`.
    pub fn retain(self, revision: &'static str, reason: &'static str) -> Self {
        self.form
            .retained
            .push(RetainedRevision { revision, reason });
        self
    }
}

#[derive(Debug, Clone)]
pub struct RegisteredForm {
    pub form: FormPresentation,
    pub retained: Vec<RetainedRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedRevision {
    pub revision: &'static str,
    pub reason: &'static str,
}

/// A form after assembly: its descriptor and revision are fixed.
#[derive(Debug, Clone)]
pub struct PublishedForm {
    pub form: FormPresentation,
    pub retained: Vec<RetainedRevision>,
    pub revision: String,
    pub descriptor: Value,
}

pub struct OperationRegistration<'a, S, O> {
    builder: &'a mut AppBuilder<S>,
    op: Arc<O>,
    index: usize,
}

impl<S, O> OperationRegistration<'_, S, O>
where
    S: Clone + Send + Sync + 'static,
    O: Operation,
    O::Context: FromInvocation<S>,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    /// Exposes the operation through a managed route.
    pub fn http(self, http: Http) {
        let entry = &mut self.builder.operations[self.index];
        let binding = http.binding(&entry.context, &entry.input);
        entry.availability = Some(meta::availability::<S, O>(self.op.clone()));
        entry.route = Some(routes::managed::<S, O>(
            self.op,
            &http,
            binding.clone(),
            entry.accepted_revisions.clone(),
        ));
        entry.binding = Some(binding);
    }
}

/// Proves the Rust input type and its contract agree: the contract's example
/// instance must deserialize into the type (caught at assembly, not at the
/// first request).
fn conformance<O: Operation>() -> Vec<String> {
    let contract = O::Input::contract();
    match contract.validate(&contract.example()) {
        Ok(normalized) => match serde_json::from_value::<O::Input>(normalized) {
            Ok(_) => Vec::new(),
            Err(err) => vec![format!(
                "operation {}: input type does not match contract {}: {err}",
                O::INFO.id,
                contract.id
            )],
        },
        // Definition problems are reported by the contract check.
        Err(_) => Vec::new(),
    }
}

pub struct RegisteredOperation<S> {
    pub app: Arc<str>,
    pub info: OperationDescriptor,
    pub policy: Arc<str>,
    pub context: ContextDescriptor,
    pub input: Arc<ContractDoc>,
    pub output: Arc<ContractDoc>,
    pub binding: Option<HttpBinding>,
    route: Option<MethodRouter<S>>,
    availability: Option<meta::AvailabilityFn<S>>,
    /// Interaction revisions this operation's managed route accepts in
    /// `Trestle-Interaction-Revision`. Filled once at assembly.
    accepted_revisions: Arc<OnceLock<Vec<String>>>,
    conformance: Vec<String>,
    invoker: InvokeFn<S>,
}

/// Resolves the context and runs the executor for one registered operation,
/// with JSON in and out.
type InvokeFn<S> = Arc<
    dyn for<'a> Fn(&'a S, &'a Invocation, &'a Value) -> BoxFuture<'a, Result<Value, Problem>>
        + Send
        + Sync,
>;

/// A stored or fresh keyed answer, as JSON.
fn from_body(operation: &str, body: &str) -> Result<Value, Problem> {
    serde_json::from_str(body).map_err(|err| {
        tracing::error!(operation, error = %err, "stored answer is not JSON");
        Problem::internal()
    })
}

fn erase_invoker<S, F>(f: F) -> InvokeFn<S>
where
    F: for<'a> Fn(&'a S, &'a Invocation, &'a Value) -> BoxFuture<'a, Result<Value, Problem>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

impl<S> RegisteredOperation<S> {
    /// Whether its binding accepts an `Idempotency-Key` (ADR-016).
    pub fn accepts_idempotency_key(&self) -> bool {
        self.binding.as_ref().is_some_and(|b| b.idempotency_key)
    }

    pub fn accepted_revisions(&self) -> &[String] {
        self.accepted_revisions
            .get()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

pub struct UnmanagedRoute<S> {
    pub app: Arc<str>,
    pub method: Method,
    pub path: &'static str,
    pub waiver: &'static str,
    route: MethodRouter<S>,
}

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub id: Arc<str>,
    pub requires: Vec<Arc<str>>,
    pub operations: Vec<Arc<str>>,
    pub forms: Vec<String>,
    pub models: Vec<String>,
    pub schema: Vec<MigrationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteKind {
    /// Framework health endpoints.
    System,
    Managed {
        operation: Arc<str>,
    },
    Unmanaged {
        app: Arc<str>,
        waiver: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInfo {
    pub method: String,
    pub path: String,
    pub kind: RouteKind,
}

#[derive(Debug)]
pub struct AssemblyError {
    pub issues: Vec<String>,
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "registry assembly failed with {} issue(s):",
            self.issues.len()
        )?;
        for issue in &self.issues {
            writeln!(f, "  - {issue}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AssemblyError {}

pub struct Project<S> {
    name: Arc<str>,
    version: Arc<str>,
    apps: Vec<Box<dyn App<S>>>,
}

impl<S: 'static> Project<S> {
    pub fn new(name: impl Into<Arc<str>>, version: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            apps: Vec::new(),
        }
    }

    pub fn install(mut self, app: impl App<S>) -> Self {
        self.apps.push(Box::new(app));
        self
    }

    /// Registers every app and validates the composition. Pure and offline.
    pub fn assemble(self) -> Result<Registry<S>, AssemblyError> {
        let mut issues = Vec::new();
        let mut builders: Vec<AppBuilder<S>> = Vec::new();
        for app in &self.apps {
            let id = app.id();
            if !valid_app_id(id) {
                issues.push(format!("app id {id:?} is not a lowercase identifier"));
            }
            if builders.iter().any(|b| &*b.app == id) {
                issues.push(format!("app {id} is installed twice"));
                continue;
            }
            let mut builder = AppBuilder::new(id);
            app.register(&mut builder);
            builders.push(builder);
        }

        let order = lifecycle_order(&builders, &mut issues);
        check_operations(&builders, &mut issues);
        check_forms(&builders, &mut issues);
        check_routes(&builders, &mut issues);
        let published = publish_forms(&builders, &mut issues);
        let desired = check_models(&builders, &mut issues);
        let keyed: Vec<&str> = builders
            .iter()
            .flat_map(|b| &b.operations)
            .filter(|op| op.accepts_idempotency_key())
            .map(|op| &*op.info.id)
            .collect();
        if !keyed.is_empty()
            && desired
                .as_ref()
                .is_some_and(|d| d.model("idempotency.receipt").is_none())
        {
            issues.push(format!(
                "{} accept Idempotency-Key, which needs trestle_core::idempotency::IdempotencyApp installed",
                keyed.join(", ")
            ));
        }
        check_resources(&builders, &mut issues);
        for builder in &builders {
            for view in &builder.views {
                let definition = &view.definition;
                if !valid_stable_id(&definition.id) || owner(&definition.id) != &*builder.app {
                    issues.push(format!(
                        "view {} must be namespaced to app {}",
                        definition.id, builder.app
                    ));
                }
                if !visible(builder, owner(&definition.query)) {
                    issues.push(format!(
                        "view {} uses an operation from an app it does not require",
                        definition.id
                    ));
                }
            }
        }
        check_jobs(&builders, &mut issues);
        check_effects(&builders, desired.as_ref(), &mut issues);

        let mut step_names = HashSet::new();
        for builder in &builders {
            for (name, step) in &builder.data_steps {
                if step.name != *name {
                    issues.push(format!(
                        "app {} registers data step {name:?}, but the step calls itself {:?}",
                        builder.app, step.name
                    ));
                }
                if !step_names.insert(*name) {
                    issues.push(format!("data step {name:?} is registered twice"));
                }
            }
        }
        if !issues.is_empty() {
            return Err(AssemblyError { issues });
        }

        // Freeze the revisions each managed route accepts: every current
        // revision of a form bound to it, plus explicitly retained ones.
        for op in builders.iter().flat_map(|b| &b.operations) {
            let accepted: Vec<String> = published
                .iter()
                .flatten()
                .filter(|p| *p.form.operation == *op.info.id)
                .flat_map(|p| {
                    std::iter::once(p.revision.clone())
                        .chain(p.retained.iter().map(|r| r.revision.to_owned()))
                })
                .collect();
            let _ = op.accepted_revisions.set(accepted);
        }
        let mut published: Vec<Vec<PublishedForm>> = published;

        let backfills: BTreeMap<String, serde_json::Value> = builders
            .iter()
            .flat_map(|b| &b.models)
            .flat_map(Model::backfills)
            .collect();
        let data_steps: Vec<DataStep> = builders
            .iter()
            .flat_map(|b| b.data_steps.iter().map(|(_, step)| *step))
            .collect();
        let mut slots: Vec<Option<AppBuilder<S>>> = builders.into_iter().map(Some).collect();
        let mut registry = Registry {
            name: self.name,
            version: self.version,
            desired: desired.unwrap_or_default(),
            backfills,
            data_steps,
            apps: Vec::new(),
            operations: Vec::new(),
            forms: Vec::new(),
            resources: Vec::new(),
            views: Vec::new(),
            unmanaged: Vec::new(),
            schema: Vec::new(),
            jobs: Vec::new(),
        };
        let mut view_registrations = Vec::new();
        let mut view_overrides = Vec::new();
        for index in order {
            let builder = slots[index].take().expect("each app appears once in order");
            registry.apps.push(AppInfo {
                id: builder.app,
                requires: builder.requires.clone(),
                operations: builder
                    .operations
                    .iter()
                    .map(|o| o.info.id.clone())
                    .collect(),
                forms: builder.forms.iter().map(|f| f.form.id.clone()).collect(),
                models: builder.models.iter().map(|m| m.id().to_owned()).collect(),
                schema: builder.schema.clone(),
            });
            for migration in builder.schema {
                if !registry.schema.contains(&migration) {
                    registry.schema.push(migration);
                }
            }
            registry.operations.extend(builder.operations);
            registry.forms.append(&mut published[index]);
            registry.resources.extend(builder.resources);
            view_registrations.extend(builder.views);
            view_overrides.extend(builder.view_overrides);
            registry.unmanaged.extend(builder.unmanaged);
            registry.jobs.extend(builder.jobs);
        }
        registry
            .assemble_views(view_registrations, view_overrides)
            .map_err(|issues| AssemblyError { issues })?;
        Ok(registry)
    }
}

fn valid_app_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn owner(id: &str) -> &str {
    id.split('.').next().unwrap_or(id)
}

/// Whether `builder` may use something namespaced to `owner`.
fn visible<S>(builder: &AppBuilder<S>, owner: &str) -> bool {
    &*builder.app == owner || builder.requires.iter().any(|r| &**r == owner)
}

/// Dependency order (install order breaks ties). Reports missing
/// dependencies and cycles.
fn lifecycle_order<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) -> Vec<usize> {
    for builder in builders {
        for required in &builder.requires {
            if *required == builder.app {
                issues.push(format!("app {} requires itself", builder.app));
            } else if !builders.iter().any(|b| b.app == *required) {
                issues.push(format!(
                    "app {} requires {required}, which is not installed",
                    builder.app
                ));
            }
        }
    }
    let mut placed: Vec<usize> = Vec::new();
    while placed.len() < builders.len() {
        let next = (0..builders.len()).find(|&i| {
            !placed.contains(&i)
                && builders[i].requires.iter().all(|r| {
                    *r == builders[i].app
                        || builders
                            .iter()
                            .position(|b| b.app == *r)
                            .is_none_or(|j| placed.contains(&j))
                })
        });
        match next {
            Some(index) => placed.push(index),
            None => {
                let stuck: Vec<&str> = (0..builders.len())
                    .filter(|i| !placed.contains(i))
                    .map(|i| &*builders[i].app)
                    .collect();
                issues.push(format!("dependency cycle among apps: {}", stuck.join(", ")));
                break;
            }
        }
    }
    placed
}

fn check_operations<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) {
    let mut ids = HashSet::new();
    let mut contracts: BTreeMap<&str, &ContractDoc> = BTreeMap::new();
    for builder in builders {
        for op in &builder.operations {
            let id: &str = &op.info.id;
            if !valid_stable_id(id) {
                issues.push(format!(
                    "operation id {id:?} is not a dotted lowercase stable ID"
                ));
            } else if owner(id) != &*builder.app {
                issues.push(format!(
                    "operation {id} is registered by app {} but namespaced to {}",
                    builder.app,
                    owner(id)
                ));
            }
            if !ids.insert(id) {
                issues.push(format!("operation {id} is registered twice"));
            }
            if op.info.version.is_empty() {
                issues.push(format!("operation {id} has no version"));
            }
            if op.policy.is_empty() {
                issues.push(format!(
                    "operation {id} is bound to a policy with an empty ID"
                ));
            }
            for (contract, kind) in [
                (&*op.input, ContractKind::Input),
                (&*op.output, ContractKind::Output),
            ] {
                if contract.kind != kind {
                    issues.push(format!(
                        "operation {id}: contract {} is an {} contract, used as {}",
                        contract.id,
                        contract.kind.as_str(),
                        kind.as_str()
                    ));
                }
                if !visible(builder, owner(&contract.id)) {
                    issues.push(format!(
                        "operation {id} uses contract {} owned by {}, which app {} does not require",
                        contract.id,
                        owner(&contract.id),
                        builder.app
                    ));
                }
                match contracts.get(contract.id.as_str()) {
                    Some(existing) if *existing != contract => issues.push(format!(
                        "contract {} has two different definitions",
                        contract.id
                    )),
                    Some(_) => {}
                    None => {
                        issues.extend(contract.problems());
                        contracts.insert(&contract.id, contract);
                    }
                }
            }
            issues.extend(op.conformance.iter().cloned());
            if op.accepts_idempotency_key() && op.info.mode != crate::operation::Mode::Write {
                issues.push(format!(
                    "operation {id} accepts Idempotency-Key, but only writes can"
                ));
            }
            if let Some(binding) = &op.binding {
                check_binding(id, &op.input, &op.output, binding, issues);
                if let Some(target) = &binding.precondition_target {
                    let source = builders
                        .iter()
                        .flat_map(|b| &b.operations)
                        .find(|o| o.info.id.as_ref() == target.operation);
                    let valid = source.is_some_and(|source| {
                        source.info.mode == crate::operation::Mode::Read
                            && source.output.id == target.representation
                            && op
                                .info
                                .audiences
                                .iter()
                                .all(|a| source.info.audiences.contains(a))
                            && source.binding.as_ref().is_some_and(|b| {
                                b.method == "GET"
                                    && b.etag.is_some()
                                    && b.path_params() == [target.parameter.as_str()]
                            })
                            && source
                                .input
                                .fields
                                .iter()
                                .all(|f| !f.required || f.name == target.parameter)
                            && source
                                .input
                                .get(&target.parameter)
                                .zip(op.input.get(&target.input_field))
                                .is_some_and(|(a, b)| a.ty == b.ty)
                    });
                    if !valid || binding.if_match.is_none() {
                        issues.push(format!(
                            "operation {id}: invalid precondition target {}",
                            target.operation
                        ));
                    }
                }
            }
        }
    }
}

fn check_binding(
    id: &str,
    input: &ContractDoc,
    output: &ContractDoc,
    binding: &HttpBinding,
    issues: &mut Vec<String>,
) {
    let path = &binding.path;
    if !valid_path(path) {
        issues.push(format!(
            "operation {id}: {path:?} is not a valid route path"
        ));
    }
    for param in binding.path_params() {
        match input.get(param) {
            None => issues.push(format!(
                "operation {id}: path parameter {{{param}}} is not a field of {}",
                input.id
            )),
            Some(field) if !field.required => issues.push(format!(
                "operation {id}: path parameter {{{param}}} must be a required input field"
            )),
            Some(_) => {}
        }
    }
    if let Some(field) = &binding.if_match {
        match input.get(field) {
            Some(f) if f.required && f.ty == FieldType::Digits => {}
            _ => issues.push(format!(
                "operation {id}: If-Match fills {field}, which must be a required digits field of {}",
                input.id
            )),
        }
    }
    if let Some(field) = &binding.etag
        && output.get(field).is_none()
    {
        issues.push(format!(
            "operation {id}: ETag uses {field}, which is not a field of {}",
            output.id
        ));
    }
    if let Some(location) = &binding.location {
        for field in HttpBinding::template_params(location) {
            if output.get(field).is_none() {
                issues.push(format!(
                    "operation {id}: Location {location} uses {{{field}}}, which is not a field of {}",
                    output.id
                ));
            }
        }
    }
}

/// A list operation's item contract, if its output is a page of items.
fn page_item(output: &ContractDoc) -> Option<&ContractDoc> {
    match &output.get("items")?.ty {
        FieldType::Items { item, .. } => Some(item),
        _ => None,
    }
}

fn valid_path(path: &str) -> bool {
    path.starts_with('/')
        && (path == "/" || !path.ends_with('/'))
        && !path.contains("//")
        && path.split('/').skip(1).all(|segment| {
            let name = segment
                .strip_prefix('{')
                .and_then(|s| s.strip_suffix('}'))
                .unwrap_or(segment);
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        })
}

fn check_forms<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) {
    let mut ids = HashSet::new();
    for builder in builders {
        for RegisteredForm { form, .. } in &builder.forms {
            let id = &form.id;
            if valid_stable_id(id) && owner(id) != &*builder.app {
                issues.push(format!(
                    "form {id} is registered by app {} but namespaced to {}",
                    builder.app,
                    owner(id)
                ));
            }
            if !ids.insert(id.as_str()) {
                issues.push(format!("form {id} is registered twice"));
            }
            let target = builders
                .iter()
                .flat_map(|b| &b.operations)
                .find(|op| *op.info.id == *form.operation);
            let Some(op) = target else {
                issues.push(format!(
                    "form {id} is bound to unknown operation {}",
                    form.operation
                ));
                continue;
            };
            if !visible(builder, owner(&op.info.id)) {
                issues.push(format!(
                    "form {id} uses operation {} owned by {}, which app {} does not require",
                    op.info.id,
                    owner(&op.info.id),
                    builder.app
                ));
            }
            if op.binding.is_none() {
                issues.push(format!(
                    "form {id}: operation {} has no HTTP binding to submit to",
                    op.info.id
                ));
            }
            if !op.info.audiences.contains(&form.audience) {
                issues.push(format!(
                    "form {id} is published for {} but operation {} is not",
                    form.audience.as_str(),
                    op.info.id,
                ));
            }
            let mut bound: Vec<&str> = Vec::new();
            if let Some(binding) = &op.binding {
                bound.extend(binding.path_params());
                bound.extend(binding.if_match.as_deref());
            }
            issues.extend(form.problems(&op.input, &bound));
            for source in form.fields.iter().filter_map(|f| f.source.as_ref()) {
                let list = builders
                    .iter()
                    .flat_map(|b| &b.operations)
                    .find(|o| *o.info.id == *source.operation);
                let valid = list.is_some_and(|list| {
                    list.binding.as_ref().is_some_and(|b| b.method == "GET")
                        && page_item(&list.output).is_some_and(|item| {
                            item.get("id").is_some_and(|f| f.ty == FieldType::Uuid)
                                && item.get(&source.label).is_some()
                        })
                });
                if let Some(lookup) = &source.lookup {
                    let resolve = builders
                        .iter()
                        .flat_map(|b| &b.operations)
                        .find(|o| o.info.id.as_ref() == lookup.resolve_operation);
                    let search_valid = list.is_some_and(|o| {
                        o.info.mode == crate::operation::Mode::Read
                            && o.input.get(&lookup.search_field).is_some_and(|f| {
                                matches!(
                                    f.ty,
                                    FieldType::Text {
                                        max_chars: Some(_),
                                        ..
                                    }
                                )
                            })
                            && o.input.get("cursor").is_some()
                            && o.input.get("limit").is_some()
                    });
                    let resolve_valid = resolve.is_some_and(|o| {
                        o.info.mode == crate::operation::Mode::Read
                            && o.binding
                                .as_ref()
                                .is_some_and(|b| b.method == "GET" && b.path_params() == ["id"])
                            && o.output.get("id").is_some_and(|f| f.ty == FieldType::Uuid)
                            && o.output.get(&source.label).is_some()
                            && visible(builder, owner(&o.info.id))
                    });
                    if !search_valid || !resolve_valid {
                        issues.push(format!(
                            "form {id}: invalid relation search/resolve binding"
                        ));
                    }
                }
                if !valid {
                    issues.push(format!(
                        "form {id}: relation options come from {}, which must be a GET list operation whose items have id and {}",
                        source.operation, source.label
                    ));
                } else if !visible(builder, owner(&source.operation)) {
                    issues.push(format!(
                        "form {id}: relation source {} is owned by {}, which app {} does not require",
                        source.operation,
                        owner(&source.operation),
                        builder.app
                    ));
                }
            }
        }
    }
}

/// Desired persistence state from every app's models, with ownership and
/// cross-app reference checks.
fn check_models<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) -> Option<SchemaState> {
    for builder in builders {
        for model in &builder.models {
            if owner(model.id()) != &*builder.app {
                issues.push(format!(
                    "model {} is registered by app {} but namespaced to {}",
                    model.id(),
                    builder.app,
                    owner(model.id())
                ));
            }
            for field in model.state().fields {
                if let Some(target) = &field.references
                    && !visible(builder, owner(target))
                {
                    issues.push(format!(
                        "{} references {target}, owned by {}, which app {} does not require",
                        field.id,
                        owner(target),
                        builder.app
                    ));
                }
            }
        }
    }
    let models: Vec<Model> = builders.iter().flat_map(|b| b.models.clone()).collect();
    match persistence::desired(&models) {
        Ok(state) => Some(state),
        Err(problems) => {
            issues.extend(problems);
            None
        }
    }
}

/// Builds each form's descriptor and revision, per app in install order, and
/// checks size limits and retained revisions.
fn publish_forms<S>(
    builders: &[AppBuilder<S>],
    issues: &mut Vec<String>,
) -> Vec<Vec<PublishedForm>> {
    let operations: Vec<&RegisteredOperation<S>> =
        builders.iter().flat_map(|b| &b.operations).collect();
    let sources: BTreeMap<String, String> = operations
        .iter()
        .filter_map(|o| Some((o.info.id.to_string(), o.binding.as_ref()?.path.clone())))
        .collect();
    builders
        .iter()
        .map(|builder| {
            builder
                .forms
                .iter()
                .filter_map(|registered| {
                    let form = &registered.form;
                    let op = operations.iter().find(|o| *o.info.id == *form.operation)?;
                    let binding = op.binding.as_ref()?;
                    let mut descriptor = form_interaction(
                        form,
                        OperationRef {
                            id: &op.info.id,
                            version: &op.info.version,
                        },
                        binding,
                        &op.input,
                        &op.output,
                        &sources,
                    );
                    // New lookup editors carry the actual resolved read contracts and
                    // bindings. Leave every legacy descriptor byte unchanged.
                    fn bind_lookups<S>(
                        node: &mut Value,
                        operations: &[&RegisteredOperation<S>],
                    ) -> bool {
                        let mut changed = false;
                        if let Some(options) = node.get_mut("options")
                            && options.get("lookup").is_some()
                        {
                            let search = options["operation"].as_str().unwrap_or_default();
                            let resolve = options["lookup"]["resolveOperation"]
                                .as_str()
                                .unwrap_or_default();
                            let bindings: Vec<Value> = [search, resolve]
                                .iter()
                                .map(|id| {
                                    operations
                                        .iter()
                                        .find(|o| o.info.id.as_ref() == *id)
                                        .and_then(|o| view::invocation(o, None).ok())
                                        .and_then(|doc| serde_json::to_value(doc).ok())
                                        .unwrap_or(Value::Null)
                                })
                                .collect();
                            options["lookup"]["searchBinding"] = bindings[0].clone();
                            options["lookup"]["resolveBinding"] = bindings[1].clone();
                            changed = true;
                        }
                        if let Some(children) =
                            node.get_mut("children").and_then(Value::as_array_mut)
                        {
                            for child in children {
                                changed |= bind_lookups(child, operations);
                            }
                        }
                        changed
                    }
                    if bind_lookups(&mut descriptor["presentation"]["layout"], &operations) {
                        descriptor.as_object_mut().unwrap().remove("revision");
                        let hash = trestle_protocol::digest(&descriptor);
                        descriptor["revision"] =
                            serde_json::json!(format!("r-{}", &hash["sha256:".len()..][..16]));
                    }
                    let size = descriptor.to_string().len();
                    if size > limits::DESCRIPTOR_BYTES {
                        issues.push(format!(
                            "form {}: descriptor is {size} bytes; the limit is {}",
                            form.id,
                            limits::DESCRIPTOR_BYTES
                        ));
                    }
                    let revision = descriptor["revision"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    for retained in &registered.retained {
                        if !valid_revision(retained.revision) {
                            issues.push(format!(
                                "form {}: retained revision {:?} is not of the form r-<16 hex>",
                                form.id, retained.revision
                            ));
                        } else if retained.revision == revision {
                            issues.push(format!(
                                "form {}: retained revision {} is the current revision",
                                form.id, retained.revision
                            ));
                        }
                        if retained.reason.trim().is_empty() {
                            issues.push(format!(
                                "form {}: retained revision {} needs a reason",
                                form.id, retained.revision
                            ));
                        }
                    }
                    Some(PublishedForm {
                        form: form.clone(),
                        retained: registered.retained.clone(),
                        revision,
                        descriptor,
                    })
                })
                .collect()
        })
        .collect()
}

fn valid_revision(revision: &str) -> bool {
    revision
        .strip_prefix("r-")
        .is_some_and(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn check_resources<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) {
    let operations: Vec<&RegisteredOperation<S>> =
        builders.iter().flat_map(|b| &b.operations).collect();
    let op = |id: &str| operations.iter().find(|o| &*o.info.id == id).copied();
    let forms: Vec<&RegisteredForm> = builders.iter().flat_map(|b| &b.forms).collect();
    let mut ids = HashSet::new();
    for builder in builders {
        for resource in &builder.resources {
            let id: &str = &resource.id;
            if !valid_stable_id(id) || owner(id) != &*builder.app {
                issues.push(format!(
                    "resource {id:?} must be a stable ID namespaced to app {}",
                    builder.app
                ));
            }
            if !ids.insert(id) {
                issues.push(format!("resource {id} is registered twice"));
            }
            if !valid_path(&resource.path) {
                issues.push(format!(
                    "resource {id}: {:?} is not a valid route path",
                    resource.path
                ));
            }
            // `If-Match` compares against the operation's output, so every
            // versioned verb must speak the retrieve representation.
            if let Some(retrieve) = resource.retrieve.as_deref().and_then(op) {
                for verb in [resource.update.as_deref(), resource.delete.as_deref()]
                    .into_iter()
                    .flatten()
                    .filter_map(op)
                {
                    if verb.output.id != retrieve.output.id {
                        issues.push(format!(
                            "resource {id}: {} returns {}, but {} returns {}; If-Match needs one representation",
                            verb.info.id, verb.output.id, retrieve.info.id, retrieve.output.id
                        ));
                    }
                }
            }
            let list = resource.list.as_deref().and_then(op);
            if let Some(list) = list
                && (page_item(&list.output).is_none() || list.output.get("next_cursor").is_none())
            {
                issues.push(format!(
                    "resource {id}: list operation {} must return a Page<T>",
                    list.info.id
                ));
            }
            let Some(view) = resource.admin_view() else {
                continue;
            };
            let Some(list) = list else {
                issues.push(format!(
                    "resource {id}: an admin view needs a list operation"
                ));
                continue;
            };
            let item = page_item(&list.output);
            for column in view.columns.iter().chain([&view.label]) {
                if item.is_none_or(|item| item.get(column).is_none()) {
                    issues.push(format!(
                        "resource {id}: admin column {column} is not a field of the listed items"
                    ));
                }
            }
            for filter in &view.filters {
                if list.input.get(filter).is_none() {
                    issues.push(format!(
                        "resource {id}: admin filter {filter} is not an input of {}",
                        list.info.id
                    ));
                }
            }
            for (form, target, role) in [
                (&view.edit_form, resource.update.as_deref(), "update"),
                (&view.create_form, resource.create.as_deref(), "create"),
            ] {
                let Some(form) = form else { continue };
                let found = forms.iter().find(|f| f.form.id == *form);
                let ok = found.is_some_and(|f| {
                    f.form.audience == Audience::Admin && Some(f.form.operation.as_str()) == target
                });
                if !ok {
                    issues.push(format!(
                        "resource {id}: admin {role} form {form} must be an admin-audience form for the {role} operation"
                    ));
                }
            }
            for admin_action in &view.actions {
                if let Some(id) = &admin_action.form {
                    let target = resource
                        .actions
                        .iter()
                        .find(|(n, _)| n == &admin_action.name)
                        .map(|(_, id)| id.as_ref());
                    if !forms.iter().any(|f| {
                        &f.form.id == id
                            && f.form.audience == Audience::Admin
                            && Some(f.form.operation.as_str()) == target
                    }) {
                        issues.push(format!("resource {id}: invalid action form {id}"));
                    }
                }
                let name = &admin_action.name;
                if let Some((field, _)) = &admin_action.visible_when
                    && item.is_none_or(|item| item.get(field).is_none())
                {
                    issues.push(format!("resource {id}: admin action {name} shows when {field}, which is not an item field"));
                }
                let action = resource
                    .actions
                    .iter()
                    .find(|(n, _)| n == name)
                    .and_then(|(_, o)| op(o));
                match action {
                    None => issues.push(format!(
                        "resource {id}: admin action {name} is not an action of this resource"
                    )),
                    Some(action) => {
                        for field in action.input.fields.iter().filter(|f| f.required) {
                            if field.name != "id"
                                && field.name != EXPECTED_VERSION_FIELD
                                && admin_action.form.is_none()
                            {
                                issues.push(format!(
                                    "resource {id}: admin action {name} needs input {}, which the admin shell cannot supply yet",
                                    field.name
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn check_jobs<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) {
    let mut kinds = HashSet::new();
    for builder in builders {
        for job in &builder.jobs {
            let kind: &str = &job.kind;
            if !valid_stable_id(kind) || owner(kind) != &*builder.app {
                issues.push(format!(
                    "job kind {kind:?} must be a stable ID namespaced to app {}",
                    builder.app
                ));
            }
            if !kinds.insert(kind) {
                issues.push(format!("job kind {kind} is registered twice"));
            }
        }
    }
}

/// Declared effects name things that exist, and a read declares no
/// writes or emits (ADR-018).
fn check_effects<S>(
    builders: &[AppBuilder<S>],
    desired: Option<&SchemaState>,
    issues: &mut Vec<String>,
) {
    let operations: Vec<&RegisteredOperation<S>> =
        builders.iter().flat_map(|b| &b.operations).collect();
    let jobs: HashSet<&str> = builders
        .iter()
        .flat_map(|b| &b.jobs)
        .map(|j| &*j.kind)
        .collect();
    for op in &operations {
        let Some(effects) = &op.info.effects else {
            continue;
        };
        let id = &op.info.id;
        if op.info.mode == crate::operation::Mode::Read
            && !(effects.writes.is_empty() && effects.emits.is_empty())
        {
            issues.push(format!(
                "operation {id} is a read; it cannot declare writes or emits"
            ));
        }
        // Without models (an invalid set) there is nothing to check against.
        if let Some(desired) = desired {
            for model in effects.reads.iter().chain(&effects.writes) {
                if desired.model(model).is_none() {
                    issues.push(format!(
                        "operation {id} declares an effect on model {model}, which no installed app declares"
                    ));
                }
            }
        }
        for job in &effects.emits {
            let known = jobs.contains(&**job) || operations.iter().any(|o| o.info.id == *job);
            if !known {
                issues.push(format!(
                    "operation {id} emits {job}, which is neither a registered operation nor a job kind"
                ));
            }
        }
        if effects.egress.iter().any(|e| e.trim().is_empty()) {
            issues.push(format!("operation {id} declares an empty egress"));
        }
    }
}

fn check_routes<S>(builders: &[AppBuilder<S>], issues: &mut Vec<String>) {
    let mut taken: Vec<(String, String, String)> = vec![
        ("GET".into(), LIVE_PATH.into(), "system".into()),
        ("GET".into(), READY_PATH.into(), "system".into()),
        ("GET".into(), MANIFEST_PATH.into(), "system".into()),
        ("GET".into(), INTERACTION_PATH.into(), "system".into()),
        ("GET".into(), ADMIN_MANIFEST_PATH.into(), "system".into()),
        ("GET".into(), UI_MANIFEST_PATH.into(), "system".into()),
        ("GET".into(), UI_VIEW_PATH.into(), "system".into()),
        ("GET".into(), ADMIN_INTERACTION_PATH.into(), "system".into()),
    ];
    let mut claim = |method: &str, path: &str, owner: String, issues: &mut Vec<String>| {
        if let Some((_, _, first)) = taken.iter().find(|(m, p, _)| m == method && p == path) {
            issues.push(format!(
                "{method} {path} is claimed by both {first} and {owner}"
            ));
        } else {
            taken.push((method.to_owned(), path.to_owned(), owner));
        }
    };
    for builder in builders {
        for op in &builder.operations {
            if let Some(binding) = &op.binding {
                claim(
                    &binding.method,
                    &binding.path,
                    format!("operation {}", op.info.id),
                    issues,
                );
            }
        }
        for route in &builder.unmanaged {
            if route.waiver.trim().is_empty() {
                issues.push(format!(
                    "unmanaged route {} {} in app {} needs a named waiver",
                    route.method, route.path, builder.app
                ));
            }
            if !valid_path(route.path) {
                issues.push(format!(
                    "unmanaged route {:?} is not a valid route path",
                    route.path
                ));
            }
            claim(
                route.method.as_str(),
                route.path,
                format!("app {} (unmanaged)", builder.app),
                issues,
            );
        }
    }
}

/// The assembled, immutable composition.
pub struct Registry<S> {
    name: Arc<str>,
    version: Arc<str>,
    desired: SchemaState,
    backfills: BTreeMap<String, serde_json::Value>,
    data_steps: Vec<DataStep>,
    apps: Vec<AppInfo>,
    operations: Vec<RegisteredOperation<S>>,
    forms: Vec<PublishedForm>,
    resources: Vec<RegisteredResource<S>>,
    views: Vec<PublishedView<S>>,
    unmanaged: Vec<UnmanagedRoute<S>>,
    schema: Vec<MigrationRef>,
    jobs: Vec<RegisteredJob<S>>,
}

impl<S> Registry<S> {
    /// The handler registered for job `kind`.
    pub fn job_handler(&self, kind: &str) -> Option<&dyn JobHandler<S>> {
        self.jobs
            .iter()
            .find(|j| &*j.kind == kind)
            .map(|j| &*j.handler)
    }

    /// Every registered job handler, in lifecycle order.
    pub fn jobs(&self) -> &[RegisteredJob<S>] {
        &self.jobs
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// Apps in lifecycle (dependency) order.
    pub fn apps(&self) -> &[AppInfo] {
        &self.apps
    }

    pub fn operations(&self) -> &[RegisteredOperation<S>] {
        &self.operations
    }

    pub fn operation(&self, id: &str) -> Option<&RegisteredOperation<S>> {
        self.operations.iter().find(|op| &*op.info.id == id)
    }

    /// Runs the operation registered as `id` for a caller that is not an
    /// HTTP request (ADR-013): its context is resolved from `invocation`,
    /// then the input is validated and the executor runs, exactly as a
    /// managed route would. Not charged to admission. An unknown `id` is 404.
    pub async fn invoke(
        &self,
        state: &S,
        id: &str,
        invocation: &Invocation,
        input: &Value,
    ) -> Result<Value, Problem> {
        let op = self.operation(id).ok_or_else(Problem::not_found)?;
        if let Some(key) = invocation.idempotency_key() {
            if !op.accepts_idempotency_key() {
                return Err(crate::idempotency::unsupported());
            }
            if !crate::idempotency::valid_key(key) {
                return Err(crate::idempotency::invalid());
            }
        }
        (op.invoker)(state, invocation, input).await
    }

    pub fn forms(&self) -> &[PublishedForm] {
        &self.forms
    }

    pub fn form(&self, id: &str) -> Option<&PublishedForm> {
        self.forms.iter().find(|f| f.form.id == id)
    }

    pub fn resources(&self) -> &[RegisteredResource<S>] {
        &self.resources
    }

    /// Desired persistence state (D) of all installed models.
    pub fn desired_schema(&self) -> &SchemaState {
        &self.desired
    }

    /// One-off backfill values declared on model columns, by field ID.
    /// Every registered migration data step.
    pub fn data_steps(&self) -> &[DataStep] {
        &self.data_steps
    }

    pub fn backfills(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.backfills
    }

    /// Every migration the installed apps need before admitting requests.
    pub fn required_migrations(&self) -> &[MigrationRef] {
        &self.schema
    }

    /// Every route the router serves, sorted by path then method.
    pub fn routes(&self) -> Vec<RouteInfo> {
        let mut routes: Vec<RouteInfo> = [
            LIVE_PATH,
            READY_PATH,
            MANIFEST_PATH,
            INTERACTION_PATH,
            ADMIN_MANIFEST_PATH,
            ADMIN_INTERACTION_PATH,
            UI_MANIFEST_PATH,
            UI_VIEW_PATH,
        ]
        .into_iter()
        .map(|path| RouteInfo {
            method: "GET".into(),
            path: path.into(),
            kind: RouteKind::System,
        })
        .collect();
        for op in &self.operations {
            if let Some(binding) = &op.binding {
                routes.push(RouteInfo {
                    method: binding.method.clone(),
                    path: binding.path.clone(),
                    kind: RouteKind::Managed {
                        operation: op.info.id.clone(),
                    },
                });
            }
        }
        for route in &self.unmanaged {
            routes.push(RouteInfo {
                method: route.method.to_string(),
                path: route.path.to_owned(),
                kind: RouteKind::Unmanaged {
                    app: route.app.clone(),
                    waiver: route.waiver,
                },
            });
        }
        routes.sort_by(|a, b| (&a.path, &a.method).cmp(&(&b.path, &b.method)));
        routes
    }
}

impl<S> Registry<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
{
    /// Every managed, unmanaged and system route. Attach state and pass the
    /// result through [`crate::http::finish`].
    pub fn router(&self) -> Router<S> {
        let mut by_path: BTreeMap<String, MethodRouter<S>> = BTreeMap::new();
        let mut add = |path: &str, route: MethodRouter<S>| {
            let merged = match by_path.remove(path) {
                Some(existing) => existing.merge(route),
                None => route,
            };
            by_path.insert(path.to_owned(), merged);
        };
        add(LIVE_PATH, health::live());
        add(READY_PATH, health::ready(Arc::from(self.schema.clone())));
        let index: meta::MetaIndex<S> = Arc::new(
            self.forms
                .iter()
                .filter(|f| f.form.audience == Audience::Product)
                .filter_map(|f| {
                    let op = self.operation(&f.form.operation)?;
                    Some(meta::MetaEntry {
                        interaction_id: f.form.id.clone(),
                        revision: f.revision.clone(),
                        operation: op.info.id.clone(),
                        title: f.form.title.clone(),
                        descriptor: f.descriptor.clone(),
                        availability: op.availability.clone()?,
                    })
                })
                .collect(),
        );
        add(
            UI_MANIFEST_PATH,
            meta::ui_manifest_route(
                self.views
                    .iter()
                    .map(|v| (v.document.clone(), v.check.clone()))
                    .collect(),
            ),
        );
        add(
            UI_VIEW_PATH,
            meta::ui_view_route(
                self.views
                    .iter()
                    .map(|v| (v.document.clone(), v.check.clone()))
                    .collect(),
            ),
        );
        add(MANIFEST_PATH, meta::manifest_route(index.clone()));
        add(INTERACTION_PATH, meta::interaction_route(index));

        let admin_resources: meta::AdminIndex<S> = Arc::new(
            self.resources
                .iter()
                .filter_map(|r| {
                    let admin = r.admin.as_ref()?;
                    Some(meta::AdminEntry {
                        description: self.admin_description(r)?,
                        check: admin.check.clone(),
                    })
                })
                .collect(),
        );
        let admin_forms: meta::AdminFormIndex<S> = Arc::new(
            self.forms
                .iter()
                .filter(|f| f.form.audience == Audience::Admin)
                .filter_map(|f| {
                    let op = self.operation(&f.form.operation)?;
                    let resource = self.resources.iter().find(|r| {
                        r.admin_view().is_some_and(|v| {
                            v.edit_form.as_deref() == Some(&f.form.id)
                                || v.create_form.as_deref() == Some(&f.form.id)
                                || v.actions
                                    .iter()
                                    .any(|a| a.form.as_deref() == Some(&f.form.id))
                        })
                    })?;
                    Some(meta::AdminFormEntry {
                        interaction_id: f.form.id.clone(),
                        descriptor: f.descriptor.clone(),
                        availability: op.availability.clone()?,
                        check: resource.admin.as_ref()?.check.clone(),
                    })
                })
                .collect(),
        );
        add(
            ADMIN_MANIFEST_PATH,
            meta::admin_manifest_route(admin_resources),
        );
        add(
            ADMIN_INTERACTION_PATH,
            meta::admin_interaction_route(admin_forms),
        );
        for op in &self.operations {
            if let (Some(binding), Some(route)) = (&op.binding, &op.route) {
                add(&binding.path, route.clone());
            }
        }
        for route in &self.unmanaged {
            add(route.path, route.route.clone());
        }
        by_path
            .into_iter()
            .fold(Router::new(), |router, (path, route)| {
                router.route(&path, route)
            })
    }
}

#[cfg(test)]
mod tests;
