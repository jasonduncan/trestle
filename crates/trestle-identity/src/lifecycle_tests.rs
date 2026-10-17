//! Account lifecycle against a real database: sign-in racing disable,
//! reset and change; API keys of disabled accounts; lockout accounting.

use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::PasswordHasher;
use argon2::{Algorithm, Argon2, Params, Version};
use http::{Method, Request, header};
use sqlx::PgPool;
use trestle_core::identity::ActorId;
use trestle_historical::planner::{PlanOptions, diff};
use trestle_historical::schema::SchemaState;
use trestle_historical::testing::TestDatabase;

use crate::api_keys;
use crate::oidc::Transport;
use crate::password::{self, PasswordPolicy, Passwords, Pause, hash_password, verify_password};
use crate::session::{SESSION_COOKIE, SessionConfig, Sessions};
use crate::{account_model, account_token_model, login_model, session_model};

const OLD: &str = "maple syrup on snow days";
const NEW: &str = "a different long passphrase";
const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

struct Env {
    db: TestDatabase,
    pool: PgPool,
    passwords: Arc<Passwords>,
    sessions: Arc<Sessions>,
}

impl Env {
    async fn new() -> Self {
        let db = TestDatabase::create().await;
        let pool = db.pool().await;
        let mut desired = SchemaState::default();
        for model in [
            session_model(),
            login_model(),
            account_model(),
            account_token_model(),
            api_keys::api_key_model(),
        ] {
            let state = model.state();
            desired.models.insert(state.id.clone(), state);
        }
        let before = SchemaState::default();
        let plan = diff(&before, &desired, &PlanOptions::default());
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
        let ops: Vec<_> = plan.changes.into_iter().flat_map(|c| c.ops).collect();
        let sql = trestle_historical::sql::render(&before, &ops).unwrap();
        sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
        let config = SessionConfig::new(
            &[7u8; 32],
            &["https://app.example.com".to_owned()],
            Transport::HttpsOnly,
        )
        .unwrap();
        Self {
            sessions: Sessions::new(pool.clone(), config),
            passwords: Passwords::new(pool.clone(), PasswordPolicy::default(), None),
            pool,
            db,
        }
    }

    /// An active account whose password hash is `phc`.
    async fn account(&self, email: &str, phc: &str) -> ActorId {
        let issued = password::invite(&self.pool, email, WEEK).await.unwrap();
        sqlx::query("UPDATE identity_accounts SET password_hash = $2 WHERE email = $1")
            .bind(email)
            .bind(phc)
            .execute(&self.pool)
            .await
            .unwrap();
        issued.actor
    }

    async fn session_live(&self, token: &str) -> bool {
        let parts = Request::builder()
            .method(Method::GET)
            .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
            .body(())
            .unwrap()
            .into_parts()
            .0;
        self.sessions.authenticate(&parts).await.is_ok()
    }

    /// Starts a sign-in that stops right after verifying the password.
    async fn paused_login(
        &self,
        email: &str,
        password: &str,
    ) -> (
        Arc<Pause>,
        tokio::task::JoinHandle<Result<(ActorId, String), trestle_core::Problem>>,
    ) {
        let pause = Pause::new();
        *self.passwords.pause.lock().unwrap() = Some(pause.clone());
        let (passwords, sessions) = (self.passwords.clone(), self.sessions.clone());
        let (email, password) = (email.to_owned(), password.to_owned());
        let task =
            tokio::spawn(async move { passwords.login_session(&sessions, &email, password).await });
        pause.reached.wait().await;
        (pause, task)
    }

    async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn weak_hash(password: &str) -> String {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(4096, 1, 1, None).unwrap(),
    )
    .hash_password(password.as_bytes())
    .unwrap()
    .to_string()
}

