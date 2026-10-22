//! Resources: narrow DRF-style conventions over ordinary operations
//! (design doc §7.1, §10).
//!
//! A resource only chooses HTTP bindings for operations you already wrote;
//! it adds no behavior and no authority. Everything it registers is a
//! normal managed operation, so the executor, contracts and policies apply
//! exactly as for hand-bound ones. CRUD is opt-in per verb, and domain
//! actions are explicit (`.action("publish", PublishLesson)`), never
//! inferred from model fields. Verbs take an [`Operation`] or one built at
//! runtime (`Arc<dyn DynOperation<S>>`, ADR-029), registered the same way.

use std::sync::Arc;

use super::{AppBuilder, Http};
use crate::Problem;
use crate::identity::{BoxFuture, Identity};
use crate::invocation::{FromInvocation, Invocation};
use crate::operation::{DynOperation, DynPolicy, Operation, Policy};
use axum::extract::FromRef;
use http::request::Parts;
use sqlx::PgPool;
use trestle_protocol::Contract;
use trestle_protocol::ContractDoc;

/// Conventional field names: responses carry `version`; updates, deletes
/// and domain actions compare `expected_version`.
pub const VERSION_FIELD: &str = "version";
pub const EXPECTED_VERSION_FIELD: &str = "expected_version";

pub(crate) type AdminCheck<S> =
    Arc<dyn for<'a> Fn(&'a mut Parts, &'a S) -> BoxFuture<'a, Result<(), Problem>> + Send + Sync>;

fn erase<S, F>(f: F) -> AdminCheck<S>
where
    F: for<'a> Fn(&'a mut Parts, &'a S) -> BoxFuture<'a, Result<(), Problem>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

/// What an admin screen shows for a resource. Declarative data only.
#[derive(Debug, Clone, Default)]
pub struct AdminView {
    pub title: String,
    /// Item field used as a record's name.
    pub label: String,
    /// Item fields shown as list columns, in order.
    pub columns: Vec<String>,
    /// List input fields offered as filters.
    pub filters: Vec<String>,
    /// Admin-audience form for the update operation.
    pub edit_form: Option<String>,
    /// Admin-audience form for the create operation.
    pub create_form: Option<String>,
    /// Domain actions offered as buttons.
    pub actions: Vec<AdminAction>,
}

#[derive(Debug, Clone)]
pub struct AdminAction {
    pub name: String,
    pub label: String,
    pub form: Option<String>,
    /// Advisory display rule (§9.3): show the button only when the listed
    /// item's field equals this value. The server still decides.
    pub visible_when: Option<(String, serde_json::Value)>,
}

impl AdminView {
    pub fn new(title: &str, label: &str) -> Self {
        Self {
            title: title.to_owned(),
            label: label.to_owned(),
            ..Default::default()
        }
    }

    pub fn columns(mut self, columns: &[&str]) -> Self {
        self.columns = columns.iter().map(|c| (*c).to_owned()).collect();
        self
    }

    pub fn filters(mut self, filters: &[&str]) -> Self {
        self.filters = filters.iter().map(|f| (*f).to_owned()).collect();
        self
    }

    pub fn edit_form(mut self, form: &str) -> Self {
        self.edit_form = Some(form.to_owned());
        self
    }

    pub fn create_form(mut self, form: &str) -> Self {
        self.create_form = Some(form.to_owned());
        self
    }

    pub fn action(mut self, name: &str, label: &str) -> Self {
        self.actions.push(AdminAction {
            name: name.to_owned(),
            label: label.to_owned(),
            form: None,
            visible_when: None,
        });
        self
    }

    /// A domain action with explicit editable input presentation.
    pub fn action_form(mut self, name: &str, label: &str, form: &str) -> Self {
        self.actions.push(AdminAction {
            name: name.into(),
            label: label.into(),
            form: Some(form.into()),
            visible_when: None,
        });
        self
    }

    /// An action button shown only while item `field` equals `value`.
    pub fn action_when(
        mut self,
        name: &str,
        label: &str,
        field: &str,
        value: serde_json::Value,
    ) -> Self {
        self.actions.push(AdminAction {
            name: name.to_owned(),
            label: label.to_owned(),
            form: None,
            visible_when: Some((field.to_owned(), value)),
        });
        self
    }
}

