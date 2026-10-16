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
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);
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
        provider.jwks.refresh(&provider.http).await?;
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
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}

struct Jwks {
    uri: Url,
    state: Mutex<KeyState>,
    refreshing: tokio::sync::Mutex<()>,
}

impl Jwks {
    fn new(uri: Url) -> Self {
        Self {
            uri,
            state: Mutex::default(),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    fn snapshot(&self) -> (Arc<Vec<Key>>, bool, bool) {
        let state = self.state.lock().expect("jwks state");
        let fresh = state.fetched.is_some_and(|t| t.elapsed() < JWKS_TTL);
        let may_refresh = state
            .attempted
            .is_none_or(|t| t.elapsed() >= JWKS_MIN_REFRESH);
        (state.keys.clone(), fresh, may_refresh)
    }

    async fn refresh(&self, http: &reqwest::Client) -> Result<(), OidcError> {
        self.state.lock().expect("jwks state").attempted = Some(Instant::now());
        let set: JwkSet = get_json(http, self.uri.as_str()).await?;
        let keys: Vec<Key> = set.keys.iter().filter_map(usable).collect();
        if keys.is_empty() {
            return Err(OidcError::Protocol(
                "the JWKS has no usable signing keys".into(),
            ));
        }
        let mut state = self.state.lock().expect("jwks state");
        state.keys = Arc::new(keys);
        state.fetched = Some(Instant::now());
        Ok(())
    }

    async fn find(
        &self,
        http: &reqwest::Client,
        kid: Option<&str>,
        alg: Algorithm,
    ) -> Result<DecodingKey, VerifyError> {
        let (keys, fresh, _) = self.snapshot();
        if fresh && let Some(key) = select(&keys, kid, alg)? {
            return Ok(key);
        }
        // Stale cache or unknown key: refetch (rate limited, one at a time).
        let _guard = self.refreshing.lock().await;
        let (keys, fresh_now, may_refresh) = self.snapshot();
        let mut failure = None;
        if (!fresh_now || select(&keys, kid, alg)?.is_none())
            && may_refresh
            && let Err(err) = self.refresh(http).await
        {
            tracing::warn!(error = %err, "refreshing signing keys failed");
            failure = Some(err.to_string());
        }
        let (keys, _, _) = self.snapshot();
        match select(&keys, kid, alg)? {
            Some(key) => Ok(key),
            None if keys.is_empty() => Err(VerifyError::Unavailable(
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
}
