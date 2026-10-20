//! Password accounts, in the manner of Django's `contrib.auth` (no HTML).
//!
//! * **Hashing:** Argon2id from the RustCrypto `argon2` crate with the OWASP
//!   parameters (19 MiB, 2 passes, 1 lane), stored as PHC strings. Hashing
//!   runs off the async runtime with a bound on concurrent hashes. A hash
//!   made with weaker parameters is upgraded at the next successful sign-in.
//! * **Accounts come from an operator**, who issues a one-time invite link;
//!   the person sets their password through it. Resets work the same way.
//!   Links carry 256 random bits, are stored as SHA-256, expire, and work
//!   once; issuing a new link voids older ones. The token travels in the
//!   URL fragment, so it never reaches server logs or `Referer` headers.
//! * **Sign-in failures all look the same:** unknown email, wrong password,
//!   disabled, locked, or no password yet. Each costs one Argon2
//!   verification, so timing does not reveal which. Five consecutive
//!   failures lock the account for 15 minutes, and attempts are limited per
//!   client address (60 a minute by default).
//! * **Every endpoint requires an allowed `Origin`** (login CSRF) and a JSON
//!   body. Success creates the same server-side session as OpenID Connect
//!   sign-in, so everything downstream is unchanged.
//!
//! ```text
//! POST /api/v1/auth/password/login    {email, password}            -> 200 session info + cookie
//! POST /api/v1/auth/password/inspect  {token}                      -> 200 {email, purpose, expires_at}
//! POST /api/v1/auth/password/set      {token, password}            -> 200 session info + cookie
//! POST /api/v1/auth/password/change   {current_password, new_password}  (session + CSRF) -> 204
//! ```

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use argon2::{Argon2, Params};
use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use http::{HeaderMap, StatusCode, header};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Semaphore;
use trestle_core::admission::{RateLimiter, client_address};
use trestle_core::identity::ActorId;
use trestle_core::input::JsonBody;
use trestle_core::{FieldError, Problem};
use uuid::Uuid;

use crate::random;
use crate::routes::{Browser, session_cookie};
use crate::session::{SESSION_COOKIE, Sessions, cookie};

pub const LOGIN_PATH: &str = "/api/v1/auth/password/login";
pub const INSPECT_PATH: &str = "/api/v1/auth/password/inspect";
pub const SET_PATH: &str = "/api/v1/auth/password/set";
pub const CHANGE_PATH: &str = "/api/v1/auth/password/change";

/// The frontend route an invite or reset link opens.
pub const SET_PASSWORD_ROUTE: &str = "/set-password";

#[derive(Debug, Clone)]
pub struct PasswordPolicy {
    pub min_chars: usize,
    pub max_chars: usize,
    /// Consecutive failures before the account is locked.
    pub lock_after: i32,
    pub lock_for: Duration,
    /// Sign-in endpoint attempts per client address. Schools often put
    /// every teacher behind one address, so this guards against spraying
    /// many accounts from one source; per-account lockout protects each
    /// account.
    pub attempts_per_minute: f64,
    /// Sign-in attempts per account (by normalized email, whether or not
    /// the account exists) per minute, from any address. Lockout after
    /// `lock_after` consecutive failures still applies on top.
    pub account_attempts_per_minute: f64,
    pub concurrent_hashes: usize,
}

impl Default for PasswordPolicy {
    fn default() -> Self {
        Self {
            min_chars: 10,
            max_chars: 256,
            lock_after: 5,
            lock_for: Duration::from_secs(15 * 60),
            attempts_per_minute: 30.0,
            account_attempts_per_minute: 10.0,
            concurrent_hashes: 4,
        }
    }
}

pub struct Passwords {
    pool: PgPool,
    policy: PasswordPolicy,
    hashing: Arc<Semaphore>,
    limiter: RateLimiter,
    accounts: RateLimiter,
    client_address_header: Option<String>,
    /// Test-only pause point between password verification and the writes
    /// that follow, for deterministic race tests.
    #[cfg(test)]
    pub(crate) pause: std::sync::Mutex<Option<Arc<Pause>>>,
}

/// Two barriers: the sign-in waits on `reached` after verifying, then on
/// `resume` before continuing.
#[cfg(test)]
pub(crate) struct Pause {
    pub reached: tokio::sync::Barrier,
    pub resume: tokio::sync::Barrier,
}

#[cfg(test)]
impl Pause {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reached: tokio::sync::Barrier::new(2),
            resume: tokio::sync::Barrier::new(2),
        })
    }
}

