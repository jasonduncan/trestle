//! Managed HTTP routes: the generated adapter between a request and the
//! executor (I04, I13). It builds the caller's `Invocation` and resolves the
//! context from it (ADR-013), checks the interaction
//! revision, assembles the input instance, enforces preconditions,
//! validates against the contract and calls the same executor a
//! hand-written handler would.
//!
//! Input instance = path parameters + (query parameters for GET/DELETE, or
//! the JSON body for POST/PUT/PATCH) + the `If-Match` version when the
//! binding requires one. A name may come from only one source. Path and
//! query values arrive as text and are converted to the contract's integer
//! or boolean types; everything else is validated as sent.

use std::sync::{Arc, OnceLock};

use axum::Json;
use axum::extract::{FromRef, FromRequest, FromRequestParts, Query, RawPathParams, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, MethodRouter};
use http::request::Parts;
use http::{HeaderValue, Method, StatusCode, header};
use serde_json::{Map, Number, Value};
use sqlx::PgPool;
use trestle_protocol::interaction::{BodySource, HttpBinding, REVISION_HEADER};
use trestle_protocol::{ContractDoc, FieldType};

use crate::Problem;
use crate::idempotency::{self, IDEMPOTENCY_KEY_HEADER};
use crate::identity::Identity;
use crate::input::JsonBody;
use crate::invocation::{FromInvocation, Invocation};
use crate::operation::{
    ContextDescriptor, DynOperation, Keyed, Operation, RequestContext, execute_dyn,
    execute_dyn_keyed, execute_keyed, execute_validated, validate,
};

/// How an operation is exposed over HTTP. Registered with
/// `AppBuilder::operation(op).http(Http::post(..))` or through a resource.
#[derive(Debug, Clone)]
pub struct Http {
    method: Method,
    path: String,
    success: StatusCode,
    location: Option<String>,
    etag: Option<String>,
    if_match: Option<String>,
    no_content: bool,
    idempotency_key: bool,
}

impl Http {
    /// Input from path and query parameters; success 200.
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    /// Input from path parameters and the JSON body; success 200 unless
    /// [`Http::created`].
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::POST, path)
    }

    /// Partial update: path parameters plus a JSON body of changes.
    pub fn patch(path: impl Into<String>) -> Self {
        Self::new(Method::PATCH, path)
    }

    /// Input from path and query parameters.
    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::DELETE, path)
    }

    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            success: StatusCode::OK,
            location: None,
            etag: None,
            if_match: None,
            no_content: false,
            idempotency_key: false,
        }
    }

    /// Accepts an optional `Idempotency-Key` (design doc §6.4, ADR-016): a
    /// repeat with the same key and input replays the committed response
    /// instead of running again. Write operations only; the project needs
    /// `trestle_core::idempotency::IdempotencyApp`.
    pub fn idempotency_key(mut self) -> Self {
        self.idempotency_key = true;
        self
    }

    /// 201 Created with a `Location` built from output fields, e.g.
    /// `/api/v1/lessons/{id}`.
    pub fn created(mut self, location: impl Into<String>) -> Self {
        self.success = StatusCode::CREATED;
        self.location = Some(location.into());
        self
    }

    /// Sends `ETag: "<output contract>:<value of output field>"` (§6.4).
    pub fn etag(mut self, output_field: &str) -> Self {
        self.etag = Some(output_field.to_owned());
        self
    }

    /// Requires `If-Match` (428 if absent). A tag for this representation
    /// fills `input_field` with its version; the operation compares it and
    /// answers 412 when stale. A tag for another representation is 412.
    pub fn if_match(mut self, input_field: &str) -> Self {
        self.if_match = Some(input_field.to_owned());
        self
    }

    /// Success is 204 with no body.
    pub fn no_content(mut self) -> Self {
        self.success = StatusCode::NO_CONTENT;
        self.no_content = true;
        self
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    /// The portable binding, including every status the adapter can return.
    pub(crate) fn binding(&self, context: &ContextDescriptor, input: &ContractDoc) -> HttpBinding {
        let body = if matches!(self.method, Method::GET | Method::DELETE) {
            BodySource::None
        } else {
            BodySource::Input
        };
        let params = HttpBinding::template_params(&self.path);
        let mut errors = vec![400, 500, 503];
        if context.authenticated {
            errors.extend([401, 403]);
        }
        match body {
            // 409: interaction_stale, conflicts and stale domain actions.
            BodySource::Input => errors.extend([409, 413, 415, 422]),
            BodySource::None => {
                if input
                    .fields
                    .iter()
                    .any(|f| !params.contains(&f.name.as_str()))
                {
                    errors.push(422);
                }
            }
        }
        if !params.is_empty() || body == BodySource::None {
            errors.push(404);
        }
        if self.if_match.is_some() {
            errors.extend([412, 428]);
        }
        if self.idempotency_key {
            errors.push(409);
        }
        errors.sort_unstable();
        errors.dedup();
        HttpBinding {
            method: self.method.to_string(),
            path: self.path.clone(),
            body,
            success_status: self.success.as_u16(),
            location: self.location.clone(),
            errors,
            etag: self.etag.clone(),
            if_match: self.if_match.clone(),
            no_content: self.no_content,
            idempotency_key: self.idempotency_key,
        }
    }
}

