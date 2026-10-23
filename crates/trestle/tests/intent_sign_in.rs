//! ADR-034: an intent project whose `[auth]` turns on sign-in gets the
//! identity app: its migrations come from `makemigrations`, `account
//! invite` works, and a password session calls generated endpoints with
//! the usual tenant and role checks. Needs PostgreSQL.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use http::{HeaderMap, Method, Request, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use trestle::historical::testing::TestDatabase;
use trestle::identity::IdentityApp;
use trestle::identity::password;
use trestle::{Environment, ServeConfig, State};

const ORIGIN: &str = "http://localhost:5173";
const GOOD: &str = "maple syrup on snow days";
const EMAIL: &str = "ada@example.com";

const CRM: &str = r#"
[project]
name = "crm"

[tenancy]
model = "workspace"

[roles]
member = ["crm.read", "crm.write"]
viewer = ["crm.read"]

[[models]]
id = "crm.deal"
fields = [{ id = "title", type = "text", max = 100 }]

[[endpoints]]
model = "crm.deal"
crud = ["list", "create"]
read = "crm.read"
write = "crm.write"
"#;

fn crm(intent: &str, dir: &PathBuf) -> trestle::Project {
    trestle::project()
        .intent_str("trestle.toml", intent)
        .migrations_dir(dir)
}

/// Runs a built-in command the way the project binary would.
async fn cli(intent: &str, dir: &PathBuf, args: &[&str]) {
    let mut argv = vec!["crm"];
    argv.extend_from_slice(args);
    let matches = crm(intent, dir)
        .command()
        .try_get_matches_from(argv)
        .unwrap();
    crm(intent, dir)
        .run_matches(&matches)
        .await
        .unwrap_or_else(|e| panic!("{args:?}: {e}"));
}

