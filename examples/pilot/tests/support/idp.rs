//! A minimal OpenID Connect provider for tests: discovery, JWKS, an
//! authorization endpoint that approves immediately, and a token endpoint
//! that enforces PKCE, single-use codes and the registered redirect URI.
//! Signs with the test-only key in `tests/fixtures/`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const KID: &str = "test-key-1";

pub fn test_key() -> EncodingKey {
    EncodingKey::from_rsa_pem(include_bytes!("../fixtures/idp-test-rsa.pem")).unwrap()
}

/// A key the provider does not publish: tokens it signs are forgeries.
pub fn forger_key() -> EncodingKey {
    EncodingKey::from_rsa_pem(include_bytes!("../fixtures/idp-other-rsa.pem")).unwrap()
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Grant {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    subject: String,
}

#[derive(Default)]
struct Behavior {
    subject: String,
    nonce_override: Option<String>,
    codes: HashMap<String, Grant>,
    token_requests: usize,
}

struct Shared {
    issuer: String,
    client_id: String,
    client_secret: String,
    jwk: Jwk,
    behavior: Mutex<Behavior>,
}

pub struct MockIdp {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl MockIdp {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let mut jwk = Jwk::from_encoding_key(&test_key(), Algorithm::RS256).unwrap();
        jwk.common.key_id = Some(KID.into());
        jwk.common.public_key_use = Some(PublicKeyUse::Signature);
        jwk.common.key_algorithm = Some(KeyAlgorithm::RS256);
        let shared = Arc::new(Shared {
            issuer: issuer.clone(),
            client_id: "pilot-web".into(),
            client_secret: "test client secret with spaces & symbols".into(),
            jwk,
            behavior: Mutex::new(Behavior {
                subject: "subject-1".into(),
                ..Default::default()
            }),
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state(shared.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            issuer,
            client_id: shared.client_id.clone(),
            client_secret: shared.client_secret.clone(),
            shared,
            task,
        }
    }

    /// The `sub` of the next account to sign in.
    pub fn sign_in_as(&self, subject: &str) {
        self.shared.behavior.lock().unwrap().subject = subject.into();
    }

    /// Issue ID tokens with this nonce instead of the requested one.
    pub fn tamper_nonce(&self, nonce: Option<&str>) {
        self.shared.behavior.lock().unwrap().nonce_override = nonce.map(Into::into);
    }

    pub fn token_requests(&self) -> usize {
        self.shared.behavior.lock().unwrap().token_requests
    }

    /// Signs `claims` as the provider (RS256, published key ID).
    pub fn sign(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(KID.into());
        encode(&header, claims, &test_key()).unwrap()
    }

    /// Follows the authorization redirect the way a browser would.
    pub async fn approve(&self, authorization_url: &str) -> (String, String) {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let response = client.get(authorization_url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let location = response.headers()[header::LOCATION].to_str().unwrap();
        let url = url::Url::parse(location).unwrap();
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        (params["code"].clone(), params["state"].clone())
    }
}

impl Drop for MockIdp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn discovery(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let issuer = &shared.issuer;
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn jwks(State(shared): State<Arc<Shared>>) -> Json<JwkSet> {
    Json(JwkSet {
        keys: vec![shared.jwk.clone()],
    })
}

async fn authorize(
    State(shared): State<Arc<Shared>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let ok = params.get("response_type").map(String::as_str) == Some("code")
        && params.get("client_id") == Some(&shared.client_id)
        && params.get("code_challenge_method").map(String::as_str) == Some("S256")
        && params
            .get("scope")
            .is_some_and(|s| s.split(' ').any(|s| s == "openid"));
    if !ok {
        return (StatusCode::BAD_REQUEST, "bad authorization request").into_response();
    }
    let code = format!("code-{}", uuid::Uuid::new_v4().simple());
    let mut behavior = shared.behavior.lock().unwrap();
    let subject = behavior.subject.clone();
    behavior.codes.insert(
        code.clone(),
        Grant {
            nonce: params["nonce"].clone(),
            challenge: params["code_challenge"].clone(),
            redirect_uri: params["redirect_uri"].clone(),
            subject,
        },
    );
    let mut location = url::Url::parse(&params["redirect_uri"]).unwrap();
    location
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &params["state"]);
    (
        StatusCode::FOUND,
        [(header::LOCATION, location.to_string())],
    )
        .into_response()
}

async fn token(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let refuse =
        |error: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    let mut behavior = shared.behavior.lock().unwrap();
    behavior.token_requests += 1;

    // client_secret_basic, with RFC 6749 §2.3.1 form-encoding.
    let expected = format!(
        "Basic {}",
        STANDARD.encode(format!(
            "{}:{}",
            url::form_urlencoded::byte_serialize(shared.client_id.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(shared.client_secret.as_bytes())
                .collect::<String>()
        ))
    );
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return refuse("unsupported_grant_type");
    }
    let Some(grant) = form.get("code").and_then(|c| behavior.codes.remove(c)) else {
        return refuse("invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) != grant.challenge
        || form.get("redirect_uri") != Some(&grant.redirect_uri)
    {
        return refuse("invalid_grant");
    }
    let nonce = behavior.nonce_override.clone().unwrap_or(grant.nonce);
    let claims = json!({
        "iss": shared.issuer,
        "sub": grant.subject,
        "aud": shared.client_id,
        "iat": now(),
        "exp": now() + 300,
        "nonce": nonce,
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.into());
    let id_token = encode(&header, &claims, &test_key()).unwrap();
    Json(json!({
        "access_token": "opaque-access-token",
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
    }))
    .into_response()
}