pub(crate) fn managed<S, O>(
    op: Arc<O>,
    http: &Http,
    binding: HttpBinding,
    accepted_revisions: Arc<OnceLock<Vec<String>>>,
) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    O: Operation,
    O::Context: FromInvocation<S>,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    let binding = Arc::new(binding);
    let descriptor = Arc::new(O::Context::describe());
    let handler = move |State(state): State<S>, request: Request| {
        let op = op.clone();
        let binding = binding.clone();
        let descriptor = descriptor.clone();
        let accepted = accepted_revisions.clone();
        async move {
            let accepted = accepted.get().map(Vec::as_slice).unwrap_or(&[]);
            match handle::<S, O>(&state, &op, &binding, &descriptor, accepted, request).await {
                Ok(response) => response,
                Err(problem) => problem.into_response(),
            }
        }
    };
    let filter =
        MethodFilter::try_from(http.method().clone()).expect("Http only builds standard methods");
    axum::routing::on(filter, handler)
}

/// Text from a URL, converted to the contract field's type where that is
/// unambiguous. Invalid text is left as text so validation reports it.
fn from_text(contract: &ContractDoc, name: &str, text: String) -> Value {
    match contract.get(name).map(|f| &f.ty) {
        Some(FieldType::Integer { .. }) => text
            .parse::<i64>()
            .map(|n| Value::Number(Number::from(n)))
            .unwrap_or(Value::String(text)),
        Some(FieldType::Boolean) => match text.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(text),
        },
        _ => Value::String(text),
    }
}

fn conflicting(name: &str) -> Problem {
    Problem::malformed(
        "conflicting_input",
        &format!("{name} is given more than once (URL, query, body or precondition)."),
    )
}

/// Parses `If-Match: "<representation>:<version>"` for this representation.
fn if_match_version(parts: &Parts, representation: &str) -> Result<String, Problem> {
    let header = parts
        .headers
        .get(header::IF_MATCH)
        .ok_or_else(Problem::precondition_required)?;
    let tag = header
        .to_str()
        .ok()
        .and_then(|v| v.trim().strip_prefix('"')?.strip_suffix('"'))
        .ok_or_else(Problem::precondition_failed)?;
    match tag.rsplit_once(':') {
        Some((rep, version))
            if rep == representation
                && !version.is_empty()
                && version.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Ok(version.to_owned())
        }
        _ => Err(Problem::precondition_failed()),
    }
}

