//! Serving: configuration, production boot checks, identity selection and
//! the public and internal listeners.
//!
//! Production refuses to boot rather than run degraded (§12.4, §16.2):
//! development tokens, a missing identity provider, missing keys or a
//! database connection without TLS are configuration errors, reported
//! together.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use http::header;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use trestle_core::admission::{Admission, AdmissionConfig};
use trestle_core::http::Perimeter;
use trestle_core::identity::{DevTokenIdentity, Identity, IdentityAdapter};
use trestle_core::jobs::{Worker, WorkerConfig};
use trestle_core::registry::Registry;
use trestle_core::{metrics, pagination};
use trestle_identity::bearer::BearerIdentity;
use trestle_identity::oidc::{Provider, Transport, check_url};
use trestle_identity::password::{PasswordPolicy, Passwords};
use trestle_identity::routes::CALLBACK_PATH;
use trestle_identity::session::{SessionConfig, Sessions};
use trestle_identity::{Browser, BrowserAuth, Chain, Client, OidcLogin};

use crate::Environment;
use crate::project::ServeExtension;

#[derive(clap::Args, Clone)]
pub struct ServeConfig {
    #[arg(long, env = "TRESTLE_BIND", default_value = "127.0.0.1:8000")]
    pub bind: SocketAddr,

    /// Internal Prometheus listener. Bind it to a private interface; it is
    /// not part of the public API.
    #[arg(long, env = "TRESTLE_METRICS_BIND")]
    pub metrics_bind: Option<SocketAddr>,

    /// Development bearer tokens: token=actor-uuid,... Refused in production.
    #[arg(long, env = "TRESTLE_DEV_TOKENS", hide_env_values = true)]
    pub dev_tokens: Option<String>,

    /// Concurrent requests before new ones get 503 `overloaded`.
    #[arg(long, env = "TRESTLE_MAX_IN_FLIGHT", default_value_t = 256)]
    pub max_in_flight: usize,
    /// Sustained requests per second per credential (or client address).
    #[arg(long, env = "TRESTLE_RATE_PER_SECOND", default_value_t = 20.0)]
    pub rate_per_second: f64,
    #[arg(long, env = "TRESTLE_RATE_BURST", default_value_t = 40.0)]
    pub rate_burst: f64,
    /// Unauthenticated requests per second per client address.
    #[arg(long, env = "TRESTLE_ADDRESS_RATE_PER_SECOND", default_value_t = 20.0)]
    pub address_rate_per_second: f64,
    /// Unauthenticated burst per client address.
    #[arg(long, env = "TRESTLE_ADDRESS_BURST", default_value_t = 60.0)]
    pub address_burst: f64,
    /// Header the trusted edge sets to the client address (e.g. X-Real-IP).
    /// Only set this behind an edge that overwrites it.
    #[arg(long, env = "TRESTLE_CLIENT_ADDRESS_HEADER")]
    pub client_address_header: Option<String>,
    /// Use the socket peer as the client address (no proxy in front, or one
    /// whose address may stand for every client). Production requires
    /// either this or TRESTLE_CLIENT_ADDRESS_HEADER.
    #[arg(long, env = "TRESTLE_TRUST_PEER_ADDRESS")]
    pub trust_peer_address: bool,

    #[arg(long, env = "TRESTLE_DB_POOL_SIZE", default_value_t = 10)]
    pub pool_size: u32,
    /// Server-side limit for any one SQL statement.
    #[arg(long, env = "TRESTLE_STATEMENT_TIMEOUT_MS", default_value_t = 10_000)]
    pub statement_timeout_ms: u64,

    /// Also run a job worker in this process (ADR-015). Needs
    /// `trestle::core::jobs::JobsApp` installed and its migration applied.
    #[arg(long, env = "TRESTLE_JOBS")]
    pub jobs: bool,
    /// Jobs the worker runs at once.
    #[arg(long, env = "TRESTLE_JOB_CONCURRENCY", default_value_t = 8)]
    pub job_concurrency: usize,

