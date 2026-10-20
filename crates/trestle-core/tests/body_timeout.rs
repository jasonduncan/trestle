//! A request body has a total deadline (security review SR-01): a stalled or
//! trickling upload gets 408 and gives its admission permit back, so it
//! cannot hold capacity that other requests need. Real TCP, real server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::{get, post};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use trestle_core::admission::{Admission, AdmissionConfig};
use trestle_core::http::{Perimeter, finish_with};
use trestle_core::input::JsonBody;

const DEADLINE: Duration = Duration::from_millis(400);

async fn serve() -> (SocketAddr, Arc<Admission>) {
    let admission = Admission::new(AdmissionConfig {
        max_in_flight: 1,
        body_timeout: DEADLINE,
        ..Default::default()
    });
    let app = finish_with(
        Router::new()
            .route(
                "/echo",
                post(|JsonBody(v): JsonBody| async move { v.to_string() }),
            )
            .route("/other", get(|| async { "ok" })),
        Perimeter {
            admission: Some(admission.clone()),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (address, admission)
}

/// Sends the head of a POST declaring `length` bytes, then `first`.
async fn start_post(address: SocketAddr, length: usize, first: &[u8]) -> TcpStream {
    let mut client = TcpStream::connect(address).await.unwrap();
    let head = format!(
        "POST /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
    );
    client.write_all(head.as_bytes()).await.unwrap();
    client.write_all(first).await.unwrap();
    client
}

/// Reads until the server closes the connection or 5 s pass.
async fn read_response(client: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn get_other(address: SocketAddr) -> String {
    let mut client = TcpStream::connect(address).await.unwrap();
    client
        .write_all(b"GET /other HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    read_response(&mut client).await
}

async fn wait_in_flight(admission: &Admission, n: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while admission.in_flight() != n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("in-flight never reached {n}"));
}

#[tokio::test]
async fn a_stalled_body_times_out_and_releases_its_permit() {
    let (address, admission) = serve().await;
    let started = Instant::now();
    let mut stalled = start_post(address, 100, b"{").await;
    wait_in_flight(&admission, 1).await;
    assert!(
        get_other(address).await.starts_with("HTTP/1.1 503"),
        "the only permit is held while the body is outstanding"
    );

    let response = read_response(&mut stalled).await;
    let elapsed = started.elapsed();
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    assert!(response.contains("request_timeout"), "{response}");
    assert!(
        response.to_ascii_lowercase().contains("connection: close"),
        "{response}"
    );
    assert!(elapsed >= DEADLINE, "not before the deadline: {elapsed:?}");
    assert!(elapsed < DEADLINE * 5, "promptly after it: {elapsed:?}");

    wait_in_flight(&admission, 0).await;
    assert!(get_other(address).await.starts_with("HTTP/1.1 200"));
}

#[tokio::test]
async fn a_trickle_cannot_extend_the_deadline() {
    let (address, admission) = serve().await;
    let started = Instant::now();
    let mut client = start_post(address, 1000, b"[").await;
    wait_in_flight(&admission, 1).await;
    // One byte every 50 ms would keep an idle timeout alive forever.
    let mut writer_alive = true;
    while writer_alive && started.elapsed() < DEADLINE * 4 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        writer_alive = client.write_all(b" ").await.is_ok();
    }
    let response = read_response(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    wait_in_flight(&admission, 0).await;
}

#[tokio::test]
async fn a_body_within_the_deadline_is_read() {
    let (address, _) = serve().await;
    let mut client = start_post(address, 2, b"{").await;
    tokio::time::sleep(DEADLINE / 4).await;
    client.write_all(b"}").await.unwrap();
    let response = read_response(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("{}"), "{response}");
}
