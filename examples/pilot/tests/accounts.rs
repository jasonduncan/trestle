//! Password accounts with operator-issued invite and reset links.

mod support;

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, StatusCode, header};
use pilot::AppState;
use pilot::config::Environment;
use pilot::dev::NORTHSIDE;
use pilot::schools::{Role, SchoolId};
use pilot::server::ServeConfig;
use serde_json::{Value, json};
use support::{Reply, TestEnv, send};
use trestle_identity::password::{self, AccountError, Issued};

const ORIGIN: &str = "http://localhost:5173";
const GOOD: &str = "maple syrup on snow days";
const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

struct Harness {
    env: TestEnv,
    app: Router,
}

impl Harness {
    async fn new() -> Self {
        Self::configured(|_| {}).await
    }

    async fn configured(adjust: impl FnOnce(&mut ServeConfig)) -> Self {
        let env = TestEnv::new().await;
        let mut config = ServeConfig::for_tests();
        config.dev_tokens = Some(pilot::dev::DEV_TOKENS.into());
        config.session_key = Some("a session key that is long enough to use".into());
        config.password_login = true;
        config.public_origin = vec![ORIGIN.into()];
        adjust(&mut config);
        let (identity, browser) = config
            .identity(Environment::Development, &env.pool)
            .await
            .unwrap();
        let app = pilot::router(
            &pilot::registry().unwrap(),
            AppState {
                pool: env.pool.clone(),
                identity,
                browser,
            },
        );
        Self { env, app }
    }

    async fn post(&self, path: &str, body: Value, headers: &[(&str, &str)]) -> Reply {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        send(
            &self.app,
            builder.body(Body::from(body.to_string())).unwrap(),
        )
        .await
    }

    /// POST from the product origin, as the browser would.
    async fn browser_post(&self, path: &str, body: Value) -> Reply {
        self.post(path, body, &[("origin", ORIGIN)]).await
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        let mut builder = Request::builder().method(Method::GET).uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        send(&self.app, builder.body(Body::empty()).unwrap()).await
    }

    /// Invite, add to Northside as a teacher, and set `GOOD` as the
    /// password. Returns the session cookie value.
    async fn onboard(&self, email: &str) -> (Issued, String) {
        let issued = password::invite(&self.env.pool, email, WEEK).await.unwrap();
        pilot::provision::set_membership(
            &self.env.pool,
            SchoolId(NORTHSIDE),
            issued.actor,
            Role::Teacher,
        )
        .await
        .unwrap();
        let set = self
            .browser_post(
                password::SET_PATH,
                json!({ "token": issued.token, "password": GOOD }),
            )
            .await;
        assert_eq!(set.status, StatusCode::OK, "{}", set.body);
        let session = session_cookie(&set.headers).unwrap();
        (issued, session)
    }

    async fn login(&self, email: &str, password: &str) -> Reply {
        self.browser_post(
            password::LOGIN_PATH,
            json!({ "email": email, "password": password }),
        )
        .await
    }

    async fn session_status(&self, session: &str) -> StatusCode {
        self.get("/api/v1/auth/session", &[("cookie", &cookie(session))])
            .await
            .status
    }
}

fn cookie(session: &str) -> String {
    format!("__Host-trestle_session={session}")
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix("__Host-trestle_session="))
        .map(|rest| rest.split(';').next().unwrap().to_owned())
}

