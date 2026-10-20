//! Request body admission and contract decoding (design doc §5).
//!
//! [`JsonBody`] enforces content type, size and a total time limit before
//! parsing ([`read_body`]). [`decode`]
//! then validates against the type's registered contract and deserializes
//! the normalized instance:
//!
//! * wrong JSON type for a field → 400 (the body has the wrong shape)
//! * missing, null, out-of-range or unknown fields → 422 with JSON Pointers

use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Request};
use http::{Extensions, HeaderMap, header};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::de::DeserializeOwned;
use serde_json::Value;
use trestle_protocol::{Contract, Rejection};

use crate::admission::AdmissionHandle;
use crate::problem::Problem;

/// Default request body limit (§8.5): 1 MiB.
pub const BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// Default time a client has to deliver a whole request body. A total
/// deadline, not an idle one: a slow trickle cannot extend it. The request
/// holds an admission permit while its body arrives, so without a deadline
/// stalled uploads could hold every permit.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(10);

/// The body deadline in force for a request: its admission's, else
/// [`BODY_TIMEOUT`].
pub fn body_timeout(extensions: &Extensions) -> Duration {
    extensions
        .get::<AdmissionHandle>()
        .map_or(BODY_TIMEOUT, |AdmissionHandle(admission)| {
            admission.body_timeout()
        })
}

/// Reads at most `limit` bytes of `body` within `timeout`: 413 past the
/// limit, 408 past the deadline, 400 if the body breaks off.
pub async fn read_body(body: Body, limit: usize, timeout: Duration) -> Result<Bytes, Problem> {
    match tokio::time::timeout(timeout, Limited::new(body, limit).collect()).await {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(err)) if err.is::<LengthLimitError>() => Err(Problem::payload_too_large(limit)),
        Ok(Err(_)) => Err(Problem::malformed(
            "body_unreadable",
            "The request body could not be read.",
        )),
        Err(_) => {
            crate::metrics::increment(
                "trestle_admission_rejected_total",
                "Requests refused at admission.",
                &[("reason", "body_timeout")],
            );
            Err(Problem::request_timeout())
        }
    }
}

/// A JSON request body, admitted but not yet decoded into a contract.
#[derive(Debug, Clone)]
pub struct JsonBody(pub Value);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = Problem;

    async fn from_request(request: Request, _state: &S) -> Result<Self, Problem> {
        if !is_json(request.headers()) {
            return Err(Problem::unsupported_media_type("application/json"));
        }
        let declared = request
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        if declared.is_some_and(|len| len > BODY_LIMIT_BYTES) {
            return Err(Problem::payload_too_large(BODY_LIMIT_BYTES));
        }
        let timeout = body_timeout(request.extensions());
        let bytes = read_body(request.into_body(), BODY_LIMIT_BYTES, timeout).await?;
        let value = serde_json::from_slice(&bytes).map_err(|_| {
            Problem::malformed("malformed_json", "The request body is not valid JSON.")
        })?;
        Ok(JsonBody(value))
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// Validates `value` against `T`'s contract and deserializes the normalized
/// instance. The contract is the only validator; serde only maps structure.
pub fn decode<T: Contract + DeserializeOwned>(value: &Value) -> Result<T, Problem> {
    let contract = T::contract();
    let normalized = contract.validate(value).map_err(Problem::from)?;
    serde_json::from_value(normalized).map_err(|err| {
        // Registry checks make this unreachable for registered operations.
        tracing::error!(contract = %contract.id, error = %err, "contract and Rust type disagree");
        Problem::internal()
    })
}

impl From<Rejection> for Problem {
    fn from(rejection: Rejection) -> Self {
        match rejection {
            Rejection::NotObject => {
                Problem::malformed("body_not_object", "The request body must be a JSON object.")
            }
            Rejection::Shape(errors) => Problem::malformed(
                "invalid_body_shape",
                "One or more fields have the wrong JSON type.",
            )
            .with_errors(errors),
            Rejection::Invalid(errors) => Problem::validation(errors),
        }
    }
}
