//! OpenID Connect relying-party pieces: discovery, signing keys, token
//! verification and the authorization-code exchange.
//!
//! Everything the provider sends is untrusted input: responses are size
//! limited, the discovered issuer must equal the configured one exactly,
//! signing keys are asymmetric only, and every token is checked for
//! signature, issuer, audience, expiry and not-before.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use trestle_core::identity::AuthError;
use url::Url;

/// Signature algorithms accepted from any provider. Never `none` and never
/// HMAC: a shared secret would let anyone holding it mint tokens, and HMAC
/// keyed with a public key is the classic key-confusion attack.
const ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// Cached keys older than this are refetched before use.
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);
/// The longest the cache is trusted after its last successful fetch. While
/// the provider is unreachable or answers with garbage, the previous keys
/// keep verifying for this long (an outage grace); after it, verification
/// fails as unavailable. Without a bound, a retired key whose private half
/// leaked would stay trusted for as long as refreshes keep failing, and the
/// attacker picks each token's `exp`. A day rides out an overnight provider
/// outage; providers publish a successor key well before retiring one.
const JWKS_MAX_STALE: Duration = Duration::from_secs(24 * 60 * 60);
/// Unknown `kid`s trigger a refetch at most this often, so a flood of
/// forged tokens cannot turn into a flood of provider requests.
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("{0}")]
    Config(String),
    #[error("identity provider request failed: {0}")]
    Unavailable(String),
    #[error("identity provider returned an invalid response: {0}")]
    Protocol(String),
}

/// Why a token was refused. Only `Unavailable` is the provider's fault.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("token rejected: {0}")]
    Invalid(String),
    #[error("signing keys unavailable: {0}")]
    Unavailable(String),
}

impl From<VerifyError> for AuthError {
    fn from(err: VerifyError) -> Self {
        match err {
            VerifyError::Invalid(reason) => {
                tracing::debug!(%reason, "token rejected");
                AuthError::Invalid
            }
            VerifyError::Unavailable(reason) => {
                tracing::warn!(%reason, "identity provider keys unavailable");
                AuthError::Unavailable
            }
        }
    }
}

/// Which provider URLs are acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Production: every provider URL is `https`.
    HttpsOnly,
    /// Local development and tests: `http` is allowed for loopback hosts.
    AllowLoopbackHttp,
}

/// Parses a provider or redirect URL under `transport`.
pub fn check_url(raw: &str, transport: Transport) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("{raw:?} is not a URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{raw:?} must not contain credentials"));
    }
    if url.fragment().is_some() {
        return Err(format!("{raw:?} must not contain a fragment"));
    }
    let loopback = matches!(
        url.host(),
        Some(url::Host::Domain("localhost"))
            | Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST))
            | Some(url::Host::Ipv6(std::net::Ipv6Addr::LOCALHOST))
    );
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback && transport == Transport::AllowLoopbackHttp => Ok(url),
        _ => Err(format!("{raw:?} must use https")),
    }
}