#[tokio::test]
async fn an_invited_person_sets_a_password_and_works_normally() {
    let h = Harness::new().await;
    let issued = password::invite(&h.env.pool, "  Teacher.One@Example.COM ", WEEK)
        .await
        .unwrap();
    assert_eq!(issued.email, "teacher.one@example.com");
    assert!(issued.created_account);
    assert_eq!(
        issued.link("https://app.example.com"),
        format!(
            "https://app.example.com/set-password#token={}",
            issued.token
        )
    );
    pilot::provision::set_membership(
        &h.env.pool,
        SchoolId(NORTHSIDE),
        issued.actor,
        Role::Teacher,
    )
    .await
    .unwrap();

    let inspected = h
        .browser_post(password::INSPECT_PATH, json!({ "token": issued.token }))
        .await;
    assert_eq!(inspected.status, StatusCode::OK);
    assert_eq!(inspected.body["email"], "teacher.one@example.com");
    assert_eq!(inspected.body["purpose"], "invite");

    // Django-style rules, reported as field errors; the link survives.
    let weak = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": issued.token, "password": "password123" }),
        )
        .await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(weak.body["errors"][0]["pointer"], "/password");
    assert_eq!(weak.body["errors"][0]["code"], "too_common");

    let set = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": issued.token, "password": GOOD }),
        )
        .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["actor"], issued.actor.to_string());
    let csrf = set.body["csrf_token"].as_str().unwrap().to_owned();
    let session = session_cookie(&set.headers).unwrap();
    let line = set.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(
        line.ends_with("; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=43200"),
        "{line}"
    );

    // The link works once.
    let again = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": issued.token, "password": GOOD }),
        )
        .await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(again.body["code"], "token_invalid");

    // The session is an ordinary browser session.
    let tenant = NORTHSIDE.to_string();
    let created = h
        .post(
            "/api/v1/lessons",
            json!({ "title": "Set by password", "grade_level": 3 }),
            &[
                ("cookie", &cookie(&session)),
                ("origin", ORIGIN),
                ("x-csrf-token", &csrf),
                ("trestle-tenant", &tenant),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);

    let wrong = h
        .login("teacher.one@example.com", "not the password at all")
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let right = h.login("TEACHER.ONE@example.com", GOOD).await;
    assert_eq!(right.status, StatusCode::OK, "{}", right.body);
    assert_eq!(right.body["actor"], issued.actor.to_string());

    let stored: String = sqlx::query_scalar("SELECT password_hash FROM identity_accounts")
        .fetch_one(&h.env.pool)
        .await
        .unwrap();
    assert!(
        stored.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{stored}"
    );
    let tokens: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM identity_account_tokens")
        .fetch_all(&h.env.pool)
        .await
        .unwrap();
    assert!(
        tokens.iter().all(|t| t.len() == 64 && *t != issued.token),
        "only digests are stored"
    );
    h.env.finish().await;
}

#[tokio::test]
async fn sign_in_failures_look_alike_and_lock_the_account() {
    let h = Harness::new().await;
    h.onboard("locked.out@example.com").await;

    let unknown = h.login("nobody@example.com", GOOD).await;
    let wrong = h
        .login("locked.out@example.com", "not the password at all")
        .await;
    for reply in [&unknown, &wrong] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.body["code"], "login_failed");
        assert_eq!(reply.body["detail"], unknown.body["detail"]);
    }

    for _ in 0..4 {
        assert_eq!(
            h.login("locked.out@example.com", "still not it, sorry")
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }
    // Five consecutive failures: now even the right password fails.
    let locked = h.login("locked.out@example.com", GOOD).await;
    assert_eq!(locked.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        locked.body["detail"], unknown.body["detail"],
        "a lock looks like any failure"
    );
    let (failed, locked_until): (i32, bool) = sqlx::query_as(
        "SELECT failed_logins, locked_until > now() FROM identity_accounts WHERE email = 'locked.out@example.com'",
    )
    .fetch_one(&h.env.pool)
    .await
    .unwrap();
    assert_eq!((failed, locked_until), (5, true));

    sqlx::query("UPDATE identity_accounts SET locked_until = now() - interval '1 second'")
        .execute(&h.env.pool)
        .await
        .unwrap();
    let after = h.login("locked.out@example.com", GOOD).await;
    assert_eq!(after.status, StatusCode::OK, "the lock expires");
    let failed: i32 = sqlx::query_scalar("SELECT failed_logins FROM identity_accounts")
        .fetch_one(&h.env.pool)
        .await
        .unwrap();
    assert_eq!(failed, 0, "success resets the count");
    h.env.finish().await;
}

#[tokio::test]
async fn sign_in_attempts_are_limited_per_client_address() {
    let h = Harness::configured(|c| {
        c.client_address_header = Some("x-real-ip".into());
        // Two a minute: a refill takes 30 s, far longer than this test.
        c.password_attempts_per_minute = 2.0;
    })
    .await;
    let attempt = |address: &'static str| {
        let h = &h;
        async move {
            h.post(
                password::LOGIN_PATH,
                json!({ "email": "someone@example.com", "password": "guess guess guess" }),
                &[("origin", ORIGIN), ("x-real-ip", address)],
            )
            .await
        }
    };
    for _ in 0..2 {
        assert_eq!(
            attempt("203.0.113.7").await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    let limited = attempt("203.0.113.7").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.body["code"], "rate_limited");
    assert!(limited.headers.contains_key(header::RETRY_AFTER));
    assert_eq!(
        attempt("198.51.100.9").await.status,
        StatusCode::UNAUTHORIZED,
        "other addresses unaffected"
    );
    h.env.finish().await;
}

