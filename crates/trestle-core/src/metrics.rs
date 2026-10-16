//! Operational metrics (design doc §15.3) in Prometheus text format.
//!
//! Labels are bounded: route templates, methods, status classes, operation
//! IDs and problem codes. Actor, tenant and record IDs are never labels.
//! Serve [`render`] on an internal listener, not through the public edge.
//!
//! The hot path allocates nothing and takes no exclusive lock: a series is
//! found under a read lock by a hash of its name and label values, then
//! updated with atomics. Only the first sighting of a series takes the
//! write lock.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    Histogram,
}

struct Series {
    name: &'static str,
    help: &'static str,
    kind: Kind,
    labels: Vec<(&'static str, String)>,
    /// Counter value, or histogram count.
    count: AtomicU64,
    /// Histogram sum in nanoseconds.
    sum_nanos: AtomicU64,
    buckets: [AtomicU64; BUCKETS.len()],
}

impl Series {
    fn matches(&self, name: &str, pairs: &[(&'static str, &str)]) -> bool {
        self.name == name
            && self.labels.len() == pairs.len()
            && self
                .labels
                .iter()
                .zip(pairs)
                .all(|((k, v), (pk, pv))| k == pk && v == pv)
    }
}

type Shelf = HashMap<u64, Vec<Arc<Series>>>;

fn registry() -> &'static RwLock<Shelf> {
    static REGISTRY: OnceLock<RwLock<Shelf>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn key(name: &str, pairs: &[(&'static str, &str)]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    for (k, v) in pairs {
        k.hash(&mut hasher);
        v.hash(&mut hasher);
    }
    hasher.finish()
}

fn series(
    name: &'static str,
    help: &'static str,
    kind: Kind,
    pairs: &[(&'static str, &str)],
) -> Arc<Series> {
    let key = key(name, pairs);
    if let Some(found) = registry()
        .read()
        .expect("metrics registry")
        .get(&key)
        .and_then(|list| list.iter().find(|s| s.matches(name, pairs)))
    {
        return found.clone();
    }
    let mut shelf = registry().write().expect("metrics registry");
    let list = shelf.entry(key).or_default();
    if let Some(found) = list.iter().find(|s| s.matches(name, pairs)) {
        return found.clone();
    }
    let created = Arc::new(Series {
        name,
        help,
        kind,
        labels: pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
        count: AtomicU64::new(0),
        sum_nanos: AtomicU64::new(0),
        buckets: Default::default(),
    });
    list.push(created.clone());
    created
}

pub fn increment(name: &'static str, help: &'static str, pairs: &[(&'static str, &str)]) {
    series(name, help, Kind::Counter, pairs)
        .count
        .fetch_add(1, Ordering::Relaxed);
}

pub fn observe(
    name: &'static str,
    help: &'static str,
    pairs: &[(&'static str, &str)],
    seconds: f64,
) {
    let series = series(name, help, Kind::Histogram, pairs);
    for (bucket, bound) in series.buckets.iter().zip(BUCKETS) {
        if seconds <= bound {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
    }
    series.count.fetch_add(1, Ordering::Relaxed);
    series
        .sum_nanos
        .fetch_add((seconds * 1e9) as u64, Ordering::Relaxed);
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn label_text(labels: &[(&str, String)], extra: Option<(&str, String)>) -> String {
    let mut parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
        .collect();
    if let Some((k, v)) = extra {
        parts.push(format!("{k}=\"{v}\""));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", parts.join(","))
    }
}

/// A value sampled at scrape time: name, help, labels, value.
pub type Gauge<'a> = (&'a str, &'a str, Vec<(&'a str, String)>, f64);

/// Prometheus text exposition of everything recorded, plus `gauges`
/// sampled by the caller at scrape time (e.g. pool sizes).
pub fn render(gauges: &[Gauge<'_>]) -> String {
    let mut all: Vec<Arc<Series>> = registry()
        .read()
        .expect("metrics registry")
        .values()
        .flatten()
        .cloned()
        .collect();
    all.sort_by(|a, b| (a.name, &a.labels).cmp(&(b.name, &b.labels)));
    let mut out = String::new();
    let mut described: Vec<&str> = Vec::new();
    for series in &all {
        let name = series.name;
        let kind = match series.kind {
            Kind::Counter => "counter",
            Kind::Histogram => "histogram",
        };
        if !described.contains(&name) {
            let _ = writeln!(out, "# HELP {name} {}\n# TYPE {name} {kind}", series.help);
            described.push(name);
        }
        let count = series.count.load(Ordering::Relaxed);
        if series.kind == Kind::Counter {
            let _ = writeln!(out, "{name}{} {count}", label_text(&series.labels, None));
            continue;
        }
        for (bucket, bound) in series.buckets.iter().zip(BUCKETS) {
            let _ = writeln!(
                out,
                "{name}_bucket{} {}",
                label_text(&series.labels, Some(("le", bound.to_string()))),
                bucket.load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{} {count}",
            label_text(&series.labels, Some(("le", "+Inf".into())))
        );
        let _ = writeln!(
            out,
            "{name}_sum{} {}",
            label_text(&series.labels, None),
            series.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9
        );
        let _ = writeln!(
            out,
            "{name}_count{} {count}",
            label_text(&series.labels, None)
        );
    }
    for (name, help, labels, value) in gauges {
        if !described.contains(name) {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge");
            described.push(name);
        }
        let _ = writeln!(out, "{name}{} {value}", label_text(labels, None));
    }
    out
}

/// `2xx`, `4xx`, …: bounded status labels.
pub fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_histograms_and_gauges() {
        increment(
            "trestle_test_total",
            "Test counter.",
            &[("route", "/a/{id}")],
        );
        increment(
            "trestle_test_total",
            "Test counter.",
            &[("route", "/a/{id}")],
        );
        increment("trestle_test_total", "Test counter.", &[("route", "/b")]);
        observe(
            "trestle_test_seconds",
            "Test histogram.",
            &[("op", "a.b")],
            0.02,
        );
        let text = render(&[(
            "trestle_test_pool",
            "Pool.",
            vec![("state", "idle".into())],
            3.0,
        )]);
        assert!(text.contains("# TYPE trestle_test_total counter"));
        assert_eq!(
            text.matches("# TYPE trestle_test_total ").count(),
            1,
            "described once"
        );
        assert!(text.contains("trestle_test_total{route=\"/a/{id}\"} 2"));
        assert!(text.contains("trestle_test_total{route=\"/b\"} 1"));
        assert!(text.contains("trestle_test_seconds_bucket{op=\"a.b\",le=\"0.025\"} 1"));
        assert!(text.contains("trestle_test_seconds_bucket{op=\"a.b\",le=\"0.01\"} 0"));
        assert!(text.contains("trestle_test_seconds_count{op=\"a.b\"} 1"));
        assert!(text.contains("trestle_test_pool{state=\"idle\"} 3"));
        assert_eq!(status_class(412), "4xx");
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..1000 {
                        increment("trestle_test_concurrent_total", "Test.", &[("k", "v")]);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(render(&[]).contains("trestle_test_concurrent_total{k=\"v\"} 8000"));
    }
}
