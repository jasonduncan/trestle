//! Production identity (ADR-005): OpenID Connect browser login with
//! server-side sessions, CSRF and origin checks, and JWT bearer tokens for
//! API clients, against a mock provider on a real socket.

mod support;

use std::collections::HashMap;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, StatusCode, header};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use pilot::AppState;
use pilot::config::Environment;
use pilot::dev::NORTHSIDE;
use pilot::schools::{Role, SchoolId};
use pilot::server::ServeConfig;
use serde_json::{Value, json};
use support::idp::{self, MockIdp};
use support::{Reply, TestEnv, send};
use trestle_core::identity::ActorId;
use trestle_identity::actor_for;

const ORIGIN: &str = "http://localhost:5173";
const API_AUDIENCE: &str = "https://api.pilot.test";

struct Harness {
    env: TestEnv,
    idp: MockIdp,
    app: Router,
}

impl Harness {
    async fn new() -> Self {
        let env = TestEnv::new().await;
        let idp = MockIdp::start().await;
        let mut config = ServeConfig::for_tests();
        config.dev_tokens = Some(pilot::dev::DEV_TOKENS.into());
        config.session_key = Some("a session key that is long enough to use".into());
        config.oidc_issuer = Some(idp.issuer.clone());
        config.oidc_client_id = Some(idp.client_id.clone());
        config.oidc_client_secret = Some(idp.client_secret.clone());
        config.oidc_api_audience = Some(API_AUDIENCE.into());
        config.public_origin = vec![ORIGIN.into()];
        let (identity, browser) = config
            .identity(Environment::Development, &env.pool)
            .await
            .expect("identity configures against the mock provider");
        let registry = pilot::registry().unwrap();
        let app = pilot::router(
            &registry,
            AppState {
                pool: env.pool.clone(),
                identity,
                browser,
            },
        );
        Self { env, idp, app }
    }

    fn actor(&self, subject: &str) -> ActorId {
        actor_for(&self.idp.issuer, subject)
    }

    async fn member(&self, subject: &str, role: Role) -> ActorId {
        let actor = self.actor(subject);
        pilot::provision::set_membership(&self.env.pool, SchoolId(NORTHSIDE), actor, role)
            .await
            .unwrap();
        actor
    }

    async fn call(&self, request: Request<Body>) -> Reply {
        send(&self.app, request).await
    }

    /// Runs the whole browser flow; returns the session cookie value.
    async fn login(&self, subject: &str) -> String {
        self.idp.sign_in_as(subject);
        let started = self
            .call(get("/api/v1/auth/login?return_to=/lessons", &[]))
            .await;
        assert_eq!(started.status, StatusCode::SEE_OTHER);
        let state_cookie = set_cookie(&started.headers, "__Host-trestle_login").unwrap();
        let (code, state) = self
            .idp
            .approve(started.headers[header::LOCATION].to_str().unwrap())
            .await;
        let done = self
            .call(get(
                &format!("/api/v1/auth/callback?code={code}&state={state}"),
                &[(
                    header::COOKIE.as_str(),
                    &format!("__Host-trestle_login={state_cookie}"),
                )],
            ))
            .await;
        assert_eq!(done.status, StatusCode::SEE_OTHER, "{:?}", done.body);
        set_cookie(&done.headers, "__Host-trestle_session").unwrap()
    }

    async fn csrf(&self, session: &str) -> String {
        let reply = self
            .call(get("/api/v1/auth/session", &[cookie(session).as_ref()]))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
        reply.body["csrf_token"].as_str().unwrap().to_owned()
    }

    async fn sessions(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM identity_sessions")
            .fetch_one(&self.env.pool)
            .await
            .unwrap()
    }

    async fn finish(self) {
        drop(self.idp);
        self.env.finish().await;
    }
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    request(Method::GET, uri, headers, None)
}

