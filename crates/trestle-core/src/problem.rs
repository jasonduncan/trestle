//! RFC 9457 problem details with Trestle's extensions (design doc §7.2).
//!
//! `type` names a stable category, `code` is the stable machine-readable
//! reason, and `errors` carries field errors addressed by JSON Pointer.
//! Nothing here may carry SQL, secrets or stack traces.

use axum::response::{IntoResponse, Response};
use http::{HeaderName, HeaderValue, StatusCode, header};
use serde::Serialize;
pub use trestle_protocol::{FieldError, pointer};

use crate::request_id;

pub const PROBLEM_JSON: &str = "application/problem+json";

/// Boxed so `Result<T, Problem>` stays small; errors are the cold path.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct Problem(Box<Body>);

#[derive(Debug, Clone, Serialize)]
struct Body {
    #[serde(rename = "type")]
    kind: &'static str,
    title: &'static str,
    status: u16,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<FieldError>,
    #[serde(skip)]
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl Problem {
    fn new(status: StatusCode, kind: &'static str, title: &'static str, code: &str) -> Self {
        Self(Box::new(Body {
            kind,
            title,
            status: status.as_u16(),
            code: code.to_owned(),
            detail: None,
            request_id: None,
            errors: Vec::new(),
            headers: Vec::new(),
        }))
    }

    /// 400: the request could not be read as the expected shape. No operation ran.
    pub fn malformed(code: &str, detail: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "urn:trestle:problem:malformed-request",
            "Malformed request",
            code,
        )
        .with_detail(detail)
    }

    /// 422: the input violates the contract or a business rule.
    pub fn validation(errors: Vec<FieldError>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "urn:trestle:problem:validation",
            "Validation failed",
            "validation_failed",
        )
        .with_errors(errors)
    }

    /// 401 with a Bearer challenge.
    pub fn unauthenticated(code: &str) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "urn:trestle:problem:authentication",
            "Authentication required",
            code,
        )
        .with_header(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))
    }

    pub fn forbidden(code: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "urn:trestle:problem:forbidden",
            "Forbidden",
            code,
        )
    }

    pub fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "urn:trestle:problem:not-found",
            "Not found",
            "not_found",
        )
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "urn:trestle:problem:method-not-allowed",
            "Method not allowed",
            "method_not_allowed",
        )
    }

    pub fn conflict(code: &str) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "urn:trestle:problem:conflict",
            "Conflict",
            code,
        )
    }

    /// 428: this operation requires `If-Match` (§6.4).
    pub fn precondition_required() -> Self {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            "urn:trestle:problem:precondition-required",
            "Precondition required",
            "precondition_required",
        )
        .with_detail("Send If-Match with the ETag of the version you are changing.")
    }

    /// 412: the `If-Match` tag does not match the current representation.
    pub fn precondition_failed() -> Self {
        Self::new(
            StatusCode::PRECONDITION_FAILED,
            "urn:trestle:problem:precondition-failed",
            "Precondition failed",
            "precondition_failed",
        )
        .with_detail("The resource changed since you read it. Reload it and try again.")
    }

    pub fn payload_too_large(limit_bytes: usize) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "urn:trestle:problem:payload-too-large",
            "Payload too large",
            "payload_too_large",
        )
        .with_detail(&format!(
            "Request bodies are limited to {limit_bytes} bytes."
        ))
    }

    /// 408 when the request body did not arrive in time. The connection is
    /// closed: the rest of the body may still be on its way.
    pub fn request_timeout() -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "urn:trestle:problem:request-timeout",
            "Request timeout",
            "request_timeout",
        )
        .with_detail("The request body did not arrive in time.")
        .with_header(header::CONNECTION, HeaderValue::from_static("close"))
    }

    pub fn unsupported_media_type(expected: &str) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "urn:trestle:problem:unsupported-media-type",
            "Unsupported media type",
            "unsupported_media_type",
        )
        .with_detail(&format!("Send the request body as {expected}."))
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "urn:trestle:problem:internal",
            "Internal error",
            "internal_error",
        )
    }

    /// 500 when a commit's outcome is unknown (I12, §6.4). The client must
    /// look the result up instead of blindly repeating the command.
    pub fn outcome_unknown() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "urn:trestle:problem:outcome-unknown",
            "Outcome unknown",
            "outcome_unknown",
        )
        .with_detail("The request may or may not have taken effect. Check before retrying.")
    }

    /// 429 with `Retry-After` (§7.2).
    pub fn rate_limited(retry_after_seconds: u64) -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "urn:trestle:problem:rate-limited",
            "Too many requests",
            "rate_limited",
        )
        .with_detail("Slow down and try again shortly.")
        .with_header(
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry_after_seconds.to_string())
                .unwrap_or(HeaderValue::from_static("1")),
        )
    }

    /// 503 when admission control refuses new work (§15.3).
    pub fn overloaded() -> Self {
        Self::unavailable("overloaded")
            .with_detail("The service is busy. Try again shortly.")
            .with_header(header::RETRY_AFTER, HeaderValue::from_static("1"))
    }

    pub fn unavailable(code: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "urn:trestle:problem:unavailable",
            "Service unavailable",
            code,
        )
    }

    pub fn with_detail(mut self, detail: &str) -> Self {
        self.0.detail = Some(detail.to_owned());
        self
    }

    pub fn with_errors(mut self, errors: Vec<FieldError>) -> Self {
        self.0.errors = errors;
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.0.headers.push((name, value));
        self
    }

    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    pub fn code(&self) -> &str {
        &self.0.code
    }

    pub fn errors(&self) -> &[FieldError] {
        &self.0.errors
    }
}

impl IntoResponse for Problem {
    fn into_response(mut self) -> Response {
        if self.0.request_id.is_none() {
            self.0.request_id = request_id::current();
        }
        let status = self.status();
        let headers = std::mem::take(&mut self.0.headers);
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| br#"{"status":500}"#.to_vec());
        let mut response = (
            status,
            [(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON))],
            body,
        )
            .into_response();
        for (name, value) in headers {
            response.headers_mut().append(name, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_escapes_reserved_characters() {
        assert_eq!(pointer("title"), "/title");
        assert_eq!(pointer("a/b~c"), "/a~1b~0c");
    }

    #[test]
    fn serializes_problem_shape() {
        let problem = Problem::validation(vec![
            FieldError::new("/title", "max_length", "Use 200 characters or fewer.")
                .with_params(serde_json::json!({"max": 200})),
        ]);
        let value = serde_json::to_value(&problem).unwrap();
        assert_eq!(value["type"], "urn:trestle:problem:validation");
        assert_eq!(value["status"], 422);
        assert_eq!(value["code"], "validation_failed");
        assert_eq!(value["errors"][0]["pointer"], "/title");
        assert_eq!(value["errors"][0]["params"]["max"], 200);
        assert!(value.get("detail").is_none());
    }
}
