//! Login check: verifies the signed token the reverse proxy adds to requests that passed its login.
//!
//! Only the signature is trusted. Plain identity headers are ignored, because the server can also be
//! reached without going through the proxy.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};

pub const TOKEN_HEADER: &str = "cf-access-jwt-assertion";
const REFETCH_EVERY: Duration = Duration::from_secs(60);

pub struct AccessConfig {
    /// The identity provider's base URL, which is also the token's issuer. No trailing slash.
    pub team: String,
    /// The application's audience tag.
    pub aud: String,
}

/// Both settings turn the login check on, neither leaves it off, and one alone is a mistake.
pub fn access_config(team: Option<String>, aud: Option<String>) -> Result<Option<AccessConfig>> {
    let team = team.filter(|s| !s.is_empty());
    let aud = aud.filter(|s| !s.is_empty());
    match (team, aud) {
        // The keys that decide who gets in must not be fetched over a connection anyone can alter
        (Some(team), _) if !team.starts_with("https://") => {
            bail!("access_team (or KIDTIME_ACCESS_TEAM) must start with https://, but is {team:?}")
        }
        (Some(team), Some(aud)) => Ok(Some(AccessConfig {
            team: team.trim_end_matches('/').to_string(),
            aud,
        })),
        (None, None) => Ok(None),
        _ => bail!(
            "the login check needs both access_team and access_aud (or KIDTIME_ACCESS_TEAM and KIDTIME_ACCESS_AUD)"
        ),
    }
}

/// Allows a key fetch at most once a minute, so neither tokens with made-up key ids nor requests
/// arriving while the provider is unreachable can cause a fetch each.
#[derive(Default)]
struct RefetchGate {
    last: Option<Instant>,
}

impl RefetchGate {
    fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < REFETCH_EVERY)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

struct Keys {
    set: Option<Arc<JwkSet>>,
    gate: RefetchGate,
}

pub struct Auth {
    config: AccessConfig,
    /// Only ever locked briefly, never across a fetch.
    keys: Mutex<Keys>,
    /// None in tests: the keys given up front are all there is.
    client: Option<reqwest::Client>,
    /// How many fetches were started, so tests can tell the gate held one back.
    #[cfg(test)]
    fetches: std::sync::atomic::AtomicUsize,
}

#[derive(serde::Deserialize)]
struct Claims {}

enum Rejected {
    /// Signed with a key id we don't have: worth one refetch.
    UnknownKey,
    Invalid,
}

impl Auth {
    pub fn new(config: AccessConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .https_only(true)
            .build()
            .expect("HTTP client");
        Self {
            config,
            keys: Mutex::new(Keys {
                set: None,
                gate: RefetchGate::default(),
            }),
            client: Some(client),
            #[cfg(test)]
            fetches: Default::default(),
        }
    }

    /// No keys and no way to fetch any.
    #[cfg(test)]
    pub fn without_keys(config: AccessConfig) -> Self {
        Self {
            config,
            keys: Mutex::new(Keys {
                set: None,
                gate: RefetchGate::default(),
            }),
            client: None,
            #[cfg(test)]
            fetches: Default::default(),
        }
    }

    #[cfg(test)]
    pub fn with_keys(config: AccessConfig, keys: JwkSet) -> Self {
        Self {
            config,
            keys: Mutex::new(Keys {
                set: Some(Arc::new(keys)),
                gate: RefetchGate::default(),
            }),
            client: None,
            #[cfg(test)]
            fetches: Default::default(),
        }
    }

    /// Fetches the keys once at start-up, so the first request doesn't have to. If this fails the
    /// server still starts, and answers 503 until a later fetch works.
    pub async fn warm(&self) {
        match self.fetch_if_allowed().await {
            Some(keys) => tracing::info!("login check is on: loaded {} keys", keys.keys.len()),
            None => tracing::error!(
                "login keys could not be loaded: requests get 503 until they can be"
            ),
        }
    }

    pub async fn check(&self, token: Option<&str>) -> Result<(), StatusCode> {
        let token = token.ok_or(StatusCode::UNAUTHORIZED)?;
        let cached = self.keys.lock().unwrap().set.clone();
        // What to answer if fresh keys can't be had. Every path below ends in a verified token or
        // an error: there is no way through without keys.
        let otherwise = match &cached {
            // Without keys nobody can be told apart
            None => StatusCode::SERVICE_UNAVAILABLE,
            Some(keys) => match self.verify(token, keys) {
                Ok(()) => return Ok(()),
                Err(Rejected::Invalid) => return Err(StatusCode::UNAUTHORIZED),
                // The provider rotates its keys; try once with fresh ones
                Err(Rejected::UnknownKey) => StatusCode::UNAUTHORIZED,
            },
        };
        let fresh = self.fetch_if_allowed().await.ok_or(otherwise)?;
        self.verify(token, &fresh)
            .map_err(|_| StatusCode::UNAUTHORIZED)
    }

