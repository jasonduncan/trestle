//! Resources: narrow DRF-style conventions over ordinary operations
//! (design doc §7.1, §10).
//!
//! A resource only chooses HTTP bindings for operations you already wrote;
//! it adds no behavior and no authority. Everything it registers is a
//! normal managed operation, so the executor, contracts and policies apply
//! exactly as for hand-bound ones. CRUD is opt-in per verb, and domain
//! actions are explicit (`.action("publish", PublishLesson)`), never
//! inferred from model fields.

use std::sync::Arc;

use axum::extract::FromRef;
use http::request::Parts;
use sqlx::PgPool;
use trestle_protocol::Contract;

use super::{AppBuilder, Http};
use crate::Problem;
use crate::identity::{BoxFuture, Identity};
use crate::invocation::{FromInvocation, Invocation};
use crate::operation::{Operation, Policy};

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
            visible_when: Some((field.to_owned(), value)),
        });
        self
    }
}

pub(crate) struct AdminProfile<S> {
    pub view: AdminView,
    pub policy: &'static str,
    pub check: AdminCheck<S>,
}

pub struct RegisteredResource<S> {
    pub app: &'static str,
    pub id: &'static str,
    pub path: &'static str,
    pub list: Option<&'static str>,
    pub create: Option<&'static str>,
    pub retrieve: Option<&'static str>,
    pub update: Option<&'static str>,
    pub delete: Option<&'static str>,
    /// (action name, operation ID)
    pub actions: Vec<(String, &'static str)>,
    pub(crate) admin: Option<AdminProfile<S>>,
}

impl<S> RegisteredResource<S> {
    pub fn admin_view(&self) -> Option<&AdminView> {
        self.admin.as_ref().map(|a| &a.view)
    }

    pub fn admin_policy(&self) -> Option<&'static str> {
        self.admin.as_ref().map(|a| a.policy)
    }
}

pub struct ResourceRegistration<'a, S> {
    pub(crate) builder: &'a mut AppBuilder<S>,
    pub(crate) index: usize,
}

impl<S> ResourceRegistration<'_, S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
{
    fn resource(&mut self) -> &mut RegisteredResource<S> {
        &mut self.builder.resources[self.index]
    }

    fn path(&self) -> &'static str {
        self.builder.resources[self.index].path
    }

    fn register<O>(&mut self, op: O, http: Http) -> &'static str
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let http = if O::Output::contract().get(VERSION_FIELD).is_some() {
            http.etag(VERSION_FIELD)
        } else {
            http
        };
        self.builder.operation(op).http(http);
        O::INFO.id
    }

    /// `GET {path}`: a page of items (output `Page<T>`).
    pub fn list<O>(mut self, op: O) -> Self
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let path = self.path();
        let id = self.register(op, Http::get(path));
        self.resource().list = Some(id);
        self
    }

    /// `POST {path}`: 201 with `Location: {path}/{id}`.
    pub fn create<O>(mut self, op: O) -> Self
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let path = self.path();
        let id = self.register(op, Http::post(path).created(format!("{path}/{{id}}")));
        self.resource().create = Some(id);
        self
    }

    /// `GET {path}/{id}` with an `ETag`.
    pub fn retrieve<O>(mut self, op: O) -> Self
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let path = self.path();
        let id = self.register(op, Http::get(format!("{path}/{{id}}")));
        self.resource().retrieve = Some(id);
        self
    }

    /// `PATCH {path}/{id}`; `If-Match` required and passed as
    /// `expected_version`.
    pub fn update<O>(mut self, op: O) -> Self
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let path = self.path();
        let http = Http::patch(format!("{path}/{{id}}")).if_match(EXPECTED_VERSION_FIELD);
        let id = self.register(op, http);
        self.resource().update = Some(id);
        self
    }

    /// `DELETE {path}/{id}`; `If-Match` required; 204.
    pub fn delete<O>(mut self, op: O) -> Self
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let path = self.path();
        let http = Http::delete(format!("{path}/{{id}}"))
            .if_match(EXPECTED_VERSION_FIELD)
            .no_content();
        self.builder.operation(op).http(http);
        self.resource().delete = Some(O::INFO.id);
        self
    }

    /// `POST {path}/{id}/actions/{name}`: a domain transition. Its input
    /// usually carries `expected_version` (409 when stale, §6.4).
    pub fn action<O>(mut self, name: &str, op: O) -> Self
    where
        O: Operation,
        O::Context: FromInvocation<S>,
        Identity: FromRef<S>,
    {
        let path = self.path();
        let id = self.register(op, Http::post(format!("{path}/{{id}}/actions/{name}")));
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
        Identity: FromRef<S>,
    {
        let id = policy.id();
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
}