#[tokio::test]
async fn sign_in_needs_the_allowed_origin_and_a_json_object() {
    let h = Harness::new().await;
    h.onboard("careful@example.com").await;
    let body = json!({ "email": "careful@example.com", "password": GOOD });
    for (headers, why) in [
        (vec![], "no Origin"),
        (vec![("origin", "https://evil.example")], "another origin"),
        (vec![("origin", "http://localhost:5174")], "another port"),
    ] {
        let refused = h.post(password::LOGIN_PATH, body.clone(), &headers).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{why}");
        assert_eq!(refused.body["code"], "csrf_failed", "{why}");
    }
    let form = send(
        &h.app,
        Request::builder()
            .method(Method::POST)
            .uri(password::LOGIN_PATH)
            .header("origin", ORIGIN)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(
        form.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a cross-site form cannot post JSON"
    );
    let extra = h
        .browser_post(
            password::LOGIN_PATH,
            json!({ "email": "careful@example.com", "password": GOOD, "remember": true }),
        )
        .await;
    assert_eq!(extra.status, StatusCode::BAD_REQUEST);
    assert_eq!(extra.body["code"], "invalid_body");
    h.env.finish().await;
}

#[tokio::test]
async fn reset_links_replace_older_ones_and_end_every_session() {
    let h = Harness::new().await;
    let (_, before) = h.onboard("forgetful@example.com").await;
    assert_eq!(h.session_status(&before).await, StatusCode::OK);

    let first = password::issue_reset(&h.env.pool, "forgetful@example.com", WEEK)
        .await
        .unwrap();
    let second = password::issue_reset(&h.env.pool, "forgetful@example.com", WEEK)
        .await
        .unwrap();
    assert_eq!(second.purpose, password::Purpose::Reset);
    let stale = h
        .browser_post(password::INSPECT_PATH, json!({ "token": first.token }))
        .await;
    assert_eq!(
        stale.body["code"], "token_invalid",
        "a new link voids the old one"
    );
    let current = h
        .browser_post(password::INSPECT_PATH, json!({ "token": second.token }))
        .await;
    assert_eq!(current.body["purpose"], "reset");

    let new_password = "a brand new passphrase";
    let reset = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": second.token, "password": new_password }),
        )
        .await;
    assert_eq!(reset.status, StatusCode::OK, "{}", reset.body);
    assert_eq!(
        h.session_status(&before).await,
        StatusCode::UNAUTHORIZED,
        "old sessions end"
    );
    let after = session_cookie(&reset.headers).unwrap();
    assert_eq!(h.session_status(&after).await, StatusCode::OK);
    assert_eq!(
        h.login("forgetful@example.com", GOOD).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.login("forgetful@example.com", new_password).await.status,
        StatusCode::OK
    );

    let expiring = password::issue_reset(&h.env.pool, "forgetful@example.com", WEEK)
        .await
        .unwrap();
    sqlx::query("UPDATE identity_account_tokens SET expires_at = now() - interval '1 second' WHERE used_at IS NULL")
        .execute(&h.env.pool)
        .await
        .unwrap();
    let expired = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": expiring.token, "password": "yet another passphrase" }),
        )
        .await;
    assert_eq!(expired.body["code"], "token_invalid");
    h.env.finish().await;
}