fn request(
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

fn cookie(session: &str) -> (&'static str, String) {
    ("cookie", format!("__Host-trestle_session={session}"))
}

trait AsPair {
    fn as_ref(&self) -> (&str, &str);
}

impl AsPair for (&'static str, String) {
    fn as_ref(&self) -> (&str, &str) {
        (self.0, self.1.as_str())
    }
}

/// The value a `Set-Cookie` header gives `name` (None when absent).
fn set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.strip_prefix(&format!("{name}=")))
        .map(|rest| rest.split(';').next().unwrap().to_owned())
}

fn set_cookie_line<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .unwrap()
}

const TENANT: &str = "trestle-tenant";

#[tokio::test]
async fn browser_login_creates_a_session_that_authorizes_api_calls() {
    let h = Harness::new().await;
    h.member("teacher-1", Role::Teacher).await;
    h.idp.sign_in_as("teacher-1");
    let tenant = NORTHSIDE.to_string();

    let anonymous = h.call(get("/api/v1/auth/session", &[])).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    // Login redirects to the provider with PKCE, state and nonce.
    let started = h
        .call(get("/api/v1/auth/login?return_to=/lessons", &[]))
        .await;
    assert_eq!(started.status, StatusCode::SEE_OTHER);
    let location = url::Url::parse(started.headers[header::LOCATION].to_str().unwrap()).unwrap();
    assert!(
        location
            .as_str()
            .starts_with(&format!("{}/authorize?", h.idp.issuer))
    );
    let params: HashMap<String, String> = location.query_pairs().into_owned().collect();
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["redirect_uri"],
        format!("{ORIGIN}/api/v1/auth/callback")
    );
    assert_eq!(params["scope"], "openid");
    assert_eq!(params["state"].len(), 43);
    assert_eq!(params["nonce"].len(), 43);
    assert_eq!(
        set_cookie_line(&started.headers, "__Host-trestle_login"),
        format!(
            "__Host-trestle_login={}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=600",
            params["state"]
        )
    );

    // The provider approves; the callback creates a session and returns.
    let (code, state) = h.idp.approve(location.as_str()).await;
    let login_cookie = format!("__Host-trestle_login={state}");
    let callback = format!("/api/v1/auth/callback?code={code}&state={state}");
    let done = h.call(get(&callback, &[("cookie", &login_cookie)])).await;
    assert_eq!(done.status, StatusCode::SEE_OTHER, "{:?}", done.body);
    assert_eq!(done.headers[header::LOCATION], "/lessons");
    let session_line = set_cookie_line(&done.headers, "__Host-trestle_session");
    assert!(
        session_line.ends_with("; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=43200"),
        "{session_line}"
    );
    assert!(set_cookie_line(&done.headers, "__Host-trestle_login").ends_with("Max-Age=0"));
    let session = set_cookie(&done.headers, "__Host-trestle_session").unwrap();

    // A login state works once.
    let replay = h.call(get(&callback, &[("cookie", &login_cookie)])).await;
    assert_eq!(replay.status, StatusCode::UNAUTHORIZED);
    assert_eq!(replay.body["code"], "login_expired");

    let info = h
        .call(get("/api/v1/auth/session", &[cookie(&session).as_ref()]))
        .await;
    assert_eq!(info.status, StatusCode::OK);
    assert_eq!(info.body["actor"], h.actor("teacher-1").to_string());
    assert_eq!(info.headers[header::CACHE_CONTROL], "private, no-store");
    let csrf = info.body["csrf_token"].as_str().unwrap().to_owned();

    // Reads need only the cookie.
    let list = h
        .call(get(
            "/api/v1/lessons",
            &[cookie(&session).as_ref(), (TENANT, &tenant)],
        ))
        .await;
    assert_eq!(list.status, StatusCode::OK, "{:?}", list.body);

    // Writes need the allowed Origin and the session's CSRF token.
    let body = json!({ "title": "Signed in", "grade_level": 4 });
    let write = |headers: Vec<(&'static str, String)>| {
        let mut all = vec![cookie(&session), (TENANT, tenant.clone())];
        all.extend(headers);
        let pairs: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
        request(Method::POST, "/api/v1/lessons", &pairs, Some(&body))
    };
    for (headers, why) in [
        (vec![], "no origin, no token"),
        (vec![("origin", ORIGIN.to_owned())], "no token"),
        (vec![("x-csrf-token", csrf.clone())], "no origin"),
        (
            vec![
                ("origin", "https://evil.example".to_owned()),
                ("x-csrf-token", csrf.clone()),
            ],
            "foreign origin",
        ),
        (
            vec![
                ("origin", ORIGIN.to_owned()),
                ("x-csrf-token", "forged".to_owned()),
            ],
            "forged token",
        ),
    ] {
        let refused = h.call(write(headers)).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN, "{why}");
        assert_eq!(refused.body["code"], "csrf_failed", "{why}");
    }
    assert_eq!(h.env.lesson_count().await, 0);
    let created = h
        .call(write(vec![
            ("origin", ORIGIN.to_owned()),
            ("x-csrf-token", csrf.clone()),
        ]))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);

    // Logout needs the same checks, then ends the session everywhere.
    let forged_logout = h
        .call(request(
            Method::POST,
            "/api/v1/auth/logout",
            &[cookie(&session).as_ref()],
            None,
        ))
        .await;
    assert_eq!(forged_logout.status, StatusCode::FORBIDDEN);
    let logout = h
        .call(request(
            Method::POST,
            "/api/v1/auth/logout",
            &[
                cookie(&session).as_ref(),
                ("origin", ORIGIN),
                ("x-csrf-token", &csrf),
            ],
            None,
        ))
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    assert!(set_cookie_line(&logout.headers, "__Host-trestle_session").ends_with("Max-Age=0"));
    let after = h
        .call(get(
            "/api/v1/lessons",
            &[cookie(&session).as_ref(), (TENANT, &tenant)],
        ))
        .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
    assert_eq!(after.body["code"], "credentials_invalid");
    h.finish().await;
}