pub(crate) struct AdminProfile<S> {
    pub view: AdminView,
    pub policy: Arc<str>,
    pub check: AdminCheck<S>,
}

pub struct RegisteredResource<S> {
    pub app: Arc<str>,
    pub id: Arc<str>,
    pub path: Arc<str>,
    pub list: Option<Arc<str>>,
    pub create: Option<Arc<str>>,
    pub retrieve: Option<Arc<str>>,
    pub update: Option<Arc<str>>,
    pub delete: Option<Arc<str>>,
    /// (action name, operation ID)
    pub actions: Vec<(String, Arc<str>)>,
    pub(crate) admin: Option<AdminProfile<S>>,
}

impl<S> RegisteredResource<S> {
    pub fn admin_view(&self) -> Option<&AdminView> {
        self.admin.as_ref().map(|a| &a.view)
    }

    pub fn admin_policy(&self) -> Option<&str> {
        self.admin.as_ref().map(|a| &*a.policy)
    }
}

/// What a resource verb takes: an [`Operation`] you wrote, or an
/// `Arc<dyn DynOperation<S>>` built at runtime. Either becomes an ordinary
/// managed operation.
pub trait ResourceOp<S> {
    /// The output contract (a `version` field earns an `ETag`).
    fn output_contract(&self) -> &ContractDoc;
    /// Registers the operation with `http`; returns its ID.
    fn register(self, builder: &mut AppBuilder<S>, http: Http) -> Arc<str>;
}

impl<S, O> ResourceOp<S> for O
where
    S: Clone + Send + Sync + 'static,
    O: Operation,
    O::Context: FromInvocation<S>,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    fn output_contract(&self) -> &ContractDoc {
        O::Output::contract()
    }

    fn register(self, builder: &mut AppBuilder<S>, http: Http) -> Arc<str> {
        builder.operation(self).http(http);
        Arc::from(O::INFO.id)
    }
}

impl<S> ResourceOp<S> for Arc<dyn DynOperation<S>>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    fn output_contract(&self) -> &ContractDoc {
        self.output()
    }

    fn register(self, builder: &mut AppBuilder<S>, http: Http) -> Arc<str> {
        let id = Arc::from(&*self.info().id);
        builder.dyn_operation(self, Some(http));
        id
    }
}

pub struct ResourceRegistration<'a, S> {
    pub(crate) builder: &'a mut AppBuilder<S>,
    pub(crate) index: usize,
    /// [`ResourceRegistration::idempotency_key`] was called.
    pub(crate) idempotency_key: bool,
}

