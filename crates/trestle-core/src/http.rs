//! Router finishing: the perimeter every Trestle service shares.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{HeaderValue, header};
use tower_http::catch_panic::CatchPanicLayer;

use crate::admission::{self, Admission};
use crate::{Problem, metrics, request_id};

/// Optional perimeter controls for serving (tests usually omit them).
#[derive(Clone, Default)]
pub struct Perimeter {
    pub admission: Option<Arc<Admission>>,
}

/// Adds problem+json fallbacks, per-route metrics, default security
/// headers, panic containment and request IDs. Call last, after all routes
/// and state are attached.
pub fn finish(router: Router) -> Router {
    finish_with(router, Perimeter::default())
}

pub fn finish_with(router: Router, perimeter: Perimeter) -> Router {
    let router = router
        .route_layer(axum::middleware::from_fn(route_metrics))
        .fallback(|| async { Problem::not_found() })
        .method_not_allowed_fallback(|| async { Problem::method_not_allowed() })
        .layer(axum::middleware::from_fn(security_headers))
        .layer(CatchPanicLayer::custom(panic_response));
    let router = match perimeter.admission {
        Some(admission) => router.layer(axum::middleware::from_fn_with_state(
            admission,
            admission::middleware,
        )),
        None => router,
    };
    router.layer(axum::middleware::from_fn(request_id::middleware))
}

/// Metrics by route template (runs after routing, so the template is known).
async fn route_metrics(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let method = request.method().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    let status = metrics::status_class(response.status().as_u16());
    let labels = [
        ("method", method.as_str()),
        ("route", route.as_str()),
        ("status", status),
    ];
    metrics::increment(
        "trestle_http_requests_total",
        "HTTP requests by route and status class.",
        &labels,
    );
    metrics::observe(
        "trestle_http_request_duration_seconds",
        "HTTP request latency by route.",
        &labels[..2],
        started.elapsed().as_secs_f64(),
    );
    response
}

/// API responses are personal by default: never cached by shared caches,
/// never sniffed, never leaking the referrer. Routes may set their own
/// `Cache-Control`; nothing here overrides it.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("private, no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn panic_response(_payload: Box<dyn Any + Send + 'static>) -> Response {
    // The payload may contain request data; log only the fact of the panic.
    tracing::error!("request handler panicked");
    Problem::internal().into_response()
}