#[tokio::test]
async fn a_callback_completes_only_in_the_browser_that_started_it() {
    let h = Harness::new().await;
    let started = h.call(get("/api/v1/auth/login", &[])).await;
    let (code, state) = h
        .idp
        .approve(started.headers[header::LOCATION].to_str().unwrap())
        .await;
    let callback = format!("/api/v1/auth/callback?code={code}&state={state}");

    // Login CSRF: the attacker's callback lands in a victim's browser,
    // which has no (or another) login cookie.
    let other = h.call(get("/api/v1/auth/login", &[])).await;
    let other_cookie = set_cookie(&other.headers, "__Host-trestle_login").unwrap();
    for headers in [
        vec![],
        vec![("cookie", format!("__Host-trestle_login={other_cookie}"))],
    ] {
        let pairs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let refused = h.call(get(&callback, &pairs)).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused.body["code"], "login_state_invalid");
    }
    assert_eq!(h.sessions().await, 0);
    assert_eq!(h.idp.token_requests(), 0, "no code was redeemed");
    h.finish().await;
}

#[tokio::test]
async fn tampered_or_refused_logins_create_no_session() {
    let h = Harness::new().await;

    // An ID token whose nonce is not this login's.
    h.idp.tamper_nonce(Some("replayed-from-another-login"));
    let started = h.call(get("/api/v1/auth/login", &[])).await;
    let state_cookie = set_cookie(&started.headers, "__Host-trestle_login").unwrap();
    let (code, state) = h
        .idp
        .approve(started.headers[header::LOCATION].to_str().unwrap())
        .await;
    let refused = h
        .call(get(
            &format!("/api/v1/auth/callback?code={code}&state={state}"),
            &[("cookie", &format!("__Host-trestle_login={state_cookie}"))],
        ))
        .await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.body["code"], "login_failed");
    h.idp.tamper_nonce(None);

    // The provider reports that the user declined.
    let started = h.call(get("/api/v1/auth/login", &[])).await;
    let state = set_cookie(&started.headers, "__Host-trestle_login").unwrap();
    let declined = h
        .call(get(
            &format!("/api/v1/auth/callback?error=access_denied&state={state}"),
            &[("cookie", &format!("__Host-trestle_login={state}"))],
        ))
        .await;
    assert_eq!(declined.status, StatusCode::UNAUTHORIZED);
    assert_eq!(declined.body["code"], "login_denied");

    // Open redirects are refused before anything is stored.
    for target in [
        "https://evil.example/",
        "//evil.example/",
        "/\\evil.example",
    ] {
        let uri = format!(
            "/api/v1/auth/login?return_to={}",
            url::form_urlencoded::byte_serialize(target.as_bytes()).collect::<String>()
        );
        let bad = h.call(get(&uri, &[])).await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{target}");
        assert_eq!(bad.body["code"], "invalid_return_to");
    }
    assert_eq!(h.sessions().await, 0);
    h.finish().await;
}