    /// Signs list cursors (at least 32 bytes). Required in production.
    #[arg(long, env = "TRESTLE_CURSOR_KEY", hide_env_values = true)]
    pub cursor_key: Option<String>,
    /// Derives session CSRF tokens (at least 32 bytes).
    #[arg(long, env = "TRESTLE_SESSION_KEY", hide_env_values = true)]
    pub session_key: Option<String>,

    /// OpenID Connect issuer, exactly as the provider publishes it.
    #[arg(long, env = "TRESTLE_OIDC_ISSUER")]
    pub oidc_issuer: Option<String>,
    /// Browser login client ID. Enables sessions.
    #[arg(long, env = "TRESTLE_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,
    #[arg(long, env = "TRESTLE_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,
    #[arg(long, env = "TRESTLE_OIDC_SCOPES", default_value = "openid")]
    pub oidc_scopes: String,
    /// Audience of JWT access tokens for API clients. Enables bearer tokens.
    #[arg(long, env = "TRESTLE_OIDC_API_AUDIENCE")]
    pub oidc_api_audience: Option<String>,
    /// Email and password sign-in for accounts created with
    /// `<project> account invite`. Needs TRESTLE_SESSION_KEY and
    /// TRESTLE_PUBLIC_ORIGIN.
    #[arg(long, env = "TRESTLE_PASSWORD_LOGIN")]
    pub password_login: bool,
    /// Password sign-in attempts per client address per minute (all
    /// accounts together). Raise it if many people sign in from one
    /// network address, e.g. an office or campus behind NAT.
    #[arg(
        long,
        env = "TRESTLE_PASSWORD_ATTEMPTS_PER_MINUTE",
        default_value_t = 30.0
    )]
    pub password_attempts_per_minute: f64,
    /// Public browser origin(s), comma-separated. The first one hosts the
    /// login callback and appears in invite and reset links.
    #[arg(long, env = "TRESTLE_PUBLIC_ORIGIN", value_delimiter = ',')]
    pub public_origin: Vec<String>,
}

impl ServeConfig {
    /// The command-line defaults with no identity, independent of the
    /// process environment. For tests.
    pub fn for_tests() -> Self {
        Self {
            bind: "127.0.0.1:0".parse().expect("static address"),
            metrics_bind: None,
            dev_tokens: None,
            max_in_flight: 256,
            rate_per_second: 20.0,
            rate_burst: 40.0,
            address_rate_per_second: 20.0,
            address_burst: 60.0,
            client_address_header: None,
            trust_peer_address: false,
            pool_size: 10,
            statement_timeout_ms: 10_000,
            jobs: false,
            job_concurrency: 8,
            cursor_key: None,
            session_key: None,
            oidc_issuer: None,
            oidc_client_id: None,
            password_login: false,
            password_attempts_per_minute: 30.0,
            oidc_client_secret: None,
            oidc_scopes: "openid".into(),
            oidc_api_audience: None,
            public_origin: Vec::new(),
        }
    }

    /// Applies the intent file's `[auth]` section: password sign-in follows
    /// `passwords` (default on), and `oidc` fills the issuer and client from
    /// the environment variables it names. Explicit `TRESTLE_OIDC_*`
    /// settings win. Reports missing variables by name, never values.
    pub fn apply_auth(
        &mut self,
        auth: &crate::intent::AuthSection,
        var: impl Fn(&str) -> Option<String>,
    ) -> Vec<String> {
        let mut problems = Vec::new();
        self.password_login = auth.passwords;
        if let Some(oidc) = &auth.oidc {
            match oidc.provider() {
                Err(e) => problems.push(format!("[auth] {e}")),
                Ok(p) => {
                    self.oidc_issuer.get_or_insert(p.issuer);
                    if self.oidc_client_id.is_none() {
                        match var(&p.client_id_env) {
                            Some(v) if !v.is_empty() => self.oidc_client_id = Some(v),
                            _ => problems
                                .push(format!("{} is required by [auth] oidc", p.client_id_env)),
                        }
                    }
                    if self.oidc_client_secret.is_none() {
                        match var(&p.client_secret_env) {
                            Some(v) if !v.is_empty() => self.oidc_client_secret = Some(v),
                            _ => problems.push(format!(
                                "{} is required by [auth] oidc",
                                p.client_secret_env
                            )),
                        }
                    }
                }
            }
        }
        problems
    }