async fn handle<S, O>(
    state: &S,
    op: &O,
    binding: &HttpBinding,
    descriptor: &ContextDescriptor,
    accepted_revisions: &[String],
    request: Request,
) -> Result<Response, Problem>
where
    S: Clone + Send + Sync + 'static,
    O: Operation,
    O::Context: FromInvocation<S>,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    use trestle_protocol::Contract as _;
    let input_contract = O::Input::contract();
    let output_contract = O::Output::contract();
    let (mut parts, body) = request.into_parts();
    // Identity and tenant before anything else is read (§6.2).
    let invocation = Invocation::from_http(&mut parts, state, descriptor).await?;
    let ctx = O::Context::resolve(&invocation, state).await?;
    let key = idempotency_key(&parts, binding)?;
    let (instance, path) = prepare(
        state,
        parts,
        body,
        binding,
        accepted_revisions,
        input_contract,
        &output_contract.id,
        O::INFO.id,
    )
    .await?;

    let input = validate::<O::Input>(&instance).map_err(|p| path_not_found(p, &path))?;

    let pool = PgPool::from_ref(state);
    if let Some(key) = key {
        let invocation = invocation.with_idempotency_key(key.clone());
        let keyed = execute_keyed(&pool, op, &ctx, input, &invocation, &key).await?;
        return keyed_response(binding, &output_contract.id, keyed);
    }
    let output = execute_validated(&pool, op, &ctx, input).await?;
    let status = StatusCode::from_u16(binding.success_status).unwrap_or(StatusCode::OK);
    if binding.no_content {
        return Ok(status.into_response());
    }

    // Serialize the typed output directly so fields keep declaration order;
    // a `Value` is built only when headers need output fields.
    let needs_fields = binding.location.is_some() || binding.etag.is_some();
    let fields = if needs_fields {
        Some(serde_json::to_value(&output).map_err(|err| {
            tracing::error!(operation = O::INFO.id, error = %err, "output does not serialize");
            Problem::internal()
        })?)
    } else {
        None
    };
    let mut response = (status, Json(output)).into_response();
    if let (Some(template), Some(fields)) = (&binding.location, &fields)
        && let Some(location) = fill(template, fields)
    {
        response.headers_mut().insert(header::LOCATION, location);
    }
    if let (Some(field), Some(fields)) = (&binding.etag, &fields)
        && let Some(value) = fields.get(field).and_then(scalar_text)
        && let Ok(etag) = HeaderValue::from_str(&format!("\"{}:{value}\"", output_contract.id))
    {
        response.headers_mut().insert(header::ETAG, etag);
    }
    Ok(response)
}

/// A managed route for a [`DynOperation`]: same adapter, same executor
/// sequence, JSON values instead of Rust types.
pub(crate) fn managed_dyn<S>(
    op: Arc<dyn DynOperation<S>>,
    http: &Http,
    binding: HttpBinding,
    accepted_revisions: Arc<OnceLock<Vec<String>>>,
) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    let binding = Arc::new(binding);
    let descriptor = Arc::new(op.context());
    let handler = move |State(state): State<S>, request: Request| {
        let op = op.clone();
        let binding = binding.clone();
        let descriptor = descriptor.clone();
        let accepted = accepted_revisions.clone();
        async move {
            let accepted = accepted.get().map(Vec::as_slice).unwrap_or(&[]);
            match handle_dyn::<S>(&state, op, &binding, &descriptor, accepted, request).await {
                Ok(response) => response,
                Err(problem) => problem.into_response(),
            }
        }
    };
    let filter =
        MethodFilter::try_from(http.method().clone()).expect("Http only builds standard methods");
    axum::routing::on(filter, handler)
}

async fn handle_dyn<S>(
    state: &S,
    op: Arc<dyn DynOperation<S>>,
    binding: &HttpBinding,
    descriptor: &ContextDescriptor,
    accepted_revisions: &[String],
    request: Request,
) -> Result<Response, Problem>
where
    S: Clone + Send + Sync + 'static,
    PgPool: FromRef<S>,
    Identity: FromRef<S>,
{
    let input_contract = op.input();
    let output_contract = op.output();
    let (mut parts, body) = request.into_parts();
    let invocation = Invocation::from_http(&mut parts, state, descriptor).await?;
    let ctx = op.resolve_context(&invocation, state).await?;
    let key = idempotency_key(&parts, binding)?;
    let (instance, path) = prepare(
        state,
        parts,
        body,
        binding,
        accepted_revisions,
        input_contract,
        &output_contract.id,
        &op.info().id,
    )
    .await?;
    let pool = PgPool::from_ref(state);
    if let Some(key) = key {
        let invocation = invocation.with_idempotency_key(key.clone());
        let keyed = execute_dyn_keyed(&pool, op.clone(), ctx, &instance, &invocation, &key)
            .await
            .map_err(|p| path_not_found(p, &path))?;
        return keyed_response(binding, &output_contract.id, keyed);
    }
    // The contracts above borrow from `op`; the executor gets its own handle.
    let output = execute_dyn(&pool, op.clone(), ctx, &instance)
        .await
        .map_err(|p| path_not_found(p, &path))?;
    let status = StatusCode::from_u16(binding.success_status).unwrap_or(StatusCode::OK);
    if binding.no_content {
        return Ok(status.into_response());
    }
    let mut response = (status, Json(&output)).into_response();
    if let Some(template) = &binding.location
        && let Some(location) = fill(template, &output)
    {
        response.headers_mut().insert(header::LOCATION, location);
    }
    if let Some(field) = &binding.etag
        && let Some(value) = output.get(field).and_then(scalar_text)
        && let Ok(etag) = HeaderValue::from_str(&format!("\"{}:{value}\"", output_contract.id))
    {
        response.headers_mut().insert(header::ETAG, etag);
    }
    Ok(response)
}

