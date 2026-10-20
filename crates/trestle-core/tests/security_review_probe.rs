// Temporary reproductions for the 2026-10-20 review; asserts observed behavior.
use axum::{Router, body::Body, routing::{get, post}};
use http::{Request, StatusCode};
use tower::ServiceExt;
use trestle_core::{admission::{Admission, AdmissionConfig}, http::{finish, finish_with, Perimeter}, input::JsonBody};

#[tokio::test]
async fn arbitrary_methods_create_persistent_metric_series() {
    let app = finish(Router::new().route("/review-metrics", get(|| async { "ok" })));
    for n in 0..32 {
        let method = format!("REVIEW{n}");
        let response = app.clone().oneshot(Request::builder()
            .method(method.as_str()).uri("/review-metrics").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    let rendered = trestle_core::metrics::render(&[]);
    let count = rendered.lines().filter(|line| line.starts_with("trestle_http_requests_total{method=\"REVIEW")).count();
    println!("Distinct attacker-selected method counter series: {count}");
    assert_eq!(count, 32);
}

#[tokio::test]
async fn incomplete_body_holds_the_admission_permit() {
    use tokio::io::AsyncWriteExt;
    use std::time::Duration;
    let admission = Admission::new(AdmissionConfig { max_in_flight: 1, ..Default::default() });
    let app = finish_with(
        Router::new().route("/review-body", post(|JsonBody(_): JsonBody| async { "ok" }))
            .route("/review-other", get(|| async { "ok" })),
        Perimeter { admission: Some(admission.clone()) },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_app = app.clone();
    let server = tokio::spawn(async move { axum::serve(listener, server_app).await.unwrap() });
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client.write_all(b"POST /review-body HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while admission.in_flight() != 1 { tokio::task::yield_now().await; }
    }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let response = app.oneshot(Request::builder().uri("/review-other").body(Body::empty()).unwrap()).await.unwrap();
    println!("Unrelated request while body stalls: {}", response.status());
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(client);
    server.abort();
}