    /// Fetches and stores the keys, unless a fetch was already started within the last minute.
    /// The lock is taken to ask the gate and again to store, and is free during the fetch itself.
    async fn fetch_if_allowed(&self) -> Option<Arc<JwkSet>> {
        if !self.keys.lock().unwrap().gate.allow(Instant::now()) {
            return None;
        }
        let fresh = Arc::new(self.fetch().await?);
        self.keys.lock().unwrap().set = Some(fresh.clone());
        Some(fresh)
    }

    fn verify(&self, token: &str, keys: &JwkSet) -> Result<(), Rejected> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| Rejected::Invalid)?;
        let kid = header.kid.ok_or(Rejected::Invalid)?;
        let jwk = keys.find(&kid).ok_or(Rejected::UnknownKey)?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| Rejected::Invalid)?;
        // RS256 only: the algorithm comes from here, never from the token's own header
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.config.aud]);
        validation.set_issuer(&[&self.config.team]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .map(|_| ())
            .map_err(|_| Rejected::Invalid)
    }

    async fn fetch(&self) -> Option<JwkSet> {
        #[cfg(test)]
        self.fetches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let client = self.client.as_ref()?;
        let url = format!("{}/cdn-cgi/access/certs", self.config.team);
        let result = async {
            client
                .get(&url)
                .send()
                .await?
                .error_for_status()?
                .json::<JwkSet>()
                .await
        }
        .await;
        match result {
            Ok(keys) => Some(keys),
            Err(e) => {
                tracing::error!("fetching login keys from {url}: {e}");
                None
            }
        }
    }
}