async fn stored_hash(pool: &PgPool, email: &str) -> String {
    sqlx::query_scalar("SELECT password_hash FROM identity_accounts WHERE email = $1")
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_sign_in_in_flight_during_disable_gets_no_session() {
    let env = Env::new().await;
    env.account("leaver@example.com", &hash_password(OLD).unwrap())
        .await;
    let (pause, login) = env.paused_login("leaver@example.com", OLD).await;
    password::set_disabled(&env.pool, "leaver@example.com", true)
        .await
        .unwrap();
    pause.resume.wait().await;
    match login.await.unwrap() {
        Ok((_, token)) => assert!(
            !env.session_live(&token).await,
            "a session issued across a disable must not be usable"
        ),
        Err(problem) => assert_eq!(problem.status().as_u16(), 401),
    }
    env.finish().await;
}

#[tokio::test]
async fn a_sign_in_in_flight_during_a_reset_gets_no_session() {
    let env = Env::new().await;
    env.account("reset@example.com", &hash_password(OLD).unwrap())
        .await;
    let link = password::issue_reset(&env.pool, "reset@example.com", WEEK)
        .await
        .unwrap();
    let (pause, login) = env.paused_login("reset@example.com", OLD).await;
    env.passwords
        .set_from_link(&env.sessions, &link.token, NEW.into())
        .await
        .unwrap();
    pause.resume.wait().await;
    match login.await.unwrap() {
        Ok((_, token)) => assert!(
            !env.session_live(&token).await,
            "the superseded password must not yield a session"
        ),
        Err(problem) => assert_eq!(problem.status().as_u16(), 401),
    }
    env.finish().await;
}

#[tokio::test]
async fn a_delayed_rehash_does_not_overwrite_a_reset() {
    let env = Env::new().await;
    env.account("weak@example.com", &weak_hash(OLD)).await;
    let link = password::issue_reset(&env.pool, "weak@example.com", WEEK)
        .await
        .unwrap();
    let (pause, login) = env.paused_login("weak@example.com", OLD).await;
    env.passwords
        .set_from_link(&env.sessions, &link.token, NEW.into())
        .await
        .unwrap();
    pause.resume.wait().await;
    let _ = login.await.unwrap();
    let phc = stored_hash(&env.pool, "weak@example.com").await;
    assert!(verify_password(NEW, &phc), "the reset password stands");
    assert!(!verify_password(OLD, &phc), "the old password stays dead");
    env.finish().await;
}

#[tokio::test]
async fn a_password_change_does_not_overwrite_a_concurrent_reset() {
    let env = Env::new().await;
    let actor = env
        .account("change@example.com", &hash_password(OLD).unwrap())
        .await;
    let link = password::issue_reset(&env.pool, "change@example.com", WEEK)
        .await
        .unwrap();
    let keep = env.sessions.create(actor).await.unwrap();
    let pause = Pause::new();
    *env.passwords.pause.lock().unwrap() = Some(pause.clone());
    let passwords = env.passwords.clone();
    let change = tokio::spawn(async move {
        passwords
            .change_password(
                actor,
                &keep,
                OLD.into(),
                "a third quite long passphrase".into(),
            )
            .await
    });
    pause.reached.wait().await;
    env.passwords
        .set_from_link(&env.sessions, &link.token, NEW.into())
        .await
        .unwrap();
    pause.resume.wait().await;
    let _ = change.await.unwrap();
    let phc = stored_hash(&env.pool, "change@example.com").await;
    assert!(
        verify_password(NEW, &phc),
        "a change checked against the superseded password must not land"
    );
    env.finish().await;
}

#[tokio::test]
async fn disabling_an_account_revokes_its_api_keys() {
    let env = Env::new().await;
    let actor = env
        .account("agent@example.com", &hash_password(OLD).unwrap())
        .await;
    let key = api_keys::issue(&env.pool, actor, "script", &[], None)
        .await
        .unwrap();
    assert!(
        api_keys::verify(&env.pool, &key.secret)
            .await
            .unwrap()
            .is_some()
    );
    password::set_disabled(&env.pool, "agent@example.com", true)
        .await
        .unwrap();
    assert!(
        api_keys::verify(&env.pool, &key.secret)
            .await
            .unwrap()
            .is_none(),
        "a disabled account's keys stop working"
    );
    // Re-enabling does not bring old keys back.
    password::set_disabled(&env.pool, "agent@example.com", false)
        .await
        .unwrap();
    assert!(
        api_keys::verify(&env.pool, &key.secret)
            .await
            .unwrap()
            .is_none()
    );
    env.finish().await;
}

#[tokio::test]
async fn disabling_works_without_the_api_keys_app() {
    let env = Env::new().await;
    sqlx::query("DROP TABLE apikeys_keys")
        .execute(&env.pool)
        .await
        .unwrap();
    env.account("plain@example.com", &hash_password(OLD).unwrap())
        .await;
    password::set_disabled(&env.pool, "plain@example.com", true)
        .await
        .unwrap();
    env.finish().await;
}

#[tokio::test]
async fn an_expired_lock_starts_the_count_again() {
    let env = Env::new().await;
    env.account("locked@example.com", &hash_password(OLD).unwrap())
        .await;
    sqlx::query(
        "UPDATE identity_accounts SET failed_logins = 5, \
            locked_until = now() - interval '1 second' WHERE email = $1",
    )
    .bind("locked@example.com")
    .execute(&env.pool)
    .await
    .unwrap();
    assert!(
        env.passwords
            .login_session(&env.sessions, "locked@example.com", "wrong guess".into())
            .await
            .is_err()
    );
    let (failed, locked): (i32, bool) = sqlx::query_as(
        "SELECT failed_logins, coalesce(locked_until > now(), false) \
         FROM identity_accounts WHERE email = $1",
    )
    .bind("locked@example.com")
    .fetch_one(&env.pool)
    .await
    .unwrap();
    assert_eq!(failed, 1, "the count restarts after the lock expires");
    assert!(!locked, "one wrong password after expiry does not re-lock");
    env.finish().await;
}

#[tokio::test]
async fn every_failed_sign_in_makes_the_same_write() {
    let env = Env::new().await;
    env.account("real@example.com", &hash_password(OLD).unwrap())
        .await;
    env.account("off@example.com", &hash_password(OLD).unwrap())
        .await;
    password::set_disabled(&env.pool, "off@example.com", true)
        .await
        .unwrap();
    password::invite(&env.pool, "invited@example.com", WEEK)
        .await
        .unwrap();
    // Count UPDATE statements on identity_accounts, matching rows or not.
    sqlx::raw_sql(
        "CREATE TABLE update_count (n integer NOT NULL);
         INSERT INTO update_count VALUES (0);
         CREATE FUNCTION count_update() RETURNS trigger LANGUAGE plpgsql AS
           $$ BEGIN UPDATE update_count SET n = n + 1; RETURN NULL; END $$;
         CREATE TRIGGER count_update AFTER UPDATE ON identity_accounts
           FOR EACH STATEMENT EXECUTE FUNCTION count_update();",
    )
    .execute(&env.pool)
    .await
    .unwrap();
    let mut counts = Vec::new();
    for email in [
        "real@example.com",
        "nobody@example.com",
        "off@example.com",
        "invited@example.com",
        "not an email",
    ] {
        sqlx::query("UPDATE update_count SET n = 0")
            .execute(&env.pool)
            .await
            .unwrap();
        assert!(
            env.passwords
                .login_session(&env.sessions, email, "wrong guess".into())
                .await
                .is_err()
        );
        let n: i32 = sqlx::query_scalar("SELECT n FROM update_count")
            .fetch_one(&env.pool)
            .await
            .unwrap();
        counts.push((email, n));
    }
    assert!(
        counts.iter().all(|(_, n)| *n == counts[0].1),
        "failures must not differ in database writes: {counts:?}"
    );
    env.finish().await;
}

#[tokio::test]
async fn sign_in_attempts_are_limited_per_account_from_any_address() {
    let env = Env::new().await;
    assert_eq!(PasswordPolicy::default().account_attempts_per_minute, 10.0);
    // The bucket refills continuously: at the default 10/min, one attempt
    // comes back every 6s, and ten full-cost Argon2id verifications can take
    // that long on a small CI runner. Three a minute (one back every 20s)
    // keeps the attempts well inside one refill.
    const LIMIT: usize = 3;
    let passwords = Passwords::new(
        env.pool.clone(),
        PasswordPolicy {
            account_attempts_per_minute: LIMIT as f64,
            ..PasswordPolicy::default()
        },
        None,
    );
    env.account("target@example.com", &weak_hash(OLD)).await;
    // Lockout (5 failures) would refuse with login_failed, so use the right
    // password here.
    for i in 0..LIMIT {
        assert!(
            passwords
                .login_session(&env.sessions, "target@example.com", OLD.into())
                .await
                .is_ok(),
            "attempt {i} allowed"
        );
    }
    let refused = passwords
        .login_session(&env.sessions, " Target@Example.com ", OLD.into())
        .await
        .unwrap_err();
    assert_eq!(refused.status(), http::StatusCode::TOO_MANY_REQUESTS);
    // Unknown accounts are limited the same way, so the limit reveals nothing.
    for _ in 0..LIMIT {
        let _ = passwords
            .login_session(&env.sessions, "nobody@example.com", OLD.into())
            .await;
    }
    let unknown = passwords
        .login_session(&env.sessions, "nobody@example.com", OLD.into())
        .await
        .unwrap_err();
    assert_eq!(unknown.status(), http::StatusCode::TOO_MANY_REQUESTS);
    // Other accounts are unaffected.
    env.account("other@example.com", &weak_hash(OLD)).await;
    assert!(
        passwords
            .login_session(&env.sessions, "other@example.com", OLD.into())
            .await
            .is_ok()
    );
    env.finish().await;
}