fn database_url(db: &TestDatabase) -> String {
    let base = std::env::var("TRESTLE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".into());
    let name = db.options().get_database().unwrap();
    format!("{}/{name}", base.rsplit_once('/').unwrap().0)
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Value,
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let req = match body {
        Some(b) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

fn session_cookie(headers: &HeaderMap) -> String {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-trestle_session="))
        .and_then(|v| v.split(';').next())
        .expect("a session cookie")
        .to_owned()
}

#[tokio::test]
async fn default_auth_signs_in_with_a_password_and_calls_generated_endpoints() {
    let dir = std::env::temp_dir().join(format!("trestle-sign-in-{}", uuid::Uuid::new_v4()));

    // makemigrations writes the identity app's migration with the intent's.
    cli(CRM, &dir, &["makemigrations", "--name", "initial"]).await;
    for app in ["identity", "crm", "workspace"] {
        assert!(
            dir.join(app).join("0001_initial.sql").exists(),
            "no migration for {app}"
        );
    }
    let db = TestDatabase::create().await;
    let url = database_url(&db);
    cli(CRM, &dir, &["--database-url", &url, "migrate"]).await;

    // `account invite` creates the account (and prints a link).
    cli(
        CRM,
        &dir,
        &[
            "--database-url",
            &url,
            "account",
            "invite",
            "--email",
            EMAIL,
            "--origin",
            ORIGIN,
        ],
    )
    .await;
    let pool = db.pool().await;
    // Inviting again replaces the link; this one is used below.
    let invited = password::invite(&pool, EMAIL, Duration::from_secs(3600))
        .await
        .unwrap();
    assert!(!invited.created_account, "the command created the account");

    let (north, south) = (uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
    for ws in [north, south] {
        sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'w')")
            .bind(ws)
            .execute(&pool)
            .await
            .unwrap();
    }
    let (actor, north_id) = (invited.actor.to_string(), north.to_string());
    cli(
        CRM,
        &dir,
        &[
            "--database-url",
            &url,
            "tenant",
            "grant",
            "--tenant",
            &north_id,
            "--actor",
            &actor,
            "--role",
            "member",
        ],
    )
    .await;

    // Browser sign-in configured from [auth], as `serve` does.
    let intent = trestle::intent::parse("trestle.toml", CRM).unwrap();
    let mut config = ServeConfig::for_tests();
    assert!(config.apply_auth(&intent.auth, |_| None).is_empty());
    assert!(config.password_login);
    config.session_key = Some("a session key that is long enough to use".into());
    config.public_origin = vec![ORIGIN.into()];
    let (identity, browser) = config
        .identity(Environment::Development, &pool)
        .await
        .unwrap();
    let registry = crm(CRM, &dir).assemble().unwrap();
    let state = State {
        pool: pool.clone(),
        identity,
        browser,
        env: Environment::Development,
    };
    let app = trestle::router_with(&registry, state, Default::default());

    let from_browser = [("origin", ORIGIN)];
    let set = send(
        &app,
        Method::POST,
        password::SET_PATH,
        &from_browser,
        Some(json!({ "token": invited.token, "password": GOOD })),
    )
    .await;
    assert_eq!(set.status, 200, "{}", set.body);
    let login = send(
        &app,
        Method::POST,
        password::LOGIN_PATH,
        &from_browser,
        Some(json!({ "email": EMAIL, "password": GOOD })),
    )
    .await;
    assert_eq!(login.status, 200, "{}", login.body);
    assert_eq!(login.body["actor"], actor);
    let cookie = session_cookie(&login.headers);
    let csrf = login.body["csrf_token"].as_str().unwrap().to_owned();

    let session = send(
        &app,
        Method::GET,
        "/api/v1/auth/session",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(session.status, 200, "{}", session.body);

    // The session authenticates generated endpoints in a member's tenant.
    let headers = |tenant: &str, csrf: Option<&str>| {
        let mut headers = vec![
            ("cookie", cookie.clone()),
            ("origin", ORIGIN.to_owned()),
            ("trestle-tenant", tenant.to_owned()),
        ];
        if let Some(csrf) = csrf {
            headers.push(("x-csrf-token", csrf.to_owned()));
        }
        headers
    };
    let call = |method: Method, headers: Vec<(&'static str, String)>, body: Option<Value>| {
        let app = app.clone();
        async move {
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            send(&app, method, "/api/v1/deals", &headers, body).await
        }
    };
    let created = call(
        Method::POST,
        headers(&north_id, Some(&csrf)),
        Some(json!({ "title": "First" })),
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let listed = call(Method::GET, headers(&north_id, None), None).await;
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert_eq!(listed.body["items"][0]["title"], "First");

    // A cookie write without the CSRF token is refused.
    let forged = call(
        Method::POST,
        headers(&north_id, None),
        Some(json!({ "title": "Forged" })),
    )
    .await;
    assert_eq!(forged.status, 403, "{}", forged.body);
    // Membership still decides: no role in the other workspace.
    let elsewhere = call(Method::GET, headers(&south.to_string(), None), None).await;
    assert_eq!(elsewhere.status, 403, "{}", elsewhere.body);

    pool.close().await;
    db.drop().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_identity_app_is_installed_only_when_sign_in_needs_it() {
    let dir = std::env::temp_dir().join("trestle-sign-in-unused");
    let identity_apps = |registry: &trestle::Registry<State>| {
        registry
            .apps()
            .iter()
            .filter(|a| &*a.id == "identity")
            .count()
    };
    let auth_routes = |registry: &trestle::Registry<State>| {
        registry
            .routes()
            .iter()
            .filter(|r| r.path.starts_with("/api/v1/auth/"))
            .count()
    };

    let default = crm(CRM, &dir).assemble().unwrap();
    assert_eq!(identity_apps(&default), 1);
    assert!(default.desired_schema().model("identity.account").is_some());
    assert_eq!(auth_routes(&default), 9);

    // OpenID Connect browser sign-in alone also needs sessions.
    let oidc = CRM.replace(
        "[tenancy]",
        "[auth]\npasswords = false\noidc = \"google\"\n\n[tenancy]",
    );
    assert_eq!(identity_apps(&crm(&oidc, &dir).assemble().unwrap()), 1);

    // Sign-in off: no tables, no routes.
    let off = CRM.replace("[tenancy]", "[auth]\npasswords = false\n\n[tenancy]");
    let off = crm(&off, &dir).assemble().unwrap();
    assert_eq!(identity_apps(&off), 0);
    assert!(off.desired_schema().model("identity.session").is_none());
    assert_eq!(auth_routes(&off), 0);
    // ...and `serve` refuses browser sign-in set by environment instead.
    let mut config = ServeConfig::for_tests();
    config.password_login = true;
    let refused = trestle::serve::serve(
        Arc::new(off),
        Environment::Development,
        "postgres://app@localhost/app".parse().unwrap(),
        config,
        trestle::NoExtension::default(),
        "crm",
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("browser sign-in needs the identity tables"),
        "{refused}"
    );
    // A project's own app with a model of that name doesn't count.
    let off = CRM.replace("[tenancy]", "[auth]\npasswords = false\n\n[tenancy]");
    let lookalike = crm(&off, &dir).install_app(Lookalike).assemble().unwrap();
    assert!(
        lookalike
            .desired_schema()
            .model("identity.session")
            .is_some()
    );
    let mut config = ServeConfig::for_tests();
    config.password_login = true;
    let refused = trestle::serve::serve(
        Arc::new(lookalike),
        Environment::Development,
        "postgres://app@localhost/app".parse().unwrap(),
        config,
        trestle::NoExtension::default(),
        "crm",
    )
    .await
    .unwrap_err();
    assert!(
        refused.contains("browser sign-in needs the identity tables"),
        "{refused}"
    );

    // A project that installs the identity app itself keeps its own.
    let own = crm(CRM, &dir)
        .install_app(IdentityApp::new(&[]))
        .assemble()
        .unwrap();
    assert_eq!(identity_apps(&own), 1);

    // The app ID is taken while sign-in is on.
    let taken = CRM.replace("crm.deal", "identity.deal");
    let Err(err) = crm(&taken, &dir).assemble() else {
        panic!("assembled");
    };
    let err = err.to_string();
    assert!(err.contains("the app ID \"identity\" is taken"), "{err}");
}

/// An app of the project's own named `identity`, with a model named
/// `identity.session` that isn't the identity app's.
struct Lookalike;

impl trestle::core::registry::App<State> for Lookalike {
    fn id(&self) -> &'static str {
        "identity"
    }

    fn register(&self, app: &mut trestle::core::registry::AppBuilder<State>) {
        use trestle::historical::schema::{Column, Model};
        app.model(
            Model::new("identity.session")
                .table("lookalike_sessions")
                .field(Column::uuid("id").primary_key()),
        );
    }
}