/// Lets a request through only with a valid login token. Does nothing when the check is off.
pub async fn require_login(
    State(auth): State<Option<Arc<Auth>>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(auth) = auth {
        let token = request
            .headers()
            .get(TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());
        if let Err(status) = auth.check(token).await {
            return status.into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use serde_json::json;

    const TEAM: &str = "https://team.example.com";
    const AUD: &str = "test-aud";

    /// One key pair for the whole test binary: generating RSA keys is slow in debug builds.
    fn key() -> &'static (EncodingKey, JwkSet) {
        static KEY: OnceLock<(EncodingKey, JwkSet)> = OnceLock::new();
        KEY.get_or_init(|| {
            let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
            let pem = private.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
            let jwks = json!({ "keys": [{
                "kty": "RSA", "alg": "RS256", "use": "sig", "kid": "key-1",
                "n": URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),
                "e": URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),
            }]});
            (
                EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
                serde_json::from_value(jwks).unwrap(),
            )
        })
    }

    fn token(kid: &str, iss: &str, aud: &str, expires_in: i64) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        let exp = chrono::Utc::now().timestamp() + expires_in;
        let claims = json!({ "iss": iss, "aud": [aud], "exp": exp, "email": "parent@example.com" });
        jsonwebtoken::encode(&header, &claims, &key().0).unwrap()
    }

    fn auth() -> Auth {
        Auth::with_keys(
            AccessConfig {
                team: TEAM.into(),
                aud: AUD.into(),
            },
            key().1.clone(),
        )
    }

    #[tokio::test]
    async fn a_valid_token_passes() {
        assert_eq!(
            auth().check(Some(&token("key-1", TEAM, AUD, 600))).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn bad_tokens_are_unauthorized() {
        let a = auth();
        let unauthorized = Err(StatusCode::UNAUTHORIZED);
        assert_eq!(a.check(None).await, unauthorized);
        assert_eq!(a.check(Some("not-a-token")).await, unauthorized);
        assert_eq!(
            a.check(Some(&token("key-1", TEAM, AUD, -600))).await,
            unauthorized,
            "expired"
        );
        assert_eq!(
            a.check(Some(&token("key-1", TEAM, "other-aud", 600))).await,
            unauthorized,
            "audience"
        );
        assert_eq!(
            a.check(Some(&token("key-1", "https://evil.example.com", AUD, 600)))
                .await,
            unauthorized,
            "issuer"
        );
        assert_eq!(
            a.check(Some(&token("key-9", TEAM, AUD, 600))).await,
            unauthorized,
            "unknown key id"
        );

        // A token whose payload was changed after signing
        let good = token("key-1", TEAM, AUD, 600);
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged = URL_SAFE_NO_PAD
            .encode(json!({ "iss": TEAM, "aud": [AUD], "exp": 9_999_999_999i64 }).to_string());
        parts[1] = &forged;
        assert_eq!(
            a.check(Some(&parts.join("."))).await,
            unauthorized,
            "signature"
        );
    }

    #[tokio::test]
    async fn an_unsigned_token_is_rejected() {
        let payload = URL_SAFE_NO_PAD
            .encode(json!({ "iss": TEAM, "aud": [AUD], "exp": 9_999_999_999i64 }).to_string());
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","kid":"key-1"}"#);
        assert_eq!(
            auth().check(Some(&format!("{header}.{payload}."))).await,
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn a_token_without_issuer_or_audience_is_unauthorized() {
        let a = auth();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("key-1".into());
        let exp = chrono::Utc::now().timestamp() + 600;
        let sign =
            |claims: serde_json::Value| jsonwebtoken::encode(&header, &claims, &key().0).unwrap();
        // The same claims with nothing left out do pass, so the rejections below are about the gap
        assert_eq!(
            a.check(Some(&sign(
                json!({ "iss": TEAM, "aud": [AUD], "exp": exp })
            )))
            .await,
            Ok(())
        );
        assert_eq!(
            a.check(Some(&sign(json!({ "aud": [AUD], "exp": exp }))))
                .await,
            Err(StatusCode::UNAUTHORIZED),
            "no issuer"
        );
        assert_eq!(
            a.check(Some(&sign(json!({ "iss": TEAM, "exp": exp }))))
                .await,
            Err(StatusCode::UNAUTHORIZED),
            "no audience"
        );
    }

    #[tokio::test]
    async fn without_keys_a_token_gets_503_and_no_token_gets_401() {
        let a = Auth::without_keys(AccessConfig {
            team: TEAM.into(),
            aud: AUD.into(),
        });
        let good = token("key-1", TEAM, AUD, 600);
        assert_eq!(a.check(None).await, Err(StatusCode::UNAUTHORIZED));
        // The first attempt may fetch (and fails: there is no client); the second is inside the
        // gate's minute and must answer without trying
        assert_eq!(
            a.check(Some(&good)).await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            a.check(Some(&good)).await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(a.check(None).await, Err(StatusCode::UNAUTHORIZED));
        assert_eq!(a.fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_failed_start_up_fetch_holds_back_the_next_one() {
        let a = Auth::without_keys(AccessConfig {
            team: TEAM.into(),
            aud: AUD.into(),
        });
        a.warm().await;
        assert_eq!(
            a.check(Some(&token("key-1", TEAM, AUD, 600))).await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(a.fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn unknown_key_ids_cause_at_most_one_refetch() {
        let a = auth();
        for _ in 0..3 {
            assert_eq!(
                a.check(Some(&token("key-9", TEAM, AUD, 600))).await,
                Err(StatusCode::UNAUTHORIZED)
            );
        }
        assert_eq!(a.fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
        // Tokens signed with a known key still pass, and need no fetch
        assert_eq!(a.check(Some(&token("key-1", TEAM, AUD, 600))).await, Ok(()));
        assert_eq!(a.fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn the_team_url_must_be_https() {
        let error = access_config(Some("http://team.example.com".into()), Some(AUD.into()))
            .err()
            .expect("http must be refused");
        assert!(error.to_string().contains("access_team"), "{error}");
        assert!(error.to_string().contains("https://"), "{error}");
    }

    #[test]
    fn refetch_is_throttled() {
        let mut gate = RefetchGate::default();
        let t0 = std::time::Instant::now();
        assert!(gate.allow(t0));
        assert!(!gate.allow(t0 + std::time::Duration::from_secs(30)));
        assert!(gate.allow(t0 + std::time::Duration::from_secs(61)));
    }

    #[test]
    fn config_needs_both_settings_or_neither() {
        assert!(access_config(None, None).unwrap().is_none());
        let both = access_config(Some("https://team.example.com/".into()), Some(AUD.into()))
            .unwrap()
            .unwrap();
        assert_eq!(both.team, TEAM, "trailing slash removed");
        assert!(access_config(Some(TEAM.into()), None).is_err());
        assert!(access_config(None, Some(AUD.into())).is_err());
        // An empty string counts as unset, so an empty environment variable doesn't half-configure it
        assert!(
            access_config(Some(String::new()), Some(String::new()))
                .unwrap()
                .is_none()
        );
    }
}