/// The request's `Idempotency-Key`, if it sent one. Refused where the
/// binding does not accept a key (ADR-016).
fn idempotency_key(parts: &Parts, binding: &HttpBinding) -> Result<Option<String>, Problem> {
    let Some(value) = parts.headers.get(IDEMPOTENCY_KEY_HEADER) else {
        return Ok(None);
    };
    if !binding.idempotency_key {
        return Err(idempotency::unsupported());
    }
    match value.to_str() {
        Ok(key) if idempotency::valid_key(key) => Ok(Some(key.to_owned())),
        _ => Err(idempotency::invalid()),
    }
}

/// A keyed write's response: the committed body exactly as first sent, with
/// the binding's status, `Location` and `ETag`, and
/// `Idempotency-Replayed: true` when it came from the receipt.
fn keyed_response(
    binding: &HttpBinding,
    output_id: &str,
    keyed: Keyed,
) -> Result<Response, Problem> {
    let status = StatusCode::from_u16(binding.success_status).unwrap_or(StatusCode::OK);
    let mut response = if binding.no_content {
        status.into_response()
    } else {
        (
            status,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            keyed.body.clone(),
        )
            .into_response()
    };
    if keyed.replayed {
        response.headers_mut().insert(
            idempotency::REPLAYED_HEADER,
            HeaderValue::from_static("true"),
        );
    }
    if binding.location.is_some() || binding.etag.is_some() {
        let fields: Value = serde_json::from_str(&keyed.body).map_err(|err| {
            tracing::error!(error = %err, "stored answer is not JSON");
            Problem::internal()
        })?;
        if let Some(template) = &binding.location
            && let Some(location) = fill(template, &fields)
        {
            response.headers_mut().insert(header::LOCATION, location);
        }
        if let Some(field) = &binding.etag
            && let Some(value) = fields.get(field).and_then(scalar_text)
            && let Ok(etag) = HeaderValue::from_str(&format!("\"{output_id}:{value}\""))
        {
            response.headers_mut().insert(header::ETAG, etag);
        }
    }
    Ok(response)
}

/// Revision check and input assembly shared by static and dynamic routes:
/// path parameters + query or body + `If-Match` version.
#[allow(clippy::too_many_arguments)]
async fn prepare<S: Send + Sync>(
    state: &S,
    mut parts: Parts,
    body: axum::body::Body,
    binding: &HttpBinding,
    accepted_revisions: &[String],
    input_contract: &ContractDoc,
    output_id: &str,
    op_id: &str,
) -> Result<(Value, Vec<(String, String)>), Problem> {
    // A generated form names the descriptor revision it rendered. Anything
    // not explicitly accepted is refused before the body is read (§11.4).
    // Plain API clients send no header and are unaffected.
    if let Some(revision) = parts.headers.get(REVISION_HEADER) {
        let known = revision
            .to_str()
            .is_ok_and(|r| accepted_revisions.iter().any(|a| a == r));
        if !known {
            crate::metrics::increment(
                "trestle_interaction_stale_total",
                "Submissions refused for an unaccepted interaction revision.",
                &[("operation", op_id)],
            );
            return Err(Problem::conflict("interaction_stale").with_detail(
                "This form changed after it was loaded. Reload it and review your input before submitting again.",
            ));
        }
    }

    let params = RawPathParams::from_request_parts(&mut parts, state)
        .await
        .map_err(|_| Problem::not_found())?;
    let path: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    let version = match &binding.if_match {
        Some(_) => Some(if_match_version(&parts, output_id)?),
        None => None,
    };

    let mut instance = match binding.body {
        BodySource::Input => {
            let JsonBody(value) =
                JsonBody::from_request(Request::from_parts(parts, body), state).await?;
            value
        }
        BodySource::None => {
            let Query(pairs): Query<Vec<(String, String)>> = Query::try_from_uri(&parts.uri)
                .map_err(|_| {
                    Problem::malformed("invalid_query", "The query string could not be read.")
                })?;
            let mut object = Map::new();
            for (name, text) in pairs {
                if object.contains_key(&name) {
                    return Err(conflicting(&name));
                }
                let value = from_text(input_contract, &name, text);
                object.insert(name, value);
            }
            Value::Object(object)
        }
    };
    if let Value::Object(object) = &mut instance {
        for (name, text) in path.iter().cloned() {
            if object.contains_key(&name) {
                return Err(conflicting(&name));
            }
            let value = from_text(input_contract, &name, text);
            object.insert(name, value);
        }
        if let (Some(field), Some(version)) = (&binding.if_match, version) {
            if object.contains_key(field) {
                return Err(conflicting(field));
            }
            object.insert(field.clone(), Value::String(version));
        }
    }

    Ok((instance, path))
}

