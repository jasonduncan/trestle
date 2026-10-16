//! Request body admission and contract decoding (design doc §5).
//!
//! [`JsonBody`] enforces content type and size before parsing. [`decode`]
//! then validates against the type's registered contract and deserializes
//! the normalized instance:
//!
//! * wrong JSON type for a field → 400 (the body has the wrong shape)
//! * missing, null, out-of-range or unknown fields → 422 with JSON Pointers

use axum::extract::{FromRequest, Request};
use http::{HeaderMap, header};
use http_body_util::{BodyExt, LengthLimitError, Limited};
use serde::de::DeserializeOwned;
use serde_json::Value;
use trestle_protocol::{Contract, Rejection};

use crate::problem::Problem;

/// Default request body limit (§8.5): 1 MiB.
pub const BODY_LIMIT_BYTES: usize = 1024 * 1024;

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
        let bytes = match Limited::new(request.into_body(), BODY_LIMIT_BYTES)
            .collect()
            .await
        {
            Ok(collected) => collected.to_bytes(),
            Err(err) if err.is::<LengthLimitError>() => {
                return Err(Problem::payload_too_large(BODY_LIMIT_BYTES));
            }
            Err(_) => {
                return Err(Problem::malformed(
                    "body_unreadable",
                    "The request body could not be read.",
                ));
            }
        };
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