impl Passwords {
    pub fn new(
        pool: PgPool,
        policy: PasswordPolicy,
        client_address_header: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            hashing: Arc::new(Semaphore::new(policy.concurrent_hashes.max(1))),
            limiter: RateLimiter::new(
                policy.attempts_per_minute / 60.0,
                policy.attempts_per_minute.max(1.0),
            ),
            accounts: RateLimiter::new(
                policy.account_attempts_per_minute / 60.0,
                policy.account_attempts_per_minute.max(1.0),
            ),
            client_address_header,
            policy,
            pool,
            #[cfg(test)]
            pause: std::sync::Mutex::new(None),
        })
    }

    pub fn policy(&self) -> &PasswordPolicy {
        &self.policy
    }

    fn throttle(&self, parts: &Parts) -> Result<(), Problem> {
        let address = client_address(
            &parts.headers,
            &parts.extensions,
            self.client_address_header.as_deref(),
        );
        let mut key = b"password-signin:".to_vec();
        key.extend_from_slice(&address);
        self.limiter.take(&key).map_err(Problem::rate_limited)
    }

    async fn hash(&self, password: String) -> Result<String, Problem> {
        // The permit moves into the blocking job: if the request is
        // cancelled, the hash still runs, and still counts against the limit.
        let permit = self
            .hashing
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Problem::internal())?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            hash_password(&password)
        })
        .await
        .map_err(|_| Problem::internal())?
        .map_err(|err| {
            tracing::error!(error = %err, "password hashing failed");
            Problem::internal()
        })
    }

    async fn verify(&self, password: String, phc: Option<String>) -> Result<bool, Problem> {
        let permit = self
            .hashing
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Problem::internal())?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match phc {
                Some(phc) => verify_password(&password, &phc),
                // Same cost as a real check, so timing reveals nothing.
                None => {
                    verify_password(&password, &DUMMY_HASH);
                    false
                }
            }
        })
        .await
        .map_err(|_| Problem::internal())
    }

    /// Signs in with `email` and `password` and creates a session; the actor
    /// and the new session's cookie secret.
    pub(crate) async fn login_session(
        &self,
        sessions: &Sessions,
        email: &str,
        password: String,
    ) -> Result<(ActorId, String), Problem> {
        let verified = self.verify_credentials(email, password).await?;
        let mut tx = self.pool.begin().await?;
        self.confirm(&mut tx, &verified).await?;
        let token = sessions.create_in(&mut tx, verified.actor).await?;
        tx.commit().await?;
        Ok((verified.actor, token))
    }

    /// Checks `email` and `password` and confirms the account is still in
    /// the state that was checked; the actor on success.
    async fn authenticate(&self, email: &str, password: String) -> Result<ActorId, Problem> {
        let verified = self.verify_credentials(email, password).await?;
        let mut tx = self.pool.begin().await?;
        self.confirm(&mut tx, &verified).await?;
        tx.commit().await?;
        Ok(verified.actor)
    }

    /// Hashing happens before this, outside any lock. Here, inside the
    /// caller's transaction, the account row is locked and must still be
    /// enabled with the very hash that was verified; a concurrent disable,
    /// reset or change therefore either finishes first (and this fails) or
    /// waits for the caller's commit (and then revokes what it issued).
    /// Clears the failure count and stores an upgraded hash if one was made.
    async fn confirm(
        &self,
        tx: &mut sqlx::PgConnection,
        verified: &Verified,
    ) -> Result<(), Problem> {
        let current: Option<(Option<String>, bool)> = sqlx::query_as(
            "SELECT password_hash, disabled_at IS NOT NULL \
             FROM identity_accounts WHERE id = $1 FOR UPDATE",
        )
        .bind(verified.account)
        .fetch_optional(&mut *tx)
        .await?;
        match current {
            Some((Some(phc), false)) if phc == verified.phc => {}
            _ => return Err(login_failed()),
        }
        sqlx::query(
            "UPDATE identity_accounts SET failed_logins = 0, locked_until = NULL, \
                password_hash = coalesce($2, password_hash) WHERE id = $1",
        )
        .bind(verified.account)
        .bind(verified.upgraded.as_deref())
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Checks `email` and `password` without issuing anything. Every
    /// failure is the same `login_failed` to the caller, costs one hash
    /// verification and runs the same failure-count statement.
    async fn verify_credentials(&self, email: &str, password: String) -> Result<Verified, Problem> {
        if password.chars().count() > self.policy.max_chars {
            return Err(login_failed());
        }
        let normalized = normalize_email(email);
        let mut key = b"password-account:".to_vec();
        match &normalized {
            Ok(e) => key.extend_from_slice(e.as_bytes()),
            Err(_) => key.extend_from_slice(email.trim().to_lowercase().as_bytes()),
        }
        self.accounts.take(&key).map_err(Problem::rate_limited)?;
        let account = match normalized {
            Ok(email) => {
                sqlx::query_as::<_, (Uuid, Uuid, Option<String>, bool, bool)>(
                    "SELECT id, actor_id, password_hash, disabled_at IS NOT NULL, \
                        coalesce(locked_until > now(), false) \
                 FROM identity_accounts WHERE email = $1",
                )
                .bind(email)
                .fetch_optional(&self.pool)
                .await?
            }
            Err(_) => None,
        };
        let usable = account
            .as_ref()
            .filter(|(_, _, hash, disabled, locked)| hash.is_some() && !disabled && !locked);
        let ok = self
            .verify(password.clone(), usable.and_then(|a| a.2.clone()))
            .await?;
        #[cfg(test)]
        {
            let pause = self.pause.lock().unwrap().take();
            if let Some(pause) = pause {
                pause.reached.wait().await;
                pause.resume.wait().await;
            }
        }
        match (usable, ok) {
            (Some((id, actor, Some(phc), _, _)), true) => {
                let upgraded = if needs_rehash(phc) {
                    Some(self.hash(password).await?)
                } else {
                    None
                };
                Ok(Verified {
                    account: *id,
                    actor: ActorId(*actor),
                    phc: phc.clone(),
                    upgraded,
                })
            }
            (usable, _) => {
                // Count the failure and lock at the threshold. A lock that
                // has expired starts the count again. Failures without a
                // usable account run the same statement against no row, so
                // every failure costs the same database work.
                let id = usable.map_or(Uuid::nil(), |a| a.0);
                sqlx::query(
                    "UPDATE identity_accounts SET \
                        failed_logins = CASE WHEN locked_until <= now() THEN 1 \
                            ELSE failed_logins + 1 END, \
                        locked_until = CASE \
                            WHEN (CASE WHEN locked_until <= now() THEN 1 \
                                ELSE failed_logins + 1 END) >= $2 \
                                THEN now() + make_interval(secs => $3) \
                            WHEN locked_until <= now() THEN NULL \
                            ELSE locked_until END \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(self.policy.lock_after)
                .bind(self.policy.lock_for.as_secs_f64())
                .execute(&self.pool)
                .await?;
                Err(login_failed())
            }
        }
    }
}

/// A password that matched: the account, the exact hash it matched, and a
/// stronger hash of it if the stored one is out of date.
struct Verified {
    account: Uuid,
    actor: ActorId,
    phc: String,
    upgraded: Option<String>,
}

fn login_failed() -> Problem {
    Problem::unauthenticated("login_failed")
        .with_detail("The email or password is not right, or the account is locked for a while.")
}

/// A verified hash of a random secret: the target for sign-in attempts
/// with no usable account, so they cost the same as real ones.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password(&random::token()).expect("hashing a random secret"));

fn hasher() -> Argon2<'static> {
    // Argon2id, version 0x13, m = 19 MiB, t = 2, p = 1 (OWASP).
    Argon2::default()
}