    fn transport(env: Environment) -> Transport {
        if env.is_production() {
            Transport::HttpsOnly
        } else {
            Transport::AllowLoopbackHttp
        }
    }

    /// Everything wrong with this configuration for `env`, all at once.
    pub fn problems(&self, env: Environment, database: &PgConnectOptions) -> Vec<String> {
        let mut problems = Vec::new();
        let transport = Self::transport(env);
        let mut key = |name: &str, value: &Option<String>, required: bool| match value {
            Some(v) if v.len() < 32 => problems.push(format!("{name} must be at least 32 bytes")),
            None if required => problems.push(format!("{name} is required")),
            _ => {}
        };
        key("TRESTLE_CURSOR_KEY", &self.cursor_key, env.is_production());
        let browser_sessions = self.oidc_client_id.is_some() || self.password_login;
        key("TRESTLE_SESSION_KEY", &self.session_key, browser_sessions);

        // `!(x > 0)` also refuses NaN; infinities are refused explicitly.
        let finite = |x: f64| x.is_finite();
        if self.max_in_flight == 0
            || !(self.rate_per_second > 0.0 && finite(self.rate_per_second))
            || !(self.rate_burst >= 1.0 && finite(self.rate_burst))
            || !(self.address_rate_per_second > 0.0 && finite(self.address_rate_per_second))
            || !(self.address_burst >= 1.0 && finite(self.address_burst))
            || !(self.password_attempts_per_minute >= 1.0
                && finite(self.password_attempts_per_minute))
        {
            problems.push(
                "admission limits must be positive (TRESTLE_MAX_IN_FLIGHT, TRESTLE_RATE_*)".into(),
            );
        }
        if let Some(issuer) = &self.oidc_issuer
            && let Err(e) = check_url(issuer, transport)
        {
            problems.push(format!("TRESTLE_OIDC_ISSUER: {e}"));
        }
        let oidc_client = self.oidc_client_id.is_some() || self.oidc_api_audience.is_some();
        if oidc_client && self.oidc_issuer.is_none() {
            problems
                .push("TRESTLE_OIDC_ISSUER is required with a client ID or API audience".into());
        }
        if browser_sessions {
            if self.public_origin.is_empty() {
                problems.push("TRESTLE_PUBLIC_ORIGIN is required for browser sign-in".into());
            }
            for origin in &self.public_origin {
                if let Err(e) = check_url(origin, transport) {
                    problems.push(format!("TRESTLE_PUBLIC_ORIGIN: {e}"));
                }
            }
        }
        if self.oidc_client_id.is_some() && !self.oidc_scopes.split(' ').any(|s| s == "openid") {
            problems.push("TRESTLE_OIDC_SCOPES must include openid".into());
        }
        if self.oidc_api_audience.is_some() && self.oidc_api_audience == self.oidc_client_id {
            problems.push(
                "TRESTLE_OIDC_API_AUDIENCE must differ from TRESTLE_OIDC_CLIENT_ID \
                 (ID tokens must not be accepted as API tokens)"
                    .into(),
            );
        }

        if self.client_address_header.is_some() && self.trust_peer_address {
            problems.push(
                "set only one of TRESTLE_CLIENT_ADDRESS_HEADER and TRESTLE_TRUST_PEER_ADDRESS"
                    .into(),
            );
        }
        if env.is_production() {
            if self.client_address_header.is_none() && !self.trust_peer_address {
                problems.push(
                    "production needs to know client addresses: set TRESTLE_CLIENT_ADDRESS_HEADER \
                     (behind an edge that overwrites it; the API must only be reachable through \
                     that edge) or TRESTLE_TRUST_PEER_ADDRESS=true (no proxy in front)"
                        .into(),
                );
            }
            if self.dev_tokens.is_some() {
                problems.push("TRESTLE_DEV_TOKENS must not be set in production".into());
            }
            if !oidc_client && !self.password_login {
                problems.push(
                    "production needs a way to sign in: TRESTLE_PASSWORD_LOGIN, and/or \
                     TRESTLE_OIDC_ISSUER with TRESTLE_OIDC_CLIENT_ID (browser sessions) or \
                     TRESTLE_OIDC_API_AUDIENCE (API tokens)"
                        .into(),
                );
            }
            if !local_database(database)
                && !matches!(
                    database.get_ssl_mode(),
                    PgSslMode::Require | PgSslMode::VerifyCa | PgSslMode::VerifyFull
                )
            {
                problems.push(
                    "DATABASE_URL must use sslmode=verify-full (or require/verify-ca) \
                     for a non-local database in production"
                        .into(),
                );
            }
        } else if self.dev_tokens.is_none() && !oidc_client && !self.password_login {
            problems.push(
                "no identity adapter configured: set TRESTLE_DEV_TOKENS (development) \
                 or TRESTLE_OIDC_*"
                    .into(),
            );
        }
        problems
    }

