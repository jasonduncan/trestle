//! Load comparison for M5 (design doc §19.3).
//!
//! * `loadtest baseline` serves the hand-written Axum baseline.
//! * `loadtest compare` drives the same closed-loop workload against the
//!   pilot on Trestle and the baseline, alternating targets across rounds,
//!   and writes latency percentiles, throughput and the p95 overhead.
//!
//! Both servers use the same database, pool size and statement timeout; the
//! load generator runs on the same machine, which the report states.

#![forbid(unsafe_code)]

mod baseline;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

const NORTHSIDE: &str = "11111111-1111-4111-8111-111111111111";

#[derive(Parser)]
struct Cli {
    #[arg(long, env = "DATABASE_URL", global = true, hide_env_values = true)]
    database_url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the hand-written baseline.
    Baseline {
        #[arg(long, default_value = "127.0.0.1:18202")]
        bind: SocketAddr,
        #[arg(long, env = "TRESTLE_DEV_TOKENS", hide_env_values = true)]
        dev_tokens: String,
        #[arg(long, default_value_t = 10)]
        pool_size: u32,
        #[arg(long, default_value_t = 10_000)]
        statement_timeout_ms: u64,
    },
    /// Run the workload against both targets and write a JSON report.
    Compare {
        #[arg(long)]
        trestle: String,
        #[arg(long)]
        baseline: String,
        #[arg(long, value_delimiter = ',', default_value = "get,list,create")]
        scenarios: Vec<Scenario>,
        #[arg(long, value_delimiter = ',', default_value = "8,32")]
        concurrency: Vec<usize>,
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Per-run warm-up before measuring.
        #[arg(long, default_value_t = 3)]
        warmup: u64,
        /// Before round 1, every scenario on both targets (caches, pools,
        /// CPU frequency), so the first measured run is not a cold start.
        #[arg(long, default_value_t = 10)]
        prewarm: u64,
        #[arg(long, default_value_t = 3)]
        rounds: usize,
        #[arg(long)]
        out: std::path::PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Scenario {
    /// GET /api/v1/lessons/{id}: auth, membership, one scoped read.
    Get,
    /// GET /api/v1/lessons: auth, membership, first page of 20 with cursor.
    List,
    /// POST /api/v1/lessons: auth, membership, validation, transaction with
    /// role recheck, insert and audit row.
    Create,
}

#[derive(Debug, Clone, Serialize)]
struct Run {
    target: &'static str,
    scenario: Scenario,
    concurrency: usize,
    round: usize,
    seconds: f64,
    requests: u64,
    errors: u64,
    throughput_rps: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    max_ms: f64,
}

fn percentile(sorted: &[u32], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    f64::from(sorted[rank.clamp(1, sorted.len()) - 1]) / 1000.0
}

struct Workload {
    client: reqwest::Client,
    ids: Arc<Vec<Uuid>>,
    counter: AtomicU64,
    tag: String,
}

impl Workload {
    async fn one(&self, base: &str, scenario: Scenario) -> Result<(), String> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let request = match scenario {
            Scenario::Get => {
                let id = self.ids[(n as usize) % self.ids.len()];
                self.client.get(format!("{base}/api/v1/lessons/{id}"))
            }
            Scenario::List => self.client.get(format!("{base}/api/v1/lessons")),
            Scenario::Create => self
                .client
                .post(format!("{base}/api/v1/lessons"))
                .header("content-type", "application/json")
                .body(json!({ "title": format!("Load {} {n}", self.tag), "grade_level": 1 + (n % 12) }).to_string()),
        };
        let response = request
            .header("authorization", "Bearer alice-dev-token")
            .header("trestle-tenant", NORTHSIDE)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let _ = response.bytes().await.map_err(|e| e.to_string())?;
        let expected = if scenario == Scenario::Create {
            201
        } else {
            200
        };
        if status == expected {
            Ok(())
        } else {
            Err(format!("status {status}"))
        }
    }
}

/// Closed loop: `concurrency` workers, each sending the next request when
/// the previous one completes, for `duration`.
async fn drive(
    workload: Arc<Workload>,
    base: &str,
    scenario: Scenario,
    concurrency: usize,
    duration: Duration,
) -> (Vec<u32>, u64, f64) {
    let deadline = Instant::now() + duration;
    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..concurrency {
        let workload = workload.clone();
        let base = base.to_owned();
        tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::new();
            let mut errors = 0u64;
            while Instant::now() < deadline {
                let t = Instant::now();
                match workload.one(&base, scenario).await {
                    Ok(()) => {
                        latencies.push(t.elapsed().as_micros().min(u128::from(u32::MAX)) as u32)
                    }
                    Err(_) => errors += 1,
                }
            }
            (latencies, errors)
        }));
    }
    let mut all = Vec::new();
    let mut errors = 0;
    for task in tasks {
        let (latencies, e) = task.await.expect("worker");
        all.extend(latencies);
        errors += e;
    }
    (all, errors, started.elapsed().as_secs_f64())
}

