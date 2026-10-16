//! Admission control and rate limits at the perimeter (design doc §12.3,
//! §15.3): bounded in-flight work and per-credential token buckets.
//!
//! * Over `max_in_flight` concurrent requests → 503 `overloaded`.
//! * Over a key's rate → 429 `rate_limited`, both with `Retry-After`.
//!
//! Two limits apply. At admission, before anything is verified, every
//! request is charged to its client address (a larger bucket, since many
//! people can share one address). Once identity is verified, the request
//! is also charged to its actor ([`charge_actor`], called by the
//! `Authenticated` extractor). An unverified credential never gets a bucket
//! of its own, so rotating made-up tokens does not escape the limit. Health
//! endpoints are exempt. Buckets are bounded in number, expire, and are
//! sharded so concurrent requests rarely contend on one lock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::{Problem, metrics};

#[derive(Debug, Clone)]
pub struct AdmissionConfig {
    pub max_in_flight: usize,
    /// Sustained requests per second per key.
    pub rate_per_second: f64,
    /// Requests allowed in a burst per key.
    pub burst: f64,
    /// Sustained requests per second per client address, before identity
    /// is known (unauthenticated traffic). Default 20.
    pub address_rate_per_second: f64,
    /// Burst per client address. Default 60.
    pub address_burst: f64,
    /// Header set by the trusted edge with the client address (e.g.
    /// `X-Real-IP`). Only used when configured.
    pub client_address_header: Option<String>,
}

/// Default unauthenticated requests per second per client address.
pub const DEFAULT_ADDRESS_RATE_PER_SECOND: f64 = 20.0;
/// Default unauthenticated burst per client address.
pub const DEFAULT_ADDRESS_BURST: f64 = 60.0;

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 256,
            rate_per_second: 20.0,
            burst: 40.0,
            address_rate_per_second: DEFAULT_ADDRESS_RATE_PER_SECOND,
            address_burst: DEFAULT_ADDRESS_BURST,
            client_address_header: None,
        }
    }
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

const SHARDS: usize = 16;
const MAX_KEYS_PER_SHARD: usize = 50_000 / SHARDS;
const EXEMPT: &[&str] = &["/healthz", "/readyz"];

/// Token buckets keyed by a digest, bounded in number and sharded so
/// concurrent requests rarely contend on one lock. In memory, per process.
pub struct RateLimiter {
    rate_per_second: f64,
    burst: f64,
    shards: [Mutex<HashMap<[u8; 16], Bucket>>; SHARDS],
}

impl RateLimiter {
    pub fn new(rate_per_second: f64, burst: f64) -> Self {
        Self {
            rate_per_second,
            burst,
            shards: Default::default(),
        }
    }

    /// Takes one token for `key` (any bytes; only a digest is kept).
    /// `Err(seconds until a token)` when the bucket is empty.
    pub fn take(&self, key: &[u8]) -> Result<(), u64> {
        let hash = Sha256::digest(key);
        let mut digest = [0u8; 16];
        digest.copy_from_slice(&hash[..16]);
        self.take_digest(digest)
    }

    #[cfg(test)]
    fn keys(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().len()).sum()
    }

    fn take_digest(&self, key: [u8; 16]) -> Result<(), u64> {
        let now = Instant::now();
        let mut buckets = self.shards[usize::from(key[0]) % SHARDS]
            .lock()
            .expect("rate buckets");
        if buckets.len() >= MAX_KEYS_PER_SHARD {
            let idle = Duration::from_secs_f64(self.burst / self.rate_per_second.max(0.001));
            buckets.retain(|_, b| now.duration_since(b.updated) < idle);
        }
        // Still full of live keys: evict the least recently used one, so
        // memory stays bounded whatever the traffic.
        if buckets.len() >= MAX_KEYS_PER_SHARD && !buckets.contains_key(&key) {
            let oldest = buckets
                .iter()
                .min_by_key(|(_, b)| b.updated)
                .map(|(k, _)| *k);
            if let Some(oldest) = oldest {
                buckets.remove(&oldest);
            }
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_second).min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / self.rate_per_second.max(0.001);
            Err(wait.ceil().max(1.0) as u64)
        }
    }
}

/// The client's address: the trusted edge's header when configured and it
/// holds a single IP address, otherwise the socket peer. Empty when neither is known (tests).
pub fn client_address(
    headers: &http::HeaderMap,
    extensions: &http::Extensions,
    header_name: Option<&str>,
) -> Vec<u8> {
    header_name
        .and_then(|name| headers.get(name))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string().into_bytes())
        .unwrap_or_else(|| {
            extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| addr.ip().to_string().into_bytes())
                .unwrap_or_default()
        })
}

pub struct Admission {
    config: AdmissionConfig,
    permits: Arc<Semaphore>,
    limiter: RateLimiter,
    addresses: RateLimiter,
}

/// The admission in force for a request, left in its extensions so the
/// identity extractor can charge the verified actor.
#[derive(Clone)]
pub struct AdmissionHandle(pub Arc<Admission>);

