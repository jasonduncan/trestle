//! Metadata publication (design doc §8.3–§8.5).
//!
//! * `GET /_meta/v1/manifest`: interactions the caller can see, with
//!   advisory availability.
//! * `GET /_meta/v1/interactions/{id}`: one descriptor plus availability.
//!
//! Visibility requires the same request context as the bound operation
//! (authentication, tenant membership). Availability runs the operation's
//! `Policy::admit` and is advisory only: executing re-authorizes (I03).
//! Responses are per-caller, so every one is `Cache-Control: private,
//! no-store`, including problems.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{FromRef, Path, Request, State};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use http::header;
use http::request::Parts;
use serde_json::{Value, json};
use trestle_protocol::interaction::{PROTOCOL, PROTOCOL_VERSION};

use crate::Problem;
use crate::identity::{BoxFuture, Identity};
use crate::invocation::{FromInvocation, Invocation};
use crate::operation::{DynOperation, Operation, Policy, RequestContext};
use crate::registry::resource::AdminCheck;

pub const MANIFEST_PATH: &str = "/_meta/v1/manifest";
pub const INTERACTION_PATH: &str = "/_meta/v1/interactions/{id}";
/// The admin publication profile (§8.3, §10): separate endpoints, each
/// resource filtered by its admin policy.
pub const ADMIN_MANIFEST_PATH: &str = "/_meta/v1/admin/manifest";
pub const ADMIN_INTERACTION_PATH: &str = "/_meta/v1/admin/interactions/{id}";

/// Advisory, caller-specific: whether the operation's admission policy
/// currently passes. Never a capability token (§8.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub available: bool,
    /// Safe problem code when unavailable, never policy internals.
    pub reason: Option<String>,
}

impl Availability {
    fn to_json(&self) -> Value {
        match &self.reason {
            Some(reason) => json!({ "available": self.available, "reason": reason }),
            None => json!({ "available": self.available }),
        }
    }
}

pub(crate) type AvailabilityFn<S> = Arc<
    dyn for<'a> Fn(&'a mut Parts, &'a S) -> BoxFuture<'a, Result<Availability, Problem>>
        + Send
        + Sync,
>;

fn erase<S, F>(f: F) -> AvailabilityFn<S>
where
    F: for<'a> Fn(&'a mut Parts, &'a S) -> BoxFuture<'a, Result<Availability, Problem>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

/// Resolves `O`'s context for the caller and evaluates admission.
pub(crate) fn availability<S, O>(op: Arc<O>) -> AvailabilityFn<S>
where
    S: Send + Sync + 'static,
    O: Operation,
    O::Context: FromInvocation<S>,
    Identity: FromRef<S>,
{
    let descriptor = Arc::new(O::Context::describe());
    erase(move |parts, state| {
        let op = op.clone();
        let descriptor = descriptor.clone();
        Box::pin(async move {
            let invocation = Invocation::from_http(parts, state, &descriptor).await?;
            let ctx = O::Context::resolve(&invocation, state).await?;
            Ok(match op.policy().admit(&ctx) {
                Ok(()) => Availability {
                    available: true,
                    reason: None,
                },
                Err(problem) => Availability {
                    available: false,
                    reason: Some(problem.code().to_owned()),
                },
            })
        })
    })
}

/// [`availability`] for an operation built at runtime.
pub(crate) fn dyn_availability<S>(op: Arc<dyn DynOperation<S>>) -> AvailabilityFn<S>
where
    S: Send + Sync + 'static,
    Identity: FromRef<S>,
{
    let descriptor = Arc::new(op.context());
    erase(move |parts, state| {
        let op = op.clone();
        let descriptor = descriptor.clone();
        Box::pin(async move {
            let invocation = Invocation::from_http(parts, state, &descriptor).await?;
            let ctx = op.resolve_context(&invocation, state).await?;
            Ok(match op.admit(&ctx) {
                Ok(()) => Availability {
                    available: true,
                    reason: None,
                },
                Err(problem) => Availability {
                    available: false,
                    reason: Some(problem.code().to_owned()),
                },
            })
        })
    })
}

pub(crate) struct MetaEntry<S> {
    pub interaction_id: String,
    pub revision: String,
    pub operation: Arc<str>,
    pub title: String,
    pub descriptor: Value,
    pub availability: AvailabilityFn<S>,
}

pub(crate) type MetaIndex<S> = Arc<Vec<MetaEntry<S>>>;

pub(crate) fn manifest_route<S>(index: MetaIndex<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    get(move |State(state): State<S>, request: Request| {
        let index = index.clone();
        async move { no_store(manifest(&state, &index, request).await) }
    })
}

pub(crate) fn interaction_route<S>(index: MetaIndex<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    get(
        move |State(state): State<S>, id: Result<Path<String>, PathRejection>, request: Request| {
            let index = index.clone();
            async move {
                let id = id.map(|Path(id)| id).unwrap_or_default();
                no_store(interaction(&state, &index, &id, request).await)
            }
        },
    )
}