pub fn hash_password(password: &str) -> Result<String, String> {
    hasher()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|err| err.to_string())
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|parsed| {
        hasher()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Whether `phc` was made with parameters weaker than the current ones.
fn needs_rehash(phc: &str) -> bool {
    let current = Params::default();
    match PasswordHash::new(phc) {
        Ok(parsed) => {
            parsed.algorithm.as_str() != "argon2id"
                || Params::try_from(&parsed).is_ok_and(|p| {
                    p.m_cost() < current.m_cost()
                        || p.t_cost() < current.t_cost()
                        || p.p_cost() < current.p_cost()
                })
        }
        Err(_) => true,
    }
}

static COMMON: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    include_str!("../data/common-passwords.txt")
        .lines()
        .collect()
});

/// Lowercased, trimmed, and shaped like an address. Not a deliverability
/// check: an invite proves the person controls the mailbox.
pub fn normalize_email(raw: &str) -> Result<String, &'static str> {
    let email = raw.trim().to_lowercase();
    let mut parts = email.split('@');
    let (local, domain) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let valid = parts.next().is_none()
        && (3..=254).contains(&email.len())
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    if valid {
        Ok(email)
    } else {
        Err("not a valid email address")
    }
}

/// Django's validators, in one place: length, common passwords, entirely
/// numeric, similarity to the email address.
pub fn password_problems(
    password: &str,
    email: &str,
    policy: &PasswordPolicy,
    pointer: &str,
) -> Vec<FieldError> {
    let mut problems = Vec::new();
    let chars = password.chars().count();
    if chars < policy.min_chars {
        problems.push(FieldError::new(
            pointer,
            "too_short",
            format!("Use at least {} characters.", policy.min_chars),
        ));
    }
    if chars > policy.max_chars {
        problems.push(FieldError::new(
            pointer,
            "too_long",
            format!("Use at most {} characters.", policy.max_chars),
        ));
    }
    let lower = password.to_lowercase();
    if !password.is_empty() && password.chars().all(|c| c.is_ascii_digit()) {
        problems.push(FieldError::new(
            pointer,
            "entirely_numeric",
            "Use more than digits.",
        ));
    }
    if COMMON.contains(lower.as_str()) {
        problems.push(FieldError::new(
            pointer,
            "too_common",
            "This password is too common.",
        ));
    }
    let local = email.split('@').next().unwrap_or("");
    if (local.chars().count() >= 3 && lower.contains(local))
        || (chars >= 3 && email.contains(lower.as_str()))
    {
        problems.push(FieldError::new(
            pointer,
            "too_similar",
            "Don't base the password on your email address.",
        ));
    }
    problems
}