    /// Builds the identity chain. Contacts the provider (discovery, keys).
    pub async fn identity(
        &self,
        env: Environment,
        pool: &PgPool,
    ) -> Result<(Identity, Browser), String> {
        let transport = Self::transport(env);
        let mut bearer: Vec<Identity> = Vec::new();
        if let Some(tokens) = &self.dev_tokens {
            let dev =
                DevTokenIdentity::parse(tokens).map_err(|e| format!("TRESTLE_DEV_TOKENS: {e}"))?;
            bearer.push(Arc::new(dev));
        }
        let mut oidc = None;
        if let Some(issuer) = &self.oidc_issuer {
            let provider = Provider::discover(issuer, transport)
                .await
                .map_err(|e| format!("OpenID Connect discovery for {issuer}: {e}"))?;
            if let Some(audience) = &self.oidc_api_audience {
                bearer.push(Arc::new(BearerIdentity::new(provider.clone(), audience)));
            }
            if let Some(client_id) = &self.oidc_client_id {
                let origin = self.public_origin[0].trim_end_matches('/');
                oidc = Some(OidcLogin {
                    provider,
                    client: Client {
                        id: client_id.clone(),
                        secret: self.oidc_client_secret.clone(),
                        redirect_url: format!("{origin}{CALLBACK_PATH}"),
                        scopes: self.oidc_scopes.clone(),
                    },
                });
            }
        }
        let mut browser = Browser::default();
        let mut sessions = None;
        if oidc.is_some() || self.password_login {
            let key = self.session_key.as_deref().unwrap_or_default();
            let config = SessionConfig::new(key.as_bytes(), &self.public_origin, transport)?;
            let store = Sessions::new(pool.clone(), config);
            let passwords = self.password_login.then(|| {
                Passwords::new(
                    pool.clone(),
                    PasswordPolicy {
                        attempts_per_minute: self.password_attempts_per_minute,
                        ..PasswordPolicy::default()
                    },
                    self.client_address_header.clone(),
                )
            });
            browser = Browser(Some(Arc::new(BrowserAuth {
                sessions: store.clone(),
                oidc,
                passwords,
            })));
            sessions = Some(store);
        }
        let chain = Chain::new(bearer, sessions);
        if chain.is_empty() {
            return Err("no identity adapter configured".into());
        }
        if env.is_production() && !chain.production_safe() {
            return Err("a development-only identity adapter cannot run in production".into());
        }
        tracing::info!(adapters = ?chain.describe(), "identity configured");
        Ok((Arc::new(chain), browser))
    }

