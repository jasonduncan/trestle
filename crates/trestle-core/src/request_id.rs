//! Per-request correlation ID.
//!
//! The ID is generated server-side, echoed in `x-request-id`, attached to the
//! request's tracing span, and readable anywhere in the request task through
//! [`current`] (problem responses and audit records use it).

use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderName, HeaderValue};
use tracing::Instrument;
use uuid::Uuid;

pub const HEADER: HeaderName = HeaderName::from_static("x-request-id");

tokio::task_local! {
    static CURRENT: String;
}

/// The current request's ID, if called inside a request task.
pub fn current() -> Option<String> {
    CURRENT.try_with(Clone::clone).ok()
}

pub async fn middleware(request: Request, next: Next) -> Response {
    // Time-ordered, but without `now_v7`'s process-wide counter lock: IDs
    // only need to be unique and sortable to the millisecond.
    let id = format!(
        "req-{}",
        Uuid::new_v7(uuid::Timestamp::now(uuid::NoContext)).simple()
    );
    let span = tracing::info_span!(
        "request",
        request_id = %id,
        method = %request.method(),
        path = %request.uri().path(),
    );
    let started = Instant::now();
    let mut response = CURRENT
        .scope(id.clone(), next.run(request))
        .instrument(span.clone())
        .await;
    span.in_scope(|| {
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "request completed"
        );
    });
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(HEADER, value);
    }
    response
}