/// Registered claims the framework relies on. Providers add more; they
/// are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    pub aud: Audience,
    pub exp: u64,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub azp: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    pub fn contains(&self, value: &str) -> bool {
        match self {
            Audience::One(one) => one == value,
            Audience::Many(many) => many.iter().any(|a| a == value),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Audience::One(_) => 1,
            Audience::Many(many) => many.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

/// A discovered provider. Cheap to share; holds the signing-key cache.
pub struct Provider {
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    jwks: Jwks,
    http: reqwest::Client,
}

impl Provider {
    /// Fetches `<issuer>/.well-known/openid-configuration` and the signing
    /// keys. The document's `issuer` must equal `issuer` exactly.
    pub async fn discover(issuer: &str, transport: Transport) -> Result<Arc<Self>, OidcError> {
        check_url(issuer, transport).map_err(OidcError::Config)?;
        let http = client()?;
        let well_known = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let doc: Discovery = get_json(&http, &well_known).await?;
        if doc.issuer != issuer {
            return Err(OidcError::Protocol(format!(
                "discovery document names issuer {:?}, expected {issuer:?}",
                doc.issuer
            )));
        }
        if let Some(methods) = &doc.code_challenge_methods_supported
            && !methods.iter().any(|m| m == "S256")
        {
            return Err(OidcError::Config(
                "the provider does not support PKCE with S256".into(),
            ));
        }
        let endpoint = |raw: &str| check_url(raw, transport).map_err(OidcError::Protocol);
        let provider = Self {
            issuer: doc.issuer.clone(),
            authorization_endpoint: endpoint(&doc.authorization_endpoint)?,
            token_endpoint: endpoint(&doc.token_endpoint)?,
            jwks: Jwks::new(endpoint(&doc.jwks_uri)?),
            http,
        };
        if provider.jwks.refresh(&provider.http).await? == 0 {
            return Err(OidcError::Protocol(
                "the JWKS has no usable signing keys".into(),
            ));
        }
        Ok(Arc::new(provider))
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The authorization endpoint with `params` appended.
    pub fn authorization_url(&self, params: &[(&str, &str)]) -> Url {
        let mut url = self.authorization_endpoint.clone();
        url.query_pairs_mut().extend_pairs(params);
        url
    }

    /// Verifies a signed JWT issued by this provider for `audience`.
    pub async fn verify(&self, token: &str, audience: &str) -> Result<Claims, VerifyError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(VerifyError::Invalid("token too large".into()));
        }
        let header =
            decode_header(token).map_err(|e| VerifyError::Invalid(format!("header: {e}")))?;
        if !ALGORITHMS.contains(&header.alg) {
            return Err(VerifyError::Invalid(format!(
                "algorithm {:?} is not accepted",
                header.alg
            )));
        }
        let key = self
            .jwks
            .find(&self.http, header.kid.as_deref(), header.alg)
            .await?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 60;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|e| VerifyError::Invalid(e.to_string()))?
            .claims;
        if claims.sub.is_empty() || claims.sub.len() > 255 {
            return Err(VerifyError::Invalid("sub must be 1-255 bytes".into()));
        }
        Ok(claims)
    }

    /// Redeems an authorization code (RFC 6749 §4.1.3 with PKCE) and
    /// returns the ID token. Confidential clients authenticate with
    /// `client_secret_basic`.
    pub async fn exchange_code(
        &self,
        client_id: &str,
        client_secret: Option<&str>,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<String, OidcError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("client_id", client_id),
        ];
        let mut request = self
            .http
            .post(self.token_endpoint.clone())
            .header(http::header::ACCEPT, "application/json");
        if let Some(secret) = client_secret {
            // RFC 6749 §2.3.1: both parts are form-encoded before Basic.
            request = request.basic_auth(form_encode(client_id), Some(form_encode(secret)));
        }
        let response = request
            .form(&form)
            .send()
            .await
            .map_err(|e| OidcError::Unavailable(e.to_string()))?;
        let status = response.status();
        let body = read_limited(response).await?;
        if !status.is_success() {
            #[derive(Deserialize)]
            struct TokenError {
                error: String,
            }
            let error = serde_json::from_slice::<TokenError>(&body)
                .map(|e| e.error)
                .unwrap_or_default();
            return Err(OidcError::Protocol(format!(
                "token endpoint answered {status} {}",
                printable(&error)
            )));
        }
        #[derive(Deserialize)]
        struct TokenResponse {
            id_token: Option<String>,
        }
        let tokens: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| OidcError::Protocol(format!("token response: {e}")))?;
        tokens
            .id_token
            .ok_or_else(|| OidcError::Protocol("token response has no id_token".into()))
    }
}

fn form_encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Provider-supplied text for logs: bounded, printable ASCII only.
pub(crate) fn printable(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(100)
        .collect()
}

fn client() -> Result<reqwest::Client, OidcError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("trestle-identity")
        .build()
        .map_err(|e| OidcError::Config(format!("HTTP client: {e}")))
}