    fn admission(&self) -> Arc<Admission> {
        Admission::new(AdmissionConfig {
            max_in_flight: self.max_in_flight,
            rate_per_second: self.rate_per_second,
            burst: self.rate_burst,
            address_rate_per_second: self.address_rate_per_second,
            address_burst: self.address_burst,
            client_address_header: self.client_address_header.clone(),
        })
    }
}

fn local_database(options: &PgConnectOptions) -> bool {
    options.get_socket().is_some()
        || options.get_host().starts_with('/')
        || matches!(options.get_host(), "localhost" | "127.0.0.1" | "::1")
}

/// Boots and serves until SIGINT/SIGTERM.
///
/// `extension` adds project-specific settings and builds the project's
/// state; its problems are reported together with the built-in ones.
pub async fn serve<S, X>(
    registry: Arc<Registry<S>>,
    env: Environment,
    database: PgConnectOptions,
    config: ServeConfig,
    extension: X,
    bin: &str,
) -> Result<(), String>
where
    S: Clone + Send + Sync + 'static,
    PgPool: axum::extract::FromRef<S>,
    X: ServeExtension<S>,
{
    let mut problems = config.problems(env, &database);
    problems.extend(extension.problems());
    if config.jobs && registry.desired_schema().model("jobs.job").is_none() {
        problems.push(
            "TRESTLE_JOBS needs the job table: install trestle::core::jobs::JobsApp and run \
             makemigrations"
                .into(),
        );
    }
    if config.jobs && config.job_concurrency == 0 {
        problems.push("TRESTLE_JOB_CONCURRENCY must be at least 1".into());
    }
    if !problems.is_empty() {
        return Err(format!(
            "refusing to start ({env:?}):\n  - {}",
            problems.join("\n  - ")
        ));
    }
    match &config.cursor_key {
        Some(key) => pagination::install_key(key.as_bytes())?,
        None => tracing::warn!("TRESTLE_CURSOR_KEY is not set: list cursors expire on restart"),
    }

    let database = database.options([(
        "statement_timeout",
        format!("{}ms", config.statement_timeout_ms),
    )]);
    let pool = PgPoolOptions::new()
        .max_connections(config.pool_size)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(database)
        .await
        .map_err(|e| format!("connect to database: {e}"))?;

    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    let missing = trestle_historical::missing(&mut conn, registry.required_migrations())
        .await
        .map_err(|e| e.to_string())?;
    drop(conn);
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(ToString::to_string).collect();
        return Err(format!(
            "required migrations are not applied: {} (run `{bin} migrate`)",
            names.join(", ")
        ));
    }

    let (identity, browser) = config.identity(env, &pool).await?;
    let admission = config.admission();
    let state = extension.state(crate::StateParts {
        pool: pool.clone(),
        identity,
        browser,
        env,
    })?;
    let app = crate::router_with(
        &registry,
        state.clone(),
        Perimeter {
            admission: Some(admission.clone()),
        },
    );

    if let Some(bind) = config.metrics_bind {
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| format!("bind metrics {bind}: {e}"))?;
        tracing::info!(%bind, "metrics listening");
        let internal = metrics_router(pool.clone(), admission.clone());
        tokio::spawn(async move {
            if let Err(err) = axum::serve(listener, internal).await {
                tracing::error!(error = %err, "metrics listener failed");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("bind {}: {e}", config.bind))?;

    // One signal stops both: HTTP stops accepting, the worker stops claiming
    // and drains.
    let (stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = stop.send(true);
    });
    let until_stopped = |mut stopped: tokio::sync::watch::Receiver<bool>| async move {
        let _ = stopped.wait_for(|s| *s).await;
    };
    let worker = config.jobs.then(|| {
        let worker = Worker::new(
            registry.clone(),
            state,
            WorkerConfig {
                concurrency: config.job_concurrency,
                ..WorkerConfig::default()
            },
        );
        tracing::info!(concurrency = config.job_concurrency, "job worker running");
        tokio::spawn(worker.run(until_stopped(stopped.clone())))
    });

    tracing::info!(bind = %config.bind, env = ?env, "serving");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(until_stopped(stopped))
    .await
    .map_err(|e| e.to_string())?;
    if let Some(worker) = worker {
        match worker.await {
            Ok(pass) => tracing::info!(?pass, "job worker stopped"),
            Err(err) => tracing::error!(error = %err, "job worker panicked"),
        }
    }
    pool.close().await;
    Ok(())
}