// Operator commands ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Invite,
    Reset,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Invite => "invite",
            Purpose::Reset => "reset",
        }
    }
}

/// A freshly issued link secret. Shown once; only its digest is stored.
/// `Debug` hides the token.
#[derive(Clone)]
pub struct Issued {
    pub email: String,
    pub actor: ActorId,
    pub purpose: Purpose,
    pub token: String,
    pub expires_at: String,
    pub created_account: bool,
}

impl std::fmt::Debug for Issued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issued")
            .field("email", &self.email)
            .field("actor", &self.actor)
            .field("purpose", &self.purpose)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("created_account", &self.created_account)
            .finish()
    }
}

impl Issued {
    /// The link to send: `<origin>/set-password#token=…`.
    pub fn link(&self, origin: &str) -> String {
        format!(
            "{}{SET_PASSWORD_ROUTE}#token={}",
            origin.trim_end_matches('/'),
            self.token
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("{0}")]
    InvalidEmail(&'static str),
    #[error("no account for {0}")]
    NotFound(String),
    #[error("{0} already has a password; issue a reset link instead")]
    AlreadyActive(String),
    #[error("{0} is disabled; enable it first")]
    Disabled(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

type AccountRow = (Uuid, Uuid, bool, bool);

async fn lock_account(
    tx: &mut sqlx::PgConnection,
    email: &str,
) -> Result<Option<AccountRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, actor_id, password_hash IS NOT NULL, disabled_at IS NOT NULL \
         FROM identity_accounts WHERE email = $1 FOR UPDATE",
    )
    .bind(email)
    .fetch_optional(&mut *tx)
    .await
}

async fn issue_token(
    tx: &mut sqlx::PgConnection,
    account: Uuid,
    purpose: Purpose,
    valid_for: Duration,
) -> Result<(String, String), sqlx::Error> {
    sqlx::query("DELETE FROM identity_account_tokens WHERE account_id = $1 AND used_at IS NULL")
        .bind(account)
        .execute(&mut *tx)
        .await?;
    let token = random::token();
    let expires_at: String = sqlx::query_scalar(
        "INSERT INTO identity_account_tokens (id, account_id, purpose, token_hash, expires_at) \
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5)) \
         RETURNING to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
    )
    .bind(Uuid::now_v7())
    .bind(account)
    .bind(purpose.as_str())
    .bind(random::digest(&token))
    .bind(valid_for.as_secs_f64())
    .fetch_one(&mut *tx)
    .await?;
    Ok((token, expires_at))
}

/// Creates the account for `email` (or re-invites one that never set a
/// password) and issues an invite link.
pub async fn invite(
    pool: &PgPool,
    email: &str,
    valid_for: Duration,
) -> Result<Issued, AccountError> {
    let mut tx = pool.begin().await?;
    let issued = invite_in(&mut tx, email, valid_for).await?;
    tx.commit().await?;
    Ok(issued)
}

/// [`invite`] inside the caller's transaction (e.g. an admin operation that
/// also writes its own records); nothing is committed here.
pub async fn invite_in(
    tx: &mut sqlx::PgConnection,
    email: &str,
    valid_for: Duration,
) -> Result<Issued, AccountError> {
    let email = normalize_email(email).map_err(AccountError::InvalidEmail)?;
    let (account, actor, created) = match lock_account(&mut *tx, &email).await? {
        Some((_, _, true, _)) => return Err(AccountError::AlreadyActive(email)),
        Some((_, _, _, true)) => return Err(AccountError::Disabled(email)),
        Some((id, actor, false, false)) => (id, actor, false),
        None => {
            let (id, actor) = (Uuid::now_v7(), Uuid::now_v7());
            sqlx::query("INSERT INTO identity_accounts (id, actor_id, email) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(actor)
                .bind(&email)
                .execute(&mut *tx)
                .await?;
            (id, actor, true)
        }
    };
    let (token, expires_at) = issue_token(&mut *tx, account, Purpose::Invite, valid_for).await?;
    Ok(Issued {
        email,
        actor: ActorId(actor),
        purpose: Purpose::Invite,
        token,
        expires_at,
        created_account: created,
    })
}

/// Issues a reset link (an invite link if no password was ever set).
/// Earlier unused links stop working.
pub async fn issue_reset(
    pool: &PgPool,
    email: &str,
    valid_for: Duration,
) -> Result<Issued, AccountError> {
    let mut tx = pool.begin().await?;
    let issued = issue_reset_in(&mut tx, email, valid_for).await?;
    tx.commit().await?;
    Ok(issued)
}

/// [`issue_reset`] inside the caller's transaction.
pub async fn issue_reset_in(
    tx: &mut sqlx::PgConnection,
    email: &str,
    valid_for: Duration,
) -> Result<Issued, AccountError> {
    let email = normalize_email(email).map_err(AccountError::InvalidEmail)?;
    let (account, actor, has_password) = match lock_account(&mut *tx, &email).await? {
        None => return Err(AccountError::NotFound(email)),
        Some((_, _, _, true)) => return Err(AccountError::Disabled(email)),
        Some((id, actor, has_password, false)) => (id, actor, has_password),
    };
    let purpose = if has_password {
        Purpose::Reset
    } else {
        Purpose::Invite
    };
    let (token, expires_at) = issue_token(&mut *tx, account, purpose, valid_for).await?;
    Ok(Issued {
        email,
        actor: ActorId(actor),
        purpose,
        token,
        expires_at,
        created_account: false,
    })
}

/// Disables or re-enables an account. Disabling voids outstanding links,
/// ends every session and revokes the actor's API keys (when the API-keys
/// app is installed), all in the transaction that disables it, so a
/// concurrent sign-in either sees the account disabled or has its session
/// revoked here. Re-enabling restores none of them.
pub async fn set_disabled(
    pool: &PgPool,
    email: &str,
    disabled: bool,
) -> Result<ActorId, AccountError> {
    let email = normalize_email(email).map_err(AccountError::InvalidEmail)?;
    let mut tx = pool.begin().await?;
    let actor: Option<Uuid> = sqlx::query_scalar(
        "UPDATE identity_accounts SET \
            disabled_at = CASE WHEN $2 THEN coalesce(disabled_at, now()) ELSE NULL END, \
            failed_logins = 0, locked_until = NULL \
         WHERE email = $1 RETURNING actor_id",
    )
    .bind(&email)
    .bind(disabled)
    .fetch_optional(&mut *tx)
    .await?;
    let actor = ActorId(actor.ok_or(AccountError::NotFound(email))?);
    if disabled {
        sqlx::query(
            "DELETE FROM identity_account_tokens WHERE used_at IS NULL \
             AND account_id = (SELECT id FROM identity_accounts WHERE actor_id = $1)",
        )
        .bind(actor.0)
        .execute(&mut *tx)
        .await?;
        crate::session::revoke_actor_in(&mut tx, actor).await?;
        let has_keys: bool = sqlx::query_scalar("SELECT to_regclass('apikeys_keys') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
        if has_keys {
            crate::api_keys::revoke_actor_in(&mut tx, actor).await?;
        }
    }
    tx.commit().await?;
    Ok(actor)
}

#[derive(Debug, Clone)]
pub struct AccountSummary {
    pub email: String,
    pub actor: ActorId,
    /// `invited`, `active`, `locked` or `disabled`.
    pub status: &'static str,
    pub created_at: String,
}

pub async fn list(pool: &PgPool) -> Result<Vec<AccountSummary>, sqlx::Error> {
    let rows: Vec<(String, Uuid, bool, bool, bool, String)> = sqlx::query_as(
        "SELECT email, actor_id, disabled_at IS NOT NULL, coalesce(locked_until > now(), false), \
                password_hash IS NOT NULL, \
                to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM identity_accounts ORDER BY email",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(email, actor, disabled, locked, has_password, created_at)| AccountSummary {
                email,
                actor: ActorId(actor),
                status: match (disabled, locked, has_password) {
                    (true, _, _) => "disabled",
                    (_, true, _) => "locked",
                    (_, _, true) => "active",
                    _ => "invited",
                },
                created_at,
            },
        )
        .collect())
}

// HTTP ------------------------------------------------------------------------

/// Sign-in for native clients that hold their own credential instead of a
/// cookie (for example a mobile app). Same throttle, same single failure and
/// the same cost as the browser route; no session or cookie is created and
/// there is no `Origin` to check. The caller issues its own credential.
pub async fn sign_in(
    browser: &Browser,
    parts: &Parts,
    email: &str,
    password: String,
) -> Result<ActorId, Problem> {
    let (_, passwords) = setup(browser)?;
    passwords.throttle(parts)?;
    passwords.authenticate(email, password).await
}

fn setup(browser: &Browser) -> Result<(&Arc<Sessions>, &Arc<Passwords>), Problem> {
    let auth = browser.get()?;
    let passwords = auth.passwords.as_ref().ok_or_else(Problem::not_found)?;
    Ok((&auth.sessions, passwords))
}

fn input<T: DeserializeOwned>(value: Value, shape: &str) -> Result<T, Problem> {
    serde_json::from_value(value).map_err(|_| {
        Problem::malformed("invalid_body", &format!("Expected a JSON object {shape}."))
    })
}

fn token_invalid() -> Problem {
    Problem::malformed(
        "token_invalid",
        "This link is no longer valid. Ask your administrator for a new one.",
    )
}

/// 200 with the session info and a fresh session cookie, ending any
/// session this browser held before.
async fn signed_in(
    sessions: &Sessions,
    parts: &Parts,
    actor: ActorId,
    token: String,
) -> Result<Response, Problem> {
    if let Some(previous) = cookie(&parts.headers, SESSION_COOKIE) {
        sessions.revoke(previous).await?;
    }
    let info = sessions.info_for(actor, &token).await?;
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, session_cookie(sessions, &token));
    Ok((StatusCode::OK, headers, Json(info)).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginInput {
    email: String,
    password: String,
}

pub(crate) async fn login(
    State(browser): State<Browser>,
    parts: Parts,
    JsonBody(body): JsonBody,
) -> Result<Response, Problem> {
    let (sessions, passwords) = setup(&browser)?;
    sessions.check_origin(&parts.headers)?;
    passwords.throttle(&parts)?;
    let body: LoginInput = input(body, "{email, password}")?;
    let (actor, token) = passwords
        .login_session(sessions, &body.email, body.password)
        .await?;
    tracing::info!(%actor, "password sign-in");
    signed_in(sessions, &parts, actor, token).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenInput {
    token: String,
}

type TokenRow = (Uuid, Uuid, String, String, String);

/// The account and purpose of a live link, if `token` is one.
async fn live_token(pool: &PgPool, token: &str) -> Result<Option<TokenRow>, sqlx::Error> {
    if !random::is_token(token) {
        return Ok(None);
    }
    sqlx::query_as(
        "SELECT t.id, a.id, a.email, t.purpose, \
                to_char(t.expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
         FROM identity_account_tokens t JOIN identity_accounts a ON a.id = t.account_id \
         WHERE t.token_hash = $1 AND t.used_at IS NULL AND t.expires_at > now() \
           AND a.disabled_at IS NULL",
    )
    .bind(random::digest(token))
    .fetch_optional(pool)
    .await
}

pub(crate) async fn inspect(
    State(browser): State<Browser>,
    parts: Parts,
    JsonBody(body): JsonBody,
) -> Result<Response, Problem> {
    let (sessions, passwords) = setup(&browser)?;
    sessions.check_origin(&parts.headers)?;
    passwords.throttle(&parts)?;
    let body: TokenInput = input(body, "{token}")?;
    let (_, _, email, purpose, expires_at) = live_token(&passwords.pool, &body.token)
        .await?
        .ok_or_else(token_invalid)?;
    Ok(
        Json(json!({ "email": email, "purpose": purpose, "expires_at": expires_at }))
            .into_response(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetInput {
    token: String,
    password: String,
}

pub(crate) async fn set(
    State(browser): State<Browser>,
    parts: Parts,
    JsonBody(body): JsonBody,
) -> Result<Response, Problem> {
    let (sessions, passwords) = setup(&browser)?;
    sessions.check_origin(&parts.headers)?;
    passwords.throttle(&parts)?;
    let body: SetInput = input(body, "{token, password}")?;
    let (actor, token) = passwords
        .set_from_link(sessions, &body.token, body.password)
        .await?;
    tracing::info!(%actor, "password set from link");
    signed_in(sessions, &parts, actor, token).await
}

impl Passwords {
    /// Sets the password from an invite or reset link, ends every other
    /// session of the account and starts a new one.
    pub(crate) async fn set_from_link(
        &self,
        sessions: &Sessions,
        link: &str,
        password: String,
    ) -> Result<(ActorId, String), Problem> {
        let (_, _, email, _, _) = live_token(&self.pool, link)
            .await?
            .ok_or_else(token_invalid)?;
        let problems = password_problems(&password, &email, &self.policy, "/password");
        if !problems.is_empty() {
            return Err(Problem::validation(problems));
        }
        let hash = self.hash(password).await?;

        // Lock the account row first, then consume the link: the order that
        // issuing, changing a password and disabling use. A password change
        // or a newer link that commits while this waits has already deleted
        // the link, so the consuming statement (a fresh snapshot after the
        // lock) finds nothing and the link is refused. The link works once
        // even if submitted twice.
        let digest = random::digest(link);
        let mut tx = self.pool.begin().await?;
        let locked: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT a.id, a.actor_id FROM identity_accounts a \
             WHERE a.id = (SELECT t.account_id FROM identity_account_tokens t \
                           WHERE t.token_hash = $1) \
               AND a.disabled_at IS NULL \
             FOR UPDATE",
        )
        .bind(&digest)
        .fetch_optional(&mut *tx)
        .await?;
        let (account, actor) = locked.ok_or_else(token_invalid)?;
        let consumed = sqlx::query(
            "UPDATE identity_account_tokens SET used_at = now() \
             WHERE token_hash = $1 AND account_id = $2 \
               AND used_at IS NULL AND expires_at > now()",
        )
        .bind(&digest)
        .bind(account)
        .execute(&mut *tx)
        .await?;
        if consumed.rows_affected() != 1 {
            return Err(token_invalid());
        }
        let actor = ActorId(actor);
        sqlx::query(
            "UPDATE identity_accounts SET password_hash = $2, password_changed_at = now(), \
            failed_logins = 0, locked_until = NULL WHERE id = $1",
        )
        .bind(account)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM identity_account_tokens WHERE account_id = $1 AND used_at IS NULL",
        )
        .bind(account)
        .execute(&mut *tx)
        .await?;
        // A reset ends every existing session (someone else may hold one),
        // in the same transaction, so no window leaves them alive.
        crate::session::revoke_actor_in(&mut tx, actor).await?;
        tx.commit().await?;
        // The new session is issued only if the password set here is
        // still the current one and the account is still enabled.
        let verified = Verified {
            account,
            actor,
            phc: hash,
            upgraded: None,
        };
        let mut tx = self.pool.begin().await?;
        self.confirm(&mut tx, &verified).await?;
        let token = sessions.create_in(&mut tx, actor).await?;
        tx.commit().await?;
        Ok((actor, token))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeInput {
    current_password: String,
    new_password: String,
}

pub(crate) async fn change(
    State(browser): State<Browser>,
    parts: Parts,
    JsonBody(body): JsonBody,
) -> Result<Response, Problem> {
    let (sessions, passwords) = setup(&browser)?;
    // Session cookie with origin and CSRF checks.
    let (actor, _) = sessions.authenticate(&parts).await?;
    passwords.throttle(&parts)?;
    let body: ChangeInput = input(body, "{current_password, new_password}")?;
    let current = cookie(&parts.headers, SESSION_COOKIE).unwrap_or_default();
    let ended = passwords
        .change_password(actor, current, body.current_password, body.new_password)
        .await?;
    tracing::info!(%actor, other_sessions_ended = ended, "password changed");
    Ok((StatusCode::NO_CONTENT, HeaderMap::new()).into_response())
}

impl Passwords {
    /// Changes the signed-in actor's password after checking the current
    /// one, voiding every unused invite or reset link and ending every
    /// other session (all but `keep`). Returns how many sessions ended.
    pub(crate) async fn change_password(
        &self,
        actor: ActorId,
        keep: &str,
        current_password: String,
        new_password: String,
    ) -> Result<u64, Problem> {
        let email: Option<String> = sqlx::query_scalar(
            "SELECT email FROM identity_accounts WHERE actor_id = $1 AND disabled_at IS NULL",
        )
        .bind(actor.0)
        .fetch_optional(&self.pool)
        .await?;
        // Signed in some other way (OIDC): there is no password to change.
        let email = email.ok_or_else(|| {
            Problem::forbidden("no_password_account")
                .with_detail("This account signs in through the identity provider.")
        })?;
        let incorrect = || {
            Problem::validation(vec![FieldError::new(
                "/current_password",
                "incorrect",
                "Your current password is not right.",
            )])
        };
        let verified = match self.verify_credentials(&email, current_password).await {
            Ok(verified) if verified.actor == actor => verified,
            _ => return Err(incorrect()),
        };
        let problems = password_problems(&new_password, &email, &self.policy, "/new_password");
        if !problems.is_empty() {
            return Err(Problem::validation(problems));
        }
        let hash = self.hash(new_password).await?;
        // Store it only if the password checked above is still current,
        // and end the other sessions in the same transaction.
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE identity_accounts SET password_hash = $3, password_changed_at = now(), \
                failed_logins = 0, locked_until = NULL \
             WHERE id = $1 AND password_hash = $2 AND disabled_at IS NULL",
        )
        .bind(verified.account)
        .bind(&verified.phc)
        .bind(hash)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(incorrect());
        }
        // Outstanding invite and reset links were issued for the old
        // password; void them here, after the account row is locked by the
        // update above (account first, then links, like issuing and
        // redeeming), so a redemption racing this either commits first (and
        // the update above then finds a different hash) or finds no link.
        sqlx::query(
            "DELETE FROM identity_account_tokens WHERE account_id = $1 AND used_at IS NULL",
        )
        .bind(verified.account)
        .execute(&mut *tx)
        .await?;
        let ended = crate::session::revoke_others_in(&mut tx, actor, keep).await?;
        tx.commit().await?;
        Ok(ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_are_salted() {
        let a = hash_password("correct horse battery staple").unwrap();
        let b = hash_password("correct horse battery staple").unwrap();
        assert!(a.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{a}");
        assert_ne!(a, b, "a random salt per hash");
        assert!(verify_password("correct horse battery staple", &a));
        assert!(!verify_password("correct horse battery stapler", &a));
        assert!(!verify_password("anything", "not a phc string"));
        assert!(!needs_rehash(&a));
        let weak = "$argon2id$v=19$m=4096,t=1,p=1$c2FsdHNhbHRzYWx0$\
                    /9UAbaq1U8M4WzwNd8Lt4vyOUz6lNGZSwbaR9AJ8OQE";
        assert!(needs_rehash(weak));
    }

    #[tokio::test]
    async fn a_cancelled_hash_keeps_its_permit_until_the_work_ends() {
        let pool = sqlx::PgPool::connect_lazy("postgres://localhost/unused").unwrap();
        let policy = PasswordPolicy {
            concurrent_hashes: 1,
            ..PasswordPolicy::default()
        };
        let passwords = Passwords::new(pool, policy, None);
        let task = {
            let passwords = passwords.clone();
            tokio::spawn(async move { passwords.hash("x".repeat(256)).await })
        };
        // Let the blocking hash start, then cancel the request.
        while passwords.hashing.available_permits() == 1 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        task.abort();
        let _ = task.await;
        assert_eq!(
            passwords.hashing.available_permits(),
            0,
            "the hash is still running, so its permit is still held"
        );
        // And it is released once the work finishes.
        let released = tokio::time::timeout(Duration::from_secs(30), async {
            while passwords.hashing.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(released.is_ok());
    }

    #[test]
    fn emails_are_normalized_and_shaped() {
        assert_eq!(
            normalize_email("  Alice@Example.COM ").unwrap(),
            "alice@example.com"
        );
        for bad in [
            "",
            "alice",
            "@example.com",
            "alice@",
            "alice@example",
            "a@b@c.com",
            "al ice@example.com",
            "alice@.com",
            "alice@example.com.",
        ] {
            assert!(normalize_email(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn password_rules_match_djangos_validators() {
        let policy = PasswordPolicy::default();
        let codes = |password: &str| -> Vec<String> {
            password_problems(password, "teacher.jones@example.com", &policy, "/password")
                .into_iter()
                .map(|e| e.code)
                .collect()
        };
        assert!(codes("maple syrup on snow days").is_empty());
        assert_eq!(codes("short"), ["too_short"]);
        assert_eq!(codes("12345678901"), ["entirely_numeric", "too_common"]);
        assert_eq!(codes("password123"), ["too_common"]);
        assert_eq!(codes("Teacher.Jones2026"), ["too_similar"]);
        assert_eq!(codes(&"x".repeat(300)), ["too_long"]);
        let errors = password_problems("short", "a@example.com", &policy, "/new_password");
        assert_eq!(errors[0].pointer, "/new_password");
    }

    #[test]
    fn links_carry_the_token_in_the_fragment() {
        let issued = Issued {
            email: "a@example.com".into(),
            actor: ActorId(Uuid::nil()),
            purpose: Purpose::Invite,
            token: "t".repeat(43),
            expires_at: String::new(),
            created_account: true,
        };
        assert!(
            !format!("{issued:?}").contains(&"t".repeat(43)),
            "Debug hides the link secret"
        );
        assert_eq!(
            issued.link("https://app.example.com/"),
            format!(
                "https://app.example.com/set-password#token={}",
                "t".repeat(43)
            )
        );
    }
}