/// Charges one request to `actor` if admission control is on. Call only
/// after the actor is verified.
pub fn charge_actor(extensions: &http::Extensions, actor: uuid::Uuid) -> Result<(), Problem> {
    let Some(AdmissionHandle(admission)) = extensions.get::<AdmissionHandle>() else {
        return Ok(());
    };
    let mut key = b"actor:".to_vec();
    key.extend_from_slice(actor.as_bytes());
    admission.limiter.take(&key).map_err(|retry_after| {
        metrics::increment(
            "trestle_admission_rejected_total",
            "Requests refused at admission.",
            &[("reason", "rate_limited")],
        );
        Problem::rate_limited(retry_after)
    })
}

impl Admission {
    pub fn new(config: AdmissionConfig) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(config.max_in_flight)),
            limiter: RateLimiter::new(config.rate_per_second, config.burst),
            addresses: RateLimiter::new(config.address_rate_per_second, config.address_burst),
            config,
        })
    }

    pub fn in_flight(&self) -> usize {
        self.config.max_in_flight - self.permits.available_permits()
    }

    fn key(&self, request: &Request) -> Vec<u8> {
        let mut key = b"address:".to_vec();
        key.extend_from_slice(&client_address(
            request.headers(),
            request.extensions(),
            self.config.client_address_header.as_deref(),
        ));
        key
    }
}

pub async fn middleware(
    State(admission): State<Arc<Admission>>,
    request: Request,
    next: Next,
) -> Response {
    if EXEMPT.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let Ok(permit) = admission.permits.clone().try_acquire_owned() else {
        metrics::increment(
            "trestle_admission_rejected_total",
            "Requests refused at admission.",
            &[("reason", "overloaded")],
        );
        return Problem::overloaded().into_response();
    };
    if let Err(retry_after) = admission.addresses.take(&admission.key(&request)) {
        metrics::increment(
            "trestle_admission_rejected_total",
            "Requests refused at admission.",
            &[("reason", "rate_limited")],
        );
        drop(permit);
        return Problem::rate_limited(retry_after).into_response();
    }
    let mut request = request;
    request
        .extensions_mut()
        .insert(AdmissionHandle(admission.clone()));
    let response = next.run(request).await;
    drop(permit);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_empty_refill_and_stay_independent() {
        let slow = RateLimiter::new(0.01, 2.0);
        let key = b"credential-a";
        assert!(slow.take(key).is_ok());
        assert!(slow.take(key).is_ok());
        assert_eq!(
            slow.take(key),
            Err(100),
            "empty: wait for one token at 0.01/s"
        );
        assert!(slow.take(b"credential-b").is_ok(), "keys are independent");

        let fast = RateLimiter::new(1000.0, 1.0);
        assert!(fast.take(key).is_ok());
        std::thread::sleep(Duration::from_millis(5));
        assert!(fast.take(key).is_ok(), "refilled");
    }

    #[test]
    fn the_number_of_keys_is_capped_even_when_all_are_fresh() {
        let limiter = RateLimiter::new(1.0, 40.0);
        for i in 0..60_000u32 {
            let _ = limiter.take(&i.to_be_bytes());
        }
        assert!(
            limiter.keys() <= MAX_KEYS_PER_SHARD * SHARDS,
            "{} keys kept",
            limiter.keys()
        );
    }

    #[test]
    fn the_address_header_must_parse_as_an_ip_address() {
        let mut extensions = http::Extensions::new();
        extensions.insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 4000))));
        let with = |value: &str| {
            let mut headers = http::HeaderMap::new();
            headers.insert("x-real-ip", value.parse().unwrap());
            client_address(&headers, &extensions, Some("x-real-ip"))
        };
        assert_eq!(with(" 192.0.2.7 "), b"192.0.2.7");
        assert_eq!(with("2001:db8::1"), b"2001:db8::1");
        for junk in [
            "not-an-address",
            "192.0.2.7, 198.51.100.1",
            "",
            "192.0.2.7:80",
        ] {
            assert_eq!(with(junk), b"10.0.0.1", "{junk:?} falls back to the peer");
        }
    }

    #[test]
    fn unauthenticated_defaults_are_explicit_and_independent_of_key_limits() {
        let config = AdmissionConfig {
            rate_per_second: 1.0,
            burst: 1.0,
            ..AdmissionConfig::default()
        };
        assert_eq!(config.address_rate_per_second, 20.0);
        assert_eq!(config.address_burst, 60.0);
        let admission = Admission::new(config);
        let key = b"address:192.0.2.7";
        for i in 0..60 {
            assert!(
                admission.addresses.take(key).is_ok(),
                "request {i} within burst"
            );
        }
        assert!(
            admission.addresses.take(key).is_err(),
            "61st request refused"
        );
        assert!(
            admission.addresses.take(b"address:192.0.2.8").is_ok(),
            "other addresses unaffected"
        );
    }
}