#[allow(clippy::too_many_arguments)] // mirrors the command-line flags one to one
async fn compare(
    database_url: &str,
    targets: [(&'static str, String); 2],
    scenarios: Vec<Scenario>,
    concurrency: Vec<usize>,
    seconds: u64,
    warmup: u64,
    prewarm: u64,
    rounds: usize,
) -> Result<Vec<Run>, String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(database_url)
        .await
        .map_err(|e| e.to_string())?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM lessons WHERE school_id = $1::uuid ORDER BY id LIMIT 1000",
    )
    .bind(NORTHSIDE)
    .fetch_all(&pool)
    .await
    .map_err(|e| e.to_string())?;
    if ids.len() < 100 {
        return Err(format!("expected seeded lessons, found {}", ids.len()));
    }
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(256)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let ids = Arc::new(ids);
    let max_concurrency = concurrency.iter().copied().max().unwrap_or(8);
    for &scenario in &scenarios {
        for (name, base) in &targets {
            let workload = Arc::new(Workload {
                client: client.clone(),
                ids: ids.clone(),
                counter: AtomicU64::new(0),
                tag: format!("{name}-prewarm-{}", Uuid::new_v4().simple()),
            });
            drive(
                workload,
                base,
                scenario,
                max_concurrency,
                Duration::from_secs(prewarm),
            )
            .await;
        }
    }
    eprintln!("prewarmed every scenario on both targets for {prewarm}s each");
    let mut runs = Vec::new();
    for round in 1..=rounds {
        for &scenario in &scenarios {
            for &c in &concurrency {
                // Alternate which target goes first each round.
                let order: Vec<&(&'static str, String)> = if round % 2 == 1 {
                    targets.iter().collect()
                } else {
                    targets.iter().rev().collect()
                };
                for (name, base) in order {
                    let workload = Arc::new(Workload {
                        client: client.clone(),
                        ids: ids.clone(),
                        counter: AtomicU64::new(0),
                        tag: format!("{name}-r{round}-c{c}-{}", Uuid::new_v4().simple()),
                    });
                    drive(
                        workload.clone(),
                        base,
                        scenario,
                        c,
                        Duration::from_secs(warmup),
                    )
                    .await;
                    let (mut latencies, errors, elapsed) =
                        drive(workload, base, scenario, c, Duration::from_secs(seconds)).await;
                    latencies.sort_unstable();
                    let run = Run {
                        target: name,
                        scenario,
                        concurrency: c,
                        round,
                        seconds: elapsed,
                        requests: latencies.len() as u64,
                        errors,
                        throughput_rps: latencies.len() as f64 / elapsed,
                        p50_ms: percentile(&latencies, 50.0),
                        p95_ms: percentile(&latencies, 95.0),
                        p99_ms: percentile(&latencies, 99.0),
                        max_ms: latencies.last().map_or(0.0, |m| f64::from(*m) / 1000.0),
                    };
                    eprintln!(
                        "{:<8} {:?} c={c:<3} r{round}: {:>8.0} req/s  p50 {:>6.2}  p95 {:>6.2}  p99 {:>6.2} ms  errors {}",
                        run.target,
                        run.scenario,
                        run.throughput_rps,
                        run.p50_ms,
                        run.p95_ms,
                        run.p99_ms,
                        run.errors
                    );
                    runs.push(run);
                }
            }
        }
    }
    Ok(runs)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

fn summarize(runs: &[Run]) -> Vec<Value> {
    let mut keys: Vec<(Scenario, usize)> =
        runs.iter().map(|r| (r.scenario, r.concurrency)).collect();
    keys.dedup();
    keys.sort_by_key(|(s, c)| (format!("{s:?}"), *c));
    keys.dedup();
    keys.into_iter()
        .map(|(scenario, c)| {
            let of = |target: &str, f: fn(&Run) -> f64| {
                median(
                    runs.iter()
                        .filter(|r| {
                            r.target == target && r.scenario == scenario && r.concurrency == c
                        })
                        .map(f)
                        .collect(),
                )
            };
            let (tp95, bp95) = (of("trestle", |r| r.p95_ms), of("baseline", |r| r.p95_ms));
            let (tp50, bp50) = (of("trestle", |r| r.p50_ms), of("baseline", |r| r.p50_ms));
            let (trps, brps) = (
                of("trestle", |r| r.throughput_rps),
                of("baseline", |r| r.throughput_rps),
            );
            let round2 = |v: f64| (v * 100.0).round() / 100.0;
            json!({
                "scenario": scenario,
                "concurrency": c,
                "p50_ms": { "trestle": round2(tp50), "baseline": round2(bp50) },
                "p95_ms": { "trestle": round2(tp95), "baseline": round2(bp95) },
                "throughput_rps": { "trestle": trps.round(), "baseline": brps.round() },
                "p95_overhead_percent": round2((tp95 - bp95) / bp95 * 100.0),
                "within_provisional_20_percent_target": tp95 <= bp95 * 1.2,
            })
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let database_url = cli.database_url.ok_or("DATABASE_URL is not set")?;
    match cli.command {
        Command::Baseline {
            bind,
            dev_tokens,
            pool_size,
            statement_timeout_ms,
        } => {
            let options: PgConnectOptions = database_url.parse().map_err(|e| format!("{e}"))?;
            let options =
                options.options([("statement_timeout", format!("{statement_timeout_ms}ms"))]);
            let pool = PgPoolOptions::new()
                .max_connections(pool_size)
                .acquire_timeout(Duration::from_secs(5))
                .connect_with(options)
                .await
                .map_err(|e| e.to_string())?;
            let app = baseline::Baseline::new(pool, &dev_tokens).router();
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .map_err(|e| e.to_string())?;
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let mut term =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("signal handler");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {},
                        _ = term.recv() => {},
                    }
                })
                .await
                .map_err(|e| e.to_string())
        }
        Command::Compare {
            trestle,
            baseline,
            scenarios,
            concurrency,
            seconds,
            warmup,
            prewarm,
            rounds,
            out,
        } => {
            let runs = compare(
                &database_url,
                [("trestle", trestle), ("baseline", baseline)],
                scenarios,
                concurrency,
                seconds,
                warmup,
                prewarm,
                rounds,
            )
            .await?;
            // A target that rejects requests is not slow or fast, it is
            // broken: never publish percentiles of the few that got through.
            let failed: Vec<String> = runs
                .iter()
                .filter(|r| r.errors > 0)
                .map(|r| {
                    format!(
                        "{} {:?} c={} r{}: {} failed requests",
                        r.target, r.scenario, r.concurrency, r.round, r.errors
                    )
                })
                .collect();
            if !failed.is_empty() {
                return Err(format!(
                    "{} run(s) had failed requests; no report written:\n  {}",
                    failed.len(),
                    failed.join("\n  ")
                ));
            }
            let report = json!({
                "summary": summarize(&runs),
                "runs": runs,
            });
            std::fs::write(
                &out,
                format!("{}\n", serde_json::to_string_pretty(&report).expect("json")),
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}