async fn manifest<S>(state: &S, index: &[MetaEntry<S>], request: Request) -> Response {
    let (mut parts, _) = request.into_parts();
    let mut listed = Vec::new();
    let mut first_problem = None;
    for entry in index {
        match (entry.availability)(&mut parts, state).await {
            Ok(availability) => listed.push(json!({
                "interactionId": entry.interaction_id,
                "revision": entry.revision,
                "operation": entry.operation,
                "title": entry.title,
                "href": format!("/_meta/v1/interactions/{}", entry.interaction_id),
                "availability": availability.to_json(),
            })),
            Err(problem) => {
                first_problem.get_or_insert(problem);
            }
        }
    }
    // A caller who can see nothing gets the reason (401, missing tenant…)
    // rather than an empty list that looks like "nothing exists".
    if listed.is_empty()
        && let Some(problem) = first_problem
    {
        return problem.into_response();
    }
    Json(json!({
        "protocol": PROTOCOL,
        "protocolVersion": PROTOCOL_VERSION,
        "audience": "product",
        "interactions": listed,
    }))
    .into_response()
}

async fn interaction<S>(state: &S, index: &[MetaEntry<S>], id: &str, request: Request) -> Response {
    let Some(entry) = index.iter().find(|e| e.interaction_id == id) else {
        return Problem::not_found().into_response();
    };
    let (mut parts, _) = request.into_parts();
    match (entry.availability)(&mut parts, state).await {
        Ok(availability) => {
            let mut descriptor = entry.descriptor.clone();
            descriptor["availability"] = availability.to_json();
            Json(descriptor).into_response()
        }
        Err(problem) => problem.into_response(),
    }
}

pub(crate) struct AdminEntry<S> {
    pub description: Value,
    pub check: AdminCheck<S>,
}

pub(crate) type AdminIndex<S> = Arc<Vec<AdminEntry<S>>>;

pub(crate) struct AdminFormEntry<S> {
    pub interaction_id: String,
    pub descriptor: Value,
    pub availability: AvailabilityFn<S>,
    pub check: AdminCheck<S>,
}

pub(crate) type AdminFormIndex<S> = Arc<Vec<AdminFormEntry<S>>>;

pub(crate) fn admin_manifest_route<S>(index: AdminIndex<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    get(move |State(state): State<S>, request: Request| {
        let index = index.clone();
        async move {
            let (mut parts, _) = request.into_parts();
            let mut resources = Vec::new();
            let mut first_problem = None;
            for entry in index.iter() {
                match (entry.check)(&mut parts, &state).await {
                    Ok(()) => resources.push(entry.description.clone()),
                    Err(problem) => {
                        first_problem.get_or_insert(problem);
                    }
                }
            }
            let response = match (resources.is_empty(), first_problem) {
                (true, Some(problem)) => problem.into_response(),
                _ => Json(json!({
                    "protocol": PROTOCOL,
                    "protocolVersion": PROTOCOL_VERSION,
                    "audience": "admin",
                    "resources": resources,
                }))
                .into_response(),
            };
            no_store(response)
        }
    })
}

pub(crate) fn admin_interaction_route<S>(index: AdminFormIndex<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    get(
        move |State(state): State<S>, id: Result<Path<String>, PathRejection>, request: Request| {
            let index = index.clone();
            async move {
                let id = id.map(|Path(id)| id).unwrap_or_default();
                let Some(entry) = index.iter().find(|e| e.interaction_id == id) else {
                    return no_store(Problem::not_found().into_response());
                };
                let (mut parts, _) = request.into_parts();
                let response = async {
                    (entry.check)(&mut parts, &state).await?;
                    let availability = (entry.availability)(&mut parts, &state).await?;
                    let mut descriptor = entry.descriptor.clone();
                    descriptor["availability"] = availability.to_json();
                    Ok::<_, Problem>(Json(descriptor).into_response())
                }
                .await;
                no_store(response.unwrap_or_else(IntoResponse::into_response))
            }
        },
    )
}

type ViewIndex<S> = Vec<(trestle_protocol::view::ViewDoc, AdminCheck<S>)>;

pub(crate) fn ui_manifest_route<S>(index: ViewIndex<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    let index = Arc::new(index);
    get(move |State(state): State<S>, request: Request| {
        let index = index.clone();
        async move {
            let (mut parts, _) = request.into_parts();
            let mut views = vec![];
            let mut authentication = None;
            for (doc, check) in index.iter() {
                match check(&mut parts, &state).await {
                    Ok(()) => views.push(json!({ "viewId": doc.view_id, "revision": doc.revision,
                        "audience": doc.audience, "kind": doc.kind, "title": doc.title,
                        "url": format!("/_meta/v1/ui/views/{}", doc.view_id) })),
                    Err(problem) if problem.status() == http::StatusCode::UNAUTHORIZED => {
                        authentication = Some(problem);
                    }
                    Err(_) => {} // Publication refuses discovery; no policy details leak.
                }
            }
            if views.is_empty()
                && let Some(problem) = authentication
            {
                return no_store(problem.into_response());
            }
            no_store(Json(json!({ "protocol": "trestle.views", "protocolVersion": "1.0", "views": views })).into_response())
        }
    })
}

pub(crate) fn ui_view_route<S>(index: ViewIndex<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    let index = Arc::new(index);
    get(
        move |State(state): State<S>, id: Result<Path<String>, PathRejection>, request: Request| {
            let index = index.clone();
            async move {
                let id = id.map(|Path(id)| id).unwrap_or_default();
                let (mut parts, _) = request.into_parts();
                let Some((doc, check)) = index.iter().find(|(doc, _)| doc.view_id == id) else {
                    return no_store(Problem::not_found().into_response());
                };
                match check(&mut parts, &state).await {
                    Ok(()) => no_store(Json(doc).into_response()),
                    Err(problem) if problem.status() == http::StatusCode::UNAUTHORIZED => {
                        no_store(problem.into_response())
                    }
                    Err(_) => no_store(Problem::not_found().into_response()),
                }
            }
        },
    )
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}