/// `GET /metrics`: recorded counters and histograms plus pool and
/// admission gauges sampled now.
pub fn metrics_router(pool: PgPool, admission: Arc<Admission>) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(
                |State((pool, admission)): State<(PgPool, Arc<Admission>)>| async move {
                    let gauges = [
                        (
                            "trestle_db_pool_connections",
                            "Database pool connections by state.",
                            vec![("state", "open".to_owned())],
                            f64::from(pool.size()),
                        ),
                        (
                            "trestle_db_pool_connections",
                            "Database pool connections by state.",
                            vec![("state", "idle".to_owned())],
                            pool.num_idle() as f64,
                        ),
                        (
                            "trestle_http_in_flight",
                            "Requests admitted and not yet finished.",
                            Vec::new(),
                            admission.in_flight() as f64,
                        ),
                    ];
                    (
                        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                        metrics::render(&gauges),
                    )
                },
            ),
        )
        .with_state((pool, admission))
}

async fn shutdown() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
    tracing::info!("shutting down: draining in-flight requests");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> PgConnectOptions {
        "postgres://app@localhost/app".parse().unwrap()
    }

    fn remote(sslmode: &str) -> PgConnectOptions {
        format!("postgres://app@db.internal/app?sslmode={sslmode}")
            .parse()
            .unwrap()
    }

    fn production() -> ServeConfig {
        let mut config = ServeConfig::for_tests();
        config.cursor_key = Some("c".repeat(32));
        config.session_key = Some("s".repeat(32));
        config.oidc_issuer = Some("https://id.example.com".into());
        config.oidc_client_id = Some("web-client".into());
        config.oidc_api_audience = Some("https://api.example.com".into());
        config.public_origin = vec!["https://app.example.com".into()];
        config.client_address_header = Some("x-real-ip".into());
        config
    }

    #[tokio::test]
    async fn jobs_refuse_to_start_without_the_job_table() {
        let start = |project: trestle_core::registry::Project<crate::State>| async {
            let mut config = ServeConfig::for_tests();
            config.jobs = true;
            let registry = Arc::new(project.assemble().unwrap());
            serve(
                registry,
                Environment::Development,
                local(),
                config,
                crate::project::NoExtension::default(),
                "t",
            )
            .await
            .unwrap_err()
        };
        let without = start(trestle_core::registry::Project::new("t", "0")).await;
        assert!(
            without.contains("TRESTLE_JOBS needs the job table"),
            "{without}"
        );
        // With the table installed the check passes; startup then fails on
        // the (unreachable) database instead.
        let with = start(
            trestle_core::registry::Project::new("t", "0").install(trestle_core::jobs::JobsApp),
        )
        .await;
        assert!(!with.contains("TRESTLE_JOBS"), "{with}");
    }

    #[test]
    fn production_requires_a_decision_about_client_addresses() {
        let mut config = production();
        config.client_address_header = None;
        let text = config
            .problems(Environment::Production, &local())
            .join("\n");
        assert!(text.contains("TRESTLE_CLIENT_ADDRESS_HEADER"), "{text}");
    }

    #[test]
    fn rate_limits_must_be_finite() {
        for bad in [f64::NAN, f64::INFINITY] {
            for field in 0..3 {
                let mut config = production();
                match field {
                    0 => config.rate_per_second = bad,
                    1 => config.rate_burst = bad,
                    _ => config.password_attempts_per_minute = bad,
                }
                let text = config
                    .problems(Environment::Production, &local())
                    .join("\n");
                assert!(
                    text.contains("TRESTLE_RATE"),
                    "{bad} in field {field}: {text}"
                );
            }
        }
    }

    #[test]
    fn production_configuration_passes_when_complete() {
        assert_eq!(
            production().problems(Environment::Production, &remote("verify-full")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn production_refuses_dev_tokens_missing_keys_and_plaintext_databases() {
        let mut config = production();
        config.dev_tokens = Some("alice-dev-token=aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into());
        config.cursor_key = None;
        config.session_key = Some("short".into());
        let problems = config.problems(Environment::Production, &remote("prefer"));
        let text = problems.join("\n");
        assert!(
            text.contains("TRESTLE_DEV_TOKENS must not be set"),
            "{text}"
        );
        assert!(text.contains("TRESTLE_CURSOR_KEY is required"), "{text}");
        assert!(
            text.contains("TRESTLE_SESSION_KEY must be at least 32 bytes"),
            "{text}"
        );
        assert!(text.contains("sslmode"), "{text}");
        assert_eq!(problems.len(), 4, "all problems are reported together");
        // A local socket or loopback database needs no TLS.
        assert!(
            production()
                .problems(Environment::Production, &local())
                .is_empty()
        );
    }

    #[test]
    fn production_requires_https_provider_and_origin() {
        let mut config = production();
        config.oidc_issuer = Some("http://id.example.com".into());
        config.public_origin = vec!["http://app.example.com".into()];
        let text = config
            .problems(Environment::Production, &local())
            .join("\n");
        assert!(text.contains("TRESTLE_OIDC_ISSUER"), "{text}");
        assert!(text.contains("TRESTLE_PUBLIC_ORIGIN"), "{text}");
    }

    #[test]
    fn production_requires_an_identity_provider() {
        let mut config = ServeConfig::for_tests();
        config.cursor_key = Some("c".repeat(32));
        let text = config
            .problems(Environment::Production, &local())
            .join("\n");
        assert!(text.contains("production needs a way to sign in"), "{text}");
    }

    #[test]
    fn password_sign_in_alone_is_enough_for_production() {
        let mut config = ServeConfig::for_tests();
        config.cursor_key = Some("c".repeat(32));
        config.password_login = true;
        config.trust_peer_address = true;
        let text = config
            .problems(Environment::Production, &local())
            .join("\n");
        assert!(text.contains("TRESTLE_SESSION_KEY is required"), "{text}");
        assert!(text.contains("TRESTLE_PUBLIC_ORIGIN is required"), "{text}");
        config.session_key = Some("s".repeat(32));
        config.public_origin = vec!["https://app.example.com".into()];
        assert!(
            config
                .problems(Environment::Production, &remote("verify-full"))
                .is_empty()
        );
    }

    #[test]
    fn id_tokens_cannot_double_as_api_tokens() {
        let mut config = production();
        config.oidc_api_audience = config.oidc_client_id.clone();
        let text = config
            .problems(Environment::Production, &local())
            .join("\n");
        assert!(text.contains("must differ"), "{text}");
    }

    #[test]
    fn development_needs_some_identity() {
        let config = ServeConfig::for_tests();
        let text = config
            .problems(Environment::Development, &local())
            .join("\n");
        assert!(text.contains("no identity adapter configured"), "{text}");
        let mut config = ServeConfig::for_tests();
        config.dev_tokens = Some("x".into());
        assert!(
            config
                .problems(Environment::Development, &local())
                .is_empty()
        );
    }
}