#[tokio::test]
async fn changing_a_password_needs_the_current_one_and_ends_other_sessions() {
    let h = Harness::new().await;
    let (_, elsewhere) = h.onboard("changer@example.com").await;
    let here = h.login("changer@example.com", GOOD).await;
    let session = session_cookie(&here.headers).unwrap();
    let csrf = here.body["csrf_token"].as_str().unwrap().to_owned();
    let change = |body: Value, csrf: Option<&str>| {
        let h = &h;
        let session = session.clone();
        let csrf = csrf.map(str::to_owned);
        async move {
            let cookie = cookie(&session);
            let mut headers = vec![("cookie", cookie.as_str()), ("origin", ORIGIN)];
            if let Some(token) = &csrf {
                headers.push(("x-csrf-token", token.as_str()));
            }
            h.post(password::CHANGE_PATH, body, &headers).await
        }
    };
    let new_password = "another long passphrase";

    let forged = change(
        json!({ "current_password": GOOD, "new_password": new_password }),
        None,
    )
    .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    let wrong = change(
        json!({ "current_password": "not it at all ok", "new_password": new_password }),
        Some(&csrf),
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(wrong.body["errors"][0]["pointer"], "/current_password");
    let weak = change(
        json!({ "current_password": GOOD, "new_password": "changer1234" }),
        Some(&csrf),
    )
    .await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(weak.body["errors"][0]["pointer"], "/new_password");
    assert_eq!(weak.body["errors"][0]["code"], "too_similar");

    let done = change(
        json!({ "current_password": GOOD, "new_password": new_password }),
        Some(&csrf),
    )
    .await;
    assert_eq!(done.status, StatusCode::NO_CONTENT, "{}", done.body);
    assert_eq!(
        h.session_status(&session).await,
        StatusCode::OK,
        "this session stays"
    );
    assert_eq!(
        h.session_status(&elsewhere).await,
        StatusCode::UNAUTHORIZED,
        "others end"
    );
    assert_eq!(
        h.login("changer@example.com", new_password).await.status,
        StatusCode::OK
    );
    h.env.finish().await;
}

impl Harness {
    /// Changes the password of the signed-in `session` (cookie and CSRF).
    async fn change_password(&self, session: &str, current: &str, new: &str) -> Reply {
        let info = self
            .get("/api/v1/auth/session", &[("cookie", &cookie(session))])
            .await;
        let csrf = info.body["csrf_token"].as_str().unwrap().to_owned();
        self.post(
            password::CHANGE_PATH,
            json!({ "current_password": current, "new_password": new }),
            &[
                ("origin", ORIGIN),
                ("cookie", &cookie(session)),
                ("x-csrf-token", &csrf),
            ],
        )
        .await
    }
}

/// SR-02: a reset link issued before a password change stops working.
#[tokio::test]
async fn changing_a_password_voids_outstanding_links() {
    let h = Harness::new().await;
    let (_, session) = h.onboard("rotator@example.com").await;
    let before = password::issue_reset(&h.env.pool, "rotator@example.com", WEEK)
        .await
        .unwrap();

    let changed_to = "new maple syrup on snowy days";
    let changed = h.change_password(&session, GOOD, changed_to).await;
    assert_eq!(changed.status, StatusCode::NO_CONTENT, "{}", changed.body);

    let inspect = h
        .browser_post(password::INSPECT_PATH, json!({ "token": before.token }))
        .await;
    assert_eq!(inspect.body["code"], "token_invalid");
    let takeover = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": before.token, "password": "someone else's passphrase" }),
        )
        .await;
    assert_eq!(takeover.body["code"], "token_invalid", "{}", takeover.body);
    assert_eq!(
        h.login("rotator@example.com", "someone else's passphrase")
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(h.session_status(&session).await, StatusCode::OK);

    // A link issued after the change works as usual.
    let after = password::issue_reset(&h.env.pool, "rotator@example.com", WEEK)
        .await
        .unwrap();
    let reset_to = "a later passphrase for real";
    let reset = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": after.token, "password": reset_to }),
        )
        .await;
    assert_eq!(reset.status, StatusCode::OK, "{}", reset.body);
    assert_eq!(
        h.login("rotator@example.com", reset_to).await.status,
        StatusCode::OK
    );
    assert_eq!(
        h.login("rotator@example.com", changed_to).await.status,
        StatusCode::UNAUTHORIZED
    );
    h.env.finish().await;
}

