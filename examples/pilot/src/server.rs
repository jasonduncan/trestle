//! Serving pilot: Trestle's `serve`, building pilot's state.
//!
//! The generic configuration, production boot checks, identity selection
//! and listeners live in `trestle::serve`. Production still refuses to boot
//! rather than run degraded; pilot adds its own problems to the same list.

use std::ops::{Deref, DerefMut};

use sqlx::postgres::PgConnectOptions;
use trestle::{ServeExtension, StateParts};

use crate::AppState;
use crate::config::Environment;

pub use trestle::metrics_router;

/// `pilot serve` settings: Trestle's (pilot adds none of its own).
#[derive(clap::Args, Clone)]
pub struct ServeConfig {
    #[command(flatten)]
    pub base: trestle::ServeConfig,
}

/// How `serve` builds pilot's [`AppState`]. Pilot has no settings beyond
/// Trestle's; a project with its own would add `clap` fields here.
#[derive(clap::Args, Clone, Default)]
pub struct PilotServe {}

impl Deref for ServeConfig {
    type Target = trestle::ServeConfig;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl DerefMut for ServeConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

impl ServeConfig {
    /// The command-line defaults with no identity, independent of the
    /// process environment. For tests.
    pub fn for_tests() -> Self {
        Self {
            base: trestle::ServeConfig::for_tests(),
        }
    }

    /// Everything wrong with this configuration for `env`, all at once.
    pub fn problems(&self, env: Environment, database: &PgConnectOptions) -> Vec<String> {
        self.base.problems(env, database)
    }
}

impl ServeExtension<AppState> for PilotServe {
    fn problems(&self) -> Vec<String> {
        Vec::new()
    }

    fn state(self, parts: StateParts) -> Result<AppState, String> {
        Ok(AppState {
            pool: parts.pool,
            identity: parts.identity,
            browser: parts.browser,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> PgConnectOptions {
        "postgres://pilot@localhost/pilot".parse().unwrap()
    }

    fn remote(sslmode: &str) -> PgConnectOptions {
        format!("postgres://pilot@db.internal/pilot?sslmode={sslmode}")
            .parse()
            .unwrap()
    }

    fn production() -> ServeConfig {
        let mut config = ServeConfig::for_tests();
        config.cursor_key = Some("c".repeat(32));
        config.session_key = Some("s".repeat(32));
        config.oidc_issuer = Some("https://id.example.com".into());
        config.oidc_client_id = Some("pilot-web".into());
        config.oidc_api_audience = Some("https://api.example.com".into());
        config.public_origin = vec!["https://app.example.com".into()];
        config.client_address_header = Some("x-real-ip".into());
        config
    }

    #[test]
    fn production_configuration_passes_when_complete() {
        assert_eq!(
            production().problems(Environment::Production, &remote("verify-full")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn production_requires_verify_full_for_a_remote_database() {
        // `require` accepts any certificate and `verify-ca` any host name,
        // so neither authenticates the server (SR-03). Each is refused, and
        // reported together with every other problem.
        for weak in ["disable", "allow", "prefer", "require", "verify-ca"] {
            let problems = production().problems(Environment::Production, &remote(weak));
            assert_eq!(problems.len(), 1, "{weak}: {problems:?}");
            assert!(
                problems[0].contains("sslmode=verify-full"),
                "{weak}: {problems:?}"
            );
            let mut config = production();
            config.cursor_key = None;
            let problems = config.problems(Environment::Production, &remote(weak));
            assert_eq!(problems.len(), 2, "{weak}: {problems:?}");
        }
        assert!(
            production()
                .problems(Environment::Production, &remote("verify-full"))
                .is_empty()
        );
        // Development and loopback databases are not affected.
        assert!(
            ServeConfig::for_tests()
                .problems(Environment::Development, &remote("require"))
                .iter()
                .all(|p| !p.contains("sslmode"))
        );
        for local in [
            "postgres://pilot@127.0.0.1/pilot?sslmode=require",
            "postgres://pilot@localhost/pilot?sslmode=disable",
        ] {
            let options: PgConnectOptions = local.parse().unwrap();
            assert!(
                production()
                    .problems(Environment::Production, &options)
                    .is_empty(),
                "{local}"
            );
        }
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