impl<S> ResourceRegistration<'_, S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    fn resource(&mut self) -> &mut RegisteredResource<S> {
        &mut self.builder.resources[self.index]
    }

    fn path(&self) -> Arc<str> {
        self.builder.resources[self.index].path.clone()
    }

    /// The conventional binding of a write verb, keyed when the resource
    /// asked for it.
    fn write(&self, http: Http) -> Http {
        if self.idempotency_key {
            http.idempotency_key()
        } else {
            http
        }
    }

    /// The conventional write bindings registered after this call (create,
    /// update, delete and [`action`](Self::action)) accept an optional
    /// `Idempotency-Key` ([`Http::idempotency_key`], ADR-016). Call it
    /// before those verbs. [`action_at`](Self::action_at) keeps the binding
    /// it is given. The project needs
    /// `trestle_core::idempotency::IdempotencyApp`.
    pub fn idempotency_key(mut self) -> Self {
        self.idempotency_key = true;
        self
    }

    fn register(&mut self, op: impl ResourceOp<S>, http: Http) -> Arc<str> {
        let http = if op.output_contract().get(VERSION_FIELD).is_some() {
            http.etag(VERSION_FIELD)
        } else {
            http
        };
        op.register(self.builder, http)
    }

    /// `GET {path}`: a page of items (output `Page<T>`).
    pub fn list(mut self, op: impl ResourceOp<S>) -> Self {
        let path = self.path();
        let id = self.register(op, Http::get(&*path));
        self.resource().list = Some(id);
        self
    }

    /// `POST {path}`: 201 with `Location: {path}/{id}`.
    pub fn create(mut self, op: impl ResourceOp<S>) -> Self {
        let path = self.path();
        let http = self.write(Http::post(&*path).created(format!("{path}/{{id}}")));
        let id = self.register(op, http);
        self.resource().create = Some(id);
        self
    }

    /// `GET {path}/{id}` with an `ETag`.
    pub fn retrieve(mut self, op: impl ResourceOp<S>) -> Self {
        let path = self.path();
        let id = self.register(op, Http::get(format!("{path}/{{id}}")));
        self.resource().retrieve = Some(id);
        self
    }

    /// `PATCH {path}/{id}`; `If-Match` required and passed as
    /// `expected_version`.
    pub fn update(mut self, op: impl ResourceOp<S>) -> Self {
        let path = self.path();
        let http =
            self.write(Http::patch(format!("{path}/{{id}}")).if_match(EXPECTED_VERSION_FIELD));
        let id = self.register(op, http);
        self.resource().update = Some(id);
        self
    }

    /// `DELETE {path}/{id}`; `If-Match` required; 204.
    pub fn delete(mut self, op: impl ResourceOp<S>) -> Self {
        let path = self.path();
        let http = self.write(
            Http::delete(format!("{path}/{{id}}"))
                .if_match(EXPECTED_VERSION_FIELD)
                .no_content(),
        );
        let id = op.register(self.builder, http);
        self.resource().delete = Some(id);
        self
    }

    /// `POST {path}/{id}/actions/{name}`: a domain transition. Its input
    /// usually carries `expected_version` (409 when stale, §6.4).
    pub fn action(mut self, name: &str, op: impl ResourceOp<S>) -> Self {
        let path = self.path();
        let http = self.write(Http::post(format!("{path}/{{id}}/actions/{name}")));
        let id = self.register(op, http);
        self.resource().actions.push((name.to_owned(), id));
        self
    }

    /// [`action`](Self::action) at a binding you choose, such as one an
    /// intent file declares. With `.if_match(..)`, the admin shell sends the
    /// row's version as `If-Match`.
    pub fn action_at(mut self, name: &str, op: impl ResourceOp<S>, http: Http) -> Self {
        let id = self.register(op, http);
        self.resource().actions.push((name.to_owned(), id));
        self
    }

    /// Publishes this resource in the admin profile to callers `policy`
    /// admits. Admin metadata grants nothing: every operation still runs
    /// its own policy.
    pub fn admin<C, P>(mut self, view: AdminView, policy: P) -> Self
    where
        C: FromInvocation<S>,
        P: Policy<C>,
    {
        let id = Arc::from(policy.id());
        let policy = Arc::new(policy);
        let descriptor = Arc::new(C::describe());
        let check = erase(move |parts, state| {
            let policy = policy.clone();
            let descriptor = descriptor.clone();
            Box::pin(async move {
                let invocation = Invocation::from_http(parts, state, &descriptor).await?;
                let ctx = C::resolve(&invocation, state).await?;
                policy.admit(&ctx)
            })
        });
        self.resource().admin = Some(AdminProfile {
            view,
            policy: id,
            check,
        });
        self
    }

    /// [`admin`](Self::admin) with a policy built at runtime.
    pub fn admin_dyn(mut self, view: AdminView, policy: Arc<dyn DynPolicy<S>>) -> Self {
        let id = Arc::from(policy.id());
        let descriptor = Arc::new(policy.context());
        let check = erase(move |parts, state| {
            let policy = policy.clone();
            let descriptor = descriptor.clone();
            Box::pin(async move {
                let invocation = Invocation::from_http(parts, state, &descriptor).await?;
                let ctx = policy.resolve_context(&invocation, state).await?;
                policy.admit(&ctx)
            })
        });
        self.resource().admin = Some(AdminProfile {
            view,
            policy: id,
            check,
        });
        self
    }
}