/// Maps a validation problem caused by a path parameter to 404: a path that
/// does not satisfy the contract names no resource.
fn path_not_found(problem: Problem, path: &[(String, String)]) -> Problem {
    let from_path = problem.errors().iter().any(|e| {
        path.iter()
            .any(|(name, _)| e.pointer.trim_start_matches('/') == name)
    });
    if from_path {
        Problem::not_found()
    } else {
        problem
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Fills `{field}` segments from output fields, percent-encoding values.
fn fill(template: &str, output: &Value) -> Option<HeaderValue> {
    let mut filled = String::new();
    for (i, segment) in template.split('/').enumerate() {
        if i > 0 {
            filled.push('/');
        }
        match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(field) => {
                let value = scalar_text(output.get(field)?)?;
                for byte in value.bytes() {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        filled.push(byte as char);
                    } else {
                        filled.push_str(&format!("%{byte:02X}"));
                    }
                }
            }
            None => filled.push_str(segment),
        }
    }
    HeaderValue::from_str(&filled).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trestle_protocol::Field;

    #[test]
    fn fills_location_from_output() {
        let output = json!({"id": "01a0dc49-2bf1", "n": 7, "odd": "a b/c"});
        assert_eq!(
            fill("/api/v1/lessons/{id}", &output).unwrap(),
            "/api/v1/lessons/01a0dc49-2bf1"
        );
        assert_eq!(fill("/x/{n}/{odd}", &output).unwrap(), "/x/7/a%20b%2Fc");
        assert!(fill("/x/{missing}", &output).is_none());
    }

    #[test]
    fn binding_lists_adapter_statuses() {
        let authenticated = ContextDescriptor::authenticated();
        let create_input = ContractDoc::input("a.create", "1").field(Field::text("title"));
        let create = Http::post("/api/v1/lessons").created("/api/v1/lessons/{id}");
        let binding = create.binding(&authenticated, &create_input);
        assert_eq!(binding.success_status, 201);
        assert_eq!(
            binding.errors,
            [400, 401, 403, 409, 413, 415, 422, 500, 503]
        );
        let get_input = ContractDoc::input("a.get", "1").field(Field::uuid("id"));
        let get = Http::get("/api/v1/lessons/{id}").binding(&authenticated, &get_input);
        assert_eq!(get.body, BodySource::None);
        assert_eq!(get.errors, [400, 401, 403, 404, 500, 503]);
        let patch = Http::patch("/api/v1/lessons/{id}")
            .if_match("expected_version")
            .binding(&authenticated, &get_input);
        assert!(patch.errors.contains(&412) && patch.errors.contains(&428));
        let delete = Http::delete("/api/v1/lessons/{id}")
            .no_content()
            .binding(&authenticated, &get_input);
        assert_eq!((delete.success_status, delete.no_content), (204, true));
    }

    #[test]
    fn url_text_becomes_typed_where_the_contract_says_so() {
        let contract = ContractDoc::input("a.list", "1")
            .field(Field::integer("grade"))
            .field(Field::boolean("archived"))
            .field(Field::text("q"));
        assert_eq!(from_text(&contract, "grade", "6".into()), json!(6));
        assert_eq!(from_text(&contract, "grade", "six".into()), json!("six"));
        assert_eq!(from_text(&contract, "archived", "true".into()), json!(true));
        assert_eq!(from_text(&contract, "q", "6".into()), json!("6"));
    }
}