#[tokio::test]
async fn sessions_expire_idle_out_and_can_be_revoked() {
    let h = Harness::new().await;
    let actor = h.member("teacher-2", Role::Teacher).await;
    let check = |session: String| {
        let h = &h;
        async move {
            h.call(get("/api/v1/auth/session", &[cookie(&session).as_ref()]))
                .await
                .status
        }
    };

    let expired = h.login("teacher-2").await;
    assert_eq!(check(expired.clone()).await, StatusCode::OK);
    sqlx::query("UPDATE identity_sessions SET expires_at = now() - interval '1 second'")
        .execute(&h.env.pool)
        .await
        .unwrap();
    assert_eq!(
        check(expired).await,
        StatusCode::UNAUTHORIZED,
        "absolute lifetime"
    );

    let idle = h.login("teacher-2").await;
    sqlx::query(
        "UPDATE identity_sessions SET last_seen_at = now() - interval '3 hours' \
         WHERE expires_at > now()",
    )
    .execute(&h.env.pool)
    .await
    .unwrap();
    assert_eq!(check(idle).await, StatusCode::UNAUTHORIZED, "idle timeout");

    let first = h.login("teacher-2").await;
    let second = h.login("teacher-2").await;
    assert_ne!(first, second, "every login is a new session");
    let revoked = trestle_identity::session::revoke_actor(&h.env.pool, actor)
        .await
        .unwrap();
    assert_eq!(
        revoked, 3,
        "both new sessions and the idle one, which had not expired"
    );
    assert_eq!(check(first).await, StatusCode::UNAUTHORIZED);
    assert_eq!(check(second).await, StatusCode::UNAUTHORIZED);

    // Only digests are stored.
    let stored: Vec<String> = sqlx::query_scalar("SELECT token_hash FROM identity_sessions")
        .fetch_all(&h.env.pool)
        .await
        .unwrap();
    assert!(stored.iter().all(|hash| hash.len() == 64));
    h.finish().await;
}