async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, OidcError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| OidcError::Unavailable(e.to_string()))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(OidcError::Protocol("response too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn get_json<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
) -> Result<T, OidcError> {
    let response = http
        .get(url)
        .header(http::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| OidcError::Unavailable(format!("{url}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        return Err(OidcError::Unavailable(format!("{url}: status {status}")));
    }
    let body = read_limited(response).await?;
    serde_json::from_slice(&body).map_err(|e| OidcError::Protocol(format!("{url}: {e}")))
}

// Signing keys --------------------------------------------------------------

#[derive(Clone)]
struct Key {
    kid: Option<String>,
    alg: Option<Algorithm>,
    key: DecodingKey,
}

#[derive(Default)]
struct KeyState {
    keys: Arc<Vec<Key>>,
    /// The last successful, authoritative fetch.
    fetched: Option<Instant>,
    /// The last attempt, successful or not.
    attempted: Option<Instant>,
}

struct Snapshot {
    keys: Arc<Vec<Key>>,
    /// Within the TTL: used without refetching.
    fresh: bool,
    /// Within the stale grace: still used when a refetch fails.
    trusted: bool,
    may_refresh: bool,
}

struct Jwks {
    uri: Url,
    ttl: Duration,
    max_stale: Duration,
    min_refresh: Duration,
    state: Mutex<KeyState>,
    refreshing: tokio::sync::Mutex<()>,
}

const REFRESH_METRIC: &str = "trestle_oidc_jwks_refresh_total";
const REFRESH_HELP: &str = "Signing-key fetches from the identity provider, by outcome.";

impl Jwks {
    fn new(uri: Url) -> Self {
        Self {
            uri,
            ttl: JWKS_TTL,
            max_stale: JWKS_MAX_STALE,
            min_refresh: JWKS_MIN_REFRESH,
            state: Mutex::default(),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().expect("jwks state");
        let age = state.fetched.map(|t| t.elapsed());
        Snapshot {
            keys: state.keys.clone(),
            fresh: age.is_some_and(|a| a < self.ttl),
            trusted: age.is_some_and(|a| a < self.max_stale),
            may_refresh: state
                .attempted
                .is_none_or(|t| t.elapsed() >= self.min_refresh),
        }
    }

    /// Fetches the key set and returns how many usable keys it has. A
    /// successful fetch is authoritative: it replaces the cache entirely, so
    /// a key the provider stopped publishing stops verifying, and a set with
    /// no usable keys leaves nothing trusted. A failed or malformed fetch
    /// leaves the cache as it was, trusted only until `max_stale`.
    async fn refresh(&self, http: &reqwest::Client) -> Result<usize, OidcError> {
        self.state.lock().expect("jwks state").attempted = Some(Instant::now());
        let set: JwkSet = match get_json(http, self.uri.as_str()).await {
            Ok(set) => set,
            Err(err) => {
                let outcome = match err {
                    OidcError::Protocol(_) => "invalid",
                    _ => "unavailable",
                };
                trestle_core::metrics::increment(
                    REFRESH_METRIC,
                    REFRESH_HELP,
                    &[("outcome", outcome)],
                );
                return Err(err);
            }
        };
        let keys: Vec<Key> = set.keys.iter().filter_map(usable).collect();
        let count = keys.len();
        let outcome = if count == 0 {
            tracing::warn!("the identity provider published no usable signing keys");
            "empty"
        } else {
            "ok"
        };
        trestle_core::metrics::increment(REFRESH_METRIC, REFRESH_HELP, &[("outcome", outcome)]);
        let mut state = self.state.lock().expect("jwks state");
        state.keys = Arc::new(keys);
        state.fetched = Some(Instant::now());
        Ok(count)
    }

    async fn find(
        &self,
        http: &reqwest::Client,
        kid: Option<&str>,
        alg: Algorithm,
    ) -> Result<DecodingKey, VerifyError> {
        let cache = self.snapshot();
        if cache.fresh
            && let Some(key) = select(&cache.keys, kid, alg)?
        {
            return Ok(key);
        }
        // Stale cache or unknown key: refetch (rate limited, one at a time).
        let _guard = self.refreshing.lock().await;
        let cache = self.snapshot();
        let mut failure = None;
        if (!cache.fresh || select(&cache.keys, kid, alg)?.is_none())
            && cache.may_refresh
            && let Err(err) = self.refresh(http).await
        {
            tracing::warn!(error = %err, "refreshing signing keys failed");
            failure = Some(err.to_string());
        }
        let cache = self.snapshot();
        if !cache.trusted {
            // Past the outage grace (or never fetched): the old keys are no
            // longer evidence of what the provider signs with.
            if !cache.keys.is_empty() {
                trestle_core::metrics::increment(
                    "trestle_oidc_jwks_expired_total",
                    "Tokens refused because the cached signing keys outlived the stale grace.",
                    &[],
                );
            }
            return Err(VerifyError::Unavailable(failure.unwrap_or_else(|| {
                "signing keys were not refreshed within the stale grace".into()
            })));
        }
        match select(&cache.keys, kid, alg)? {
            Some(key) => Ok(key),
            None if cache.keys.is_empty() => Err(VerifyError::Unavailable(
                failure.unwrap_or_else(|| "no signing keys".into()),
            )),
            None => Err(VerifyError::Invalid("no matching signing key".into())),
        }
    }
}

/// An asymmetric signing key, or `None` for encryption/HMAC/unknown keys.
fn usable(jwk: &Jwk) -> Option<Key> {
    if matches!(jwk.common.public_key_use, Some(ref u) if *u != PublicKeyUse::Signature) {
        return None;
    }
    if matches!(
        jwk.algorithm,
        AlgorithmParameters::OctetKey(_) | AlgorithmParameters::Other(_)
    ) {
        return None;
    }
    let alg = match &jwk.common.key_algorithm {
        None => None,
        Some(declared) => {
            let name = serde_json::to_value(declared).ok()?;
            let alg: Algorithm = name.as_str()?.parse().ok()?;
            if !ALGORITHMS.contains(&alg) {
                return None;
            }
            Some(alg)
        }
    };
    Some(Key {
        kid: jwk.common.key_id.clone(),
        alg,
        key: DecodingKey::from_jwk(jwk).ok()?,
    })
}

/// The one key that may verify a token with `kid`/`alg`. Without a `kid`,
/// exactly one compatible key must exist.
fn select(
    keys: &[Key],
    kid: Option<&str>,
    alg: Algorithm,
) -> Result<Option<DecodingKey>, VerifyError> {
    let mut candidates = keys.iter().filter(|k| {
        (kid.is_none() || k.kid.as_deref() == kid)
            && k.alg.is_none_or(|a| a == alg)
            && k.key.family() == alg.family()
    });
    let first = candidates.next();
    if kid.is_none() && candidates.next().is_some() {
        return Err(VerifyError::Invalid(
            "token has no kid and several keys match".into(),
        ));
    }
    Ok(first.map(|k| k.key.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_urls_require_https_except_loopback_in_development() {
        assert!(check_url("https://id.example.com", Transport::HttpsOnly).is_ok());
        assert!(check_url("http://id.example.com", Transport::HttpsOnly).is_err());
        assert!(check_url("http://127.0.0.1:9000", Transport::HttpsOnly).is_err());
        assert!(check_url("http://127.0.0.1:9000", Transport::AllowLoopbackHttp).is_ok());
        assert!(check_url("http://localhost:9000", Transport::AllowLoopbackHttp).is_ok());
        assert!(check_url("http://id.example.com", Transport::AllowLoopbackHttp).is_err());
        assert!(check_url("https://user:pw@id.example.com", Transport::HttpsOnly).is_err());
        assert!(check_url("https://id.example.com/#x", Transport::HttpsOnly).is_err());
    }

    #[test]
    fn provider_text_is_bounded_and_printable() {
        assert_eq!(
            printable("invalid_grant\n<script>"),
            "invalid_grant<script>"
        );
        assert_eq!(printable(&"x".repeat(500)).len(), 100);
    }

    /// Signing-key cache lifetime against a mock JWKS endpoint (SR-04).
    /// Time is simulated by backdating the cache; the TTL and grace are
    /// shortened so the backdating stays a few seconds.
    mod key_lifetime {
        use super::*;
        use axum::Router;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        use jsonwebtoken::jwk::KeyAlgorithm;
        use jsonwebtoken::{EncodingKey, Header, encode};
        use std::sync::atomic::{AtomicUsize, Ordering};

        const ISSUER: &str = "https://issuer.example";
        const AUDIENCE: &str = "api";
        const TTL: Duration = Duration::from_secs(10);
        const GRACE: Duration = Duration::from_secs(20);
        /// Past the TTL, inside the grace.
        const STALE: Duration = Duration::from_secs(15);
        /// Past the grace.
        const EXPIRED: Duration = Duration::from_secs(25);

        fn key_a() -> EncodingKey {
            EncodingKey::from_rsa_pem(include_bytes!(
                "../../../examples/pilot/tests/fixtures/idp-test-rsa.pem"
            ))
            .unwrap()
        }

        fn key_b() -> EncodingKey {
            EncodingKey::from_rsa_pem(include_bytes!(
                "../../../examples/pilot/tests/fixtures/idp-other-rsa.pem"
            ))
            .unwrap()
        }

        fn jwk(key: &EncodingKey, kid: &str) -> Jwk {
            let mut jwk = Jwk::from_encoding_key(key, Algorithm::RS256).unwrap();
            jwk.common.key_id = Some(kid.into());
            jwk.common.public_key_use = Some(PublicKeyUse::Signature);
            jwk.common.key_algorithm = Some(KeyAlgorithm::RS256);
            jwk
        }

        #[derive(Clone)]
        enum Serve {
            Keys(Vec<Jwk>),
            Status(StatusCode),
            Malformed,
            Hang,
        }

        struct Idp {
            serve: Mutex<Serve>,
            requests: AtomicUsize,
        }

        async fn jwks(State(idp): State<Arc<Idp>>) -> Response {
            idp.requests.fetch_add(1, Ordering::SeqCst);
            let serve = idp.serve.lock().unwrap().clone();
            match serve {
                Serve::Keys(keys) => axum::Json(JwkSet { keys }).into_response(),
                Serve::Status(status) => status.into_response(),
                Serve::Malformed => {
                    ([("content-type", "application/json")], "{\"keys\": [{").into_response()
                }
                Serve::Hang => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    StatusCode::OK.into_response()
                }
            }
        }

        struct Fixture {
            idp: Arc<Idp>,
            provider: Provider,
            task: tokio::task::JoinHandle<()>,
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        impl Fixture {
            async fn start(keys: Vec<Jwk>) -> Self {
                let published = keys.len();
                let idp = Arc::new(Idp {
                    serve: Mutex::new(Serve::Keys(keys)),
                    requests: AtomicUsize::new(0),
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let uri = format!("http://{}/jwks", listener.local_addr().unwrap());
                let app = Router::new()
                    .route("/jwks", get(jwks))
                    .with_state(idp.clone());
                let task = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let mut jwks = Jwks::new(Url::parse(&uri).unwrap());
                jwks.ttl = TTL;
                jwks.max_stale = GRACE;
                jwks.min_refresh = Duration::ZERO;
                let provider = Provider {
                    issuer: ISSUER.into(),
                    authorization_endpoint: Url::parse("https://issuer.example/authorize").unwrap(),
                    token_endpoint: Url::parse("https://issuer.example/token").unwrap(),
                    jwks,
                    // Short enough that a hanging provider is a quick test.
                    http: reqwest::Client::builder()
                        .timeout(Duration::from_millis(300))
                        .build()
                        .unwrap(),
                };
                let usable = provider.jwks.refresh(&provider.http).await.unwrap();
                assert_eq!(usable, published);
                Self {
                    idp,
                    provider,
                    task,
                }
            }

            fn serve(&self, serve: Serve) {
                *self.idp.serve.lock().unwrap() = serve;
            }

            fn requests(&self) -> usize {
                self.idp.requests.load(Ordering::SeqCst)
            }

            /// Pretends the last successful fetch happened `age` ago.
            fn age_cache(&self, age: Duration) {
                let mut state = self.provider.jwks.state.lock().unwrap();
                state.fetched = Some(Instant::now().checked_sub(age).unwrap());
                state.attempted = state.fetched;
            }

            async fn verify(&self, key: &EncodingKey, kid: &str) -> Result<Claims, VerifyError> {
                let mut header = Header::new(Algorithm::RS256);
                header.kid = Some(kid.into());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let token = encode(
                    &header,
                    &serde_json::json!({
                        "iss": ISSUER, "aud": AUDIENCE, "sub": "actor", "exp": now + 600
                    }),
                    key,
                )
                .unwrap();
                self.provider.verify(&token, AUDIENCE).await
            }
        }

        fn unavailable(result: Result<Claims, VerifyError>) -> bool {
            matches!(result, Err(VerifyError::Unavailable(_)))
        }

        #[test]
        fn production_grace_is_bounded_and_outlasts_the_ttl() {
            const _: () = assert!(JWKS_TTL.as_secs() < JWKS_MAX_STALE.as_secs());
            const _: () = assert!(JWKS_MAX_STALE.as_secs() <= 24 * 60 * 60);
            let jwks = Jwks::new(Url::parse("https://issuer.example/jwks").unwrap());
            assert_eq!(jwks.max_stale, JWKS_MAX_STALE);
            assert_eq!(jwks.ttl, JWKS_TTL);
        }

        #[tokio::test]
        async fn failed_refreshes_keep_cached_keys_only_within_the_grace() {
            let failures = [
                Serve::Status(StatusCode::SERVICE_UNAVAILABLE),
                Serve::Malformed,
                Serve::Hang,
            ];
            for failure in failures {
                let fx = Fixture::start(vec![jwk(&key_a(), "a")]).await;
                fx.verify(&key_a(), "a").await.unwrap();

                fx.serve(failure);
                fx.age_cache(STALE);
                let before = fx.requests();
                fx.verify(&key_a(), "a")
                    .await
                    .expect("cached key within the grace");
                assert_eq!(fx.requests(), before + 1, "a stale cache is refetched");

                fx.age_cache(EXPIRED);
                let refused = fx.verify(&key_a(), "a").await;
                assert!(
                    unavailable(refused),
                    "past the grace a fresh token is refused"
                );
                let err = fx.verify(&key_a(), "a").await.unwrap_err();
                assert!(matches!(AuthError::from(err), AuthError::Unavailable));

                // Recovery: one successful fetch restores trust.
                fx.serve(Serve::Keys(vec![jwk(&key_a(), "a")]));
                fx.verify(&key_a(), "a").await.expect("recovered");
                assert!(fx.provider.jwks.snapshot().fresh);
            }
        }

        #[tokio::test]
        async fn expired_cache_is_refused_even_when_refresh_is_rate_limited() {
            // The review's probe: an old cache right after a failed refresh,
            // so the next refetch is suppressed.
            let mut fx = Fixture::start(vec![jwk(&key_a(), "a")]).await;
            fx.provider.jwks.min_refresh = JWKS_MIN_REFRESH;
            fx.serve(Serve::Status(StatusCode::BAD_GATEWAY));
            fx.age_cache(EXPIRED);
            fx.provider.jwks.state.lock().unwrap().attempted = Some(Instant::now());
            let before = fx.requests();
            assert!(unavailable(fx.verify(&key_a(), "a").await));
            assert_eq!(fx.requests(), before, "rate limit still applies");
        }

        #[tokio::test]
        async fn an_empty_key_set_leaves_nothing_trusted() {
            let fx = Fixture::start(vec![jwk(&key_a(), "a")]).await;
            fx.verify(&key_a(), "a").await.unwrap();

            fx.serve(Serve::Keys(vec![]));
            fx.age_cache(STALE);
            assert!(unavailable(fx.verify(&key_a(), "a").await));
            assert!(fx.provider.jwks.snapshot().keys.is_empty());

            // Recovery.
            fx.serve(Serve::Keys(vec![jwk(&key_a(), "a")]));
            fx.verify(&key_a(), "a").await.expect("recovered");
        }

        #[tokio::test]
        async fn a_key_removed_by_a_successful_refresh_stops_verifying() {
            let fx = Fixture::start(vec![jwk(&key_a(), "a"), jwk(&key_b(), "b")]).await;
            fx.verify(&key_a(), "a").await.unwrap();
            fx.verify(&key_b(), "b").await.unwrap();

            // Retire "a" at the provider; the next refetch drops it.
            fx.serve(Serve::Keys(vec![jwk(&key_b(), "b")]));
            fx.age_cache(STALE);
            fx.verify(&key_b(), "b").await.expect("current key");
            let refused = fx.verify(&key_a(), "a").await;
            assert!(
                matches!(refused, Err(VerifyError::Invalid(_))),
                "a retired key is an invalid credential"
            );
            // Even if the provider then fails, the retired key stays gone.
            fx.serve(Serve::Status(StatusCode::INTERNAL_SERVER_ERROR));
            fx.age_cache(STALE);
            assert!(fx.verify(&key_a(), "a").await.is_err());
            fx.verify(&key_b(), "b").await.expect("within the grace");
        }
    }
}