/// SR-02 ordering. Redemption locks the account row before the link, the
/// order a password change uses (its update locks the row, then it deletes
/// the links). Whichever commits first wins: a redemption that waited on a
/// change finds no link; a change after a redemption finds the current
/// password no longer matches, and the redemption already ended its session.
#[tokio::test]
async fn a_change_and_a_redemption_serialize_on_the_account() {
    let h = Harness::new().await;
    let (_, session) = h.onboard("racer@example.com").await;
    let link = password::issue_reset(&h.env.pool, "racer@example.com", WEEK)
        .await
        .unwrap();

    // Hold the account row the way a committing password change does, let
    // the redemption queue behind it, then void the links and commit.
    let mut change = h.env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM identity_accounts WHERE email = 'racer@example.com' FOR UPDATE")
        .execute(&mut *change)
        .await
        .unwrap();
    let redeem = h.browser_post(
        password::SET_PATH,
        json!({ "token": link.token, "password": "the racing passphrase" }),
    );
    let finish_change = async {
        let waiting = async {
            loop {
                let blocked: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock'",
                )
                .fetch_one(&h.env.pool)
                .await
                .unwrap();
                if blocked > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the redemption waits on the account row");
        sqlx::query(
            "DELETE FROM identity_account_tokens WHERE used_at IS NULL AND account_id = \
             (SELECT id FROM identity_accounts WHERE email = 'racer@example.com')",
        )
        .execute(&mut *change)
        .await
        .unwrap();
        change.commit().await.unwrap();
    };
    let (redeemed, ()) = tokio::join!(redeem, finish_change);
    assert_eq!(redeemed.body["code"], "token_invalid", "{}", redeemed.body);
    assert_eq!(
        h.login("racer@example.com", GOOD).await.status,
        StatusCode::OK,
        "the refused link changed nothing"
    );

    // The other order: the link is redeemed first, so a change that checked
    // the old password is refused and its session has already ended.
    let link = password::issue_reset(&h.env.pool, "racer@example.com", WEEK)
        .await
        .unwrap();
    let redeemed = h
        .browser_post(
            password::SET_PATH,
            json!({ "token": link.token, "password": "the winning passphrase" }),
        )
        .await;
    assert_eq!(redeemed.status, StatusCode::OK, "{}", redeemed.body);
    assert_eq!(h.session_status(&session).await, StatusCode::UNAUTHORIZED);
    let fresh = session_cookie(&redeemed.headers).unwrap();
    let late = h
        .change_password(&fresh, GOOD, "the losing passphrase ok")
        .await;
    assert_eq!(late.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(late.body["errors"][0]["pointer"], "/current_password");
    assert_eq!(
        h.login("racer@example.com", "the winning passphrase")
            .await
            .status,
        StatusCode::OK
    );
    h.env.finish().await;
}

#[tokio::test]
async fn disabled_accounts_cannot_sign_in_or_use_links() {
    let h = Harness::new().await;
    let (issued, session) = h.onboard("leaver@example.com").await;
    assert!(matches!(
        password::invite(&h.env.pool, "leaver@example.com", WEEK).await,
        Err(AccountError::AlreadyActive(_))
    ));
    let pending = password::issue_reset(&h.env.pool, "leaver@example.com", WEEK)
        .await
        .unwrap();

    let actor = password::set_disabled(&h.env.pool, "leaver@example.com", true)
        .await
        .unwrap();
    assert_eq!(actor, issued.actor);
    assert_eq!(h.session_status(&session).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.login("leaver@example.com", GOOD).await.status,
        StatusCode::UNAUTHORIZED
    );
    let link = h
        .browser_post(password::INSPECT_PATH, json!({ "token": pending.token }))
        .await;
    assert_eq!(link.body["code"], "token_invalid");
    assert!(matches!(
        password::issue_reset(&h.env.pool, "leaver@example.com", WEEK).await,
        Err(AccountError::Disabled(_))
    ));
    let listed = password::list(&h.env.pool).await.unwrap();
    assert_eq!(listed[0].status, "disabled");

    password::set_disabled(&h.env.pool, "leaver@example.com", false)
        .await
        .unwrap();
    assert_eq!(
        h.login("leaver@example.com", GOOD).await.status,
        StatusCode::OK
    );
    h.env.finish().await;
}

#[tokio::test]
async fn the_sign_in_screen_learns_which_methods_exist() {
    let h = Harness::new().await;
    let methods = h.get("/api/v1/auth/methods", &[]).await;
    assert_eq!(methods.status, StatusCode::OK);
    assert_eq!(methods.body, json!({ "password": true, "oidc": false }));
    h.env.finish().await;

    // Development tokens only: no browser sign-in at all.
    let env = TestEnv::new().await;
    let app = pilot::router(&pilot::registry().unwrap(), env.state.clone());
    for path in ["/api/v1/auth/methods", password::LOGIN_PATH] {
        let reply = send(
            &app,
            Request::builder().uri(path).body(Body::empty()).unwrap(),
        )
        .await;
        assert!(
            matches!(
                reply.status,
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{path}: {}",
            reply.status
        );
    }
    env.finish().await;
}