#[tokio::test]
async fn provider_access_tokens_authenticate_api_clients() {
    let h = Harness::new().await;
    h.member("service-1", Role::Viewer).await;
    let tenant = NORTHSIDE.to_string();
    let now = idp::now();
    let claims = |overrides: Value| {
        let mut base = json!({
            "iss": h.idp.issuer,
            "sub": "service-1",
            "aud": API_AUDIENCE,
            "iat": now,
            "exp": now + 300,
        });
        for (k, v) in overrides.as_object().unwrap() {
            if v.is_null() {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = v.clone();
            }
        }
        base
    };
    let list = |token: String| {
        let h = &h;
        let tenant = tenant.clone();
        async move {
            let auth = format!("Bearer {token}");
            h.call(get(
                "/api/v1/lessons",
                &[("authorization", &auth), (TENANT, &tenant)],
            ))
            .await
        }
    };

    let ok = list(h.idp.sign(&claims(json!({})))).await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
    let multiple = list(
        h.idp
            .sign(&claims(json!({ "aud": ["other", API_AUDIENCE] }))),
    )
    .await;
    assert_eq!(multiple.status, StatusCode::OK);

    let with_key = |key: &EncodingKey, alg: Algorithm, kid: Option<&str>| {
        let mut header = Header::new(alg);
        header.kid = kid.map(Into::into);
        encode(&header, &claims(json!({})), key).unwrap()
    };
    for (token, why) in [
        (
            h.idp.sign(&claims(json!({ "aud": h.idp.client_id }))),
            "an ID token (browser audience)",
        ),
        (h.idp.sign(&claims(json!({ "aud": null }))), "no audience"),
        (h.idp.sign(&claims(json!({ "exp": now - 120 }))), "expired"),
        (
            h.idp.sign(&claims(json!({ "nbf": now + 600 }))),
            "not yet valid",
        ),
        (
            h.idp
                .sign(&claims(json!({ "iss": "https://evil.example" }))),
            "another issuer",
        ),
        (h.idp.sign(&claims(json!({ "sub": null }))), "no subject"),
        (h.idp.sign(&claims(json!({ "sub": "" }))), "empty subject"),
        (
            with_key(&idp::forger_key(), Algorithm::RS256, Some(idp::KID)),
            "forged signature",
        ),
        (
            with_key(&idp::forger_key(), Algorithm::RS256, Some("unknown")),
            "unknown key",
        ),
        (
            with_key(
                &EncodingKey::from_secret(b"guessable"),
                Algorithm::HS256,
                Some(idp::KID),
            ),
            "HMAC algorithm",
        ),
        ("not-a-jwt".to_owned(), "opaque token"),
    ] {
        let refused = list(token).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED, "{why}");
        assert_eq!(refused.body["code"], "credentials_invalid", "{why}");
    }

    // alg=none: header and claims only.
    let none = {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.",
            b64.encode(br#"{"alg":"none","typ":"JWT"}"#),
            b64.encode(claims(json!({})).to_string())
        )
    };
    assert_eq!(list(none).await.status, StatusCode::UNAUTHORIZED);

    // One request, one credential: a bearer token is not rescued by a cookie.
    let session = {
        h.member("teacher-3", Role::Teacher).await;
        h.login("teacher-3").await
    };
    let both = h
        .call(get(
            "/api/v1/lessons",
            &[
                ("authorization", "Bearer not-a-jwt"),
                cookie(&session).as_ref(),
                (TENANT, &tenant),
            ],
        ))
        .await;
    assert_eq!(both.status, StatusCode::UNAUTHORIZED);

    // Development tokens still work alongside (development only).
    let dev = h
        .call(get(
            "/api/v1/lessons",
            &[
                ("authorization", "Bearer alice-dev-token"),
                (TENANT, &tenant),
            ],
        ))
        .await;
    assert_eq!(dev.status, StatusCode::OK);
    h.finish().await;
}

#[tokio::test]
async fn logout_without_a_session_is_harmless_and_login_routes_need_configuration() {
    let h = Harness::new().await;
    let logout = h
        .call(request(Method::POST, "/api/v1/auth/logout", &[], None))
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    let csrf_probe = h.csrf(&h.login("teacher-4").await).await;
    assert_eq!(csrf_probe.len(), 43);
    h.finish().await;

    // Without OIDC configuration the routes do not exist.
    let env = TestEnv::new().await;
    let app = pilot::router(&pilot::registry().unwrap(), env.state.clone());
    for uri in [
        "/api/v1/auth/login",
        "/api/v1/auth/session",
        "/api/v1/auth/callback",
    ] {
        assert_eq!(
            send(&app, get(uri, &[])).await.status,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    env.finish().await;
}
