//! Single sign-on with OpenID Connect: the authorization code flow with
//! PKCE (S256), a state and a nonce.
//!
//! The ID token comes straight from the provider's token endpoint over TLS,
//! with the client authenticated, so its signature is not checked; its
//! issuer, audience, expiry and nonce are (OpenID Connect Core 1.0, section
//! 3.1.3.7).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::random_token;
use crate::config::{Config, Provider};
use crate::now;

/// How long a sign-in may take at the provider.
const PENDING_FOR: Duration = Duration::from_secs(10 * 60);
/// How long a provider's discovery document is kept.
const DISCOVERY_FOR: Duration = Duration::from_secs(60 * 60);
/// Most sign-ins waiting for the provider at once.
const MAX_PENDING: usize = 10_000;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

/// A sign-in that went to the provider and has not come back.
#[derive(Clone)]
pub struct Pending {
    pub provider: String,
    verifier: String,
    nonce: String,
    /// The account to connect the identity to, when a logged-in user
    /// started it.
    pub link_user: Option<String>,
    started: Instant,
}

/// What the provider says about the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claims {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub preferred_username: Option<String>,
}

#[derive(Default)]
pub struct Oidc {
    discovered: Mutex<HashMap<String, (Discovery, Instant)>>,
    pending: Mutex<HashMap<String, Pending>>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

pub fn redirect_uri(config: &Config, provider: &Provider) -> String {
    format!("{}/auth/oidc/{}/callback", config.public_url(), provider.id)
}

impl Oidc {
    /// The provider's discovery document. Blocks: call from a blocking
    /// thread.
    pub fn discover(&self, provider: &Provider) -> Result<Discovery, String> {
        if let Some((discovery, at)) = self.discovered.lock().unwrap().get(&provider.id)
            && at.elapsed() < DISCOVERY_FOR
        {
            return Ok(discovery.clone());
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            provider.issuer.trim_end_matches('/')
        );
        let mut response = agent()
            .get(&url)
            .call()
            .map_err(|e| format!("{url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("{url}: {}", response.status()));
        }
        let discovery: Discovery = response
            .body_mut()
            .read_json()
            .map_err(|e| format!("{url}: {e}"))?;
        if discovery.issuer.trim_end_matches('/') != provider.issuer.trim_end_matches('/') {
            return Err(format!(
                "{url} names the issuer {}, not {}",
                discovery.issuer, provider.issuer
            ));
        }
        self.discovered
            .lock()
            .unwrap()
            .insert(provider.id.clone(), (discovery.clone(), Instant::now()));
        Ok(discovery)
    }

    /// The provider's login page to send the browser to.
    pub fn start(
        &self,
        config: &Config,
        provider: &Provider,
        link_user: Option<String>,
    ) -> Result<String, String> {
        let discovery = self.discover(provider)?;
        let state = random_token();
        let verifier = random_token();
        let nonce = random_token();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let scopes = provider.scopes.join(" ");
        let redirect = redirect_uri(config, provider);
        let query = [
            ("response_type", "code"),
            ("client_id", provider.client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("scope", scopes.as_str()),
            ("state", state.as_str()),
            ("nonce", nonce.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];
        let separator = if discovery.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        let url = format!(
            "{}{separator}{}",
            discovery.authorization_endpoint,
            form(&query)
        );
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.started.elapsed() < PENDING_FOR);
        // Anyone can start sign-ins; keep their memory bounded.
        if pending.len() >= MAX_PENDING {
            return Err("too many sign-ins at once, try again in a few minutes".into());
        }
        pending.insert(
            state,
            Pending {
                provider: provider.id.clone(),
                verifier,
                nonce,
                link_user,
                started: Instant::now(),
            },
        );
        Ok(url)
    }

    /// Takes the sign-in that `state` names. Each works once.
    pub fn take(&self, state: &str) -> Option<Pending> {
        let pending = self.pending.lock().unwrap().remove(state)?;
        (pending.started.elapsed() < PENDING_FOR).then_some(pending)
    }

    /// Trades `code` for the user's claims. Blocks: call from a blocking
    /// thread.
    pub fn finish(
        &self,
        config: &Config,
        provider: &Provider,
        pending: &Pending,
        code: &str,
    ) -> Result<Claims, String> {
        let discovery = self.discover(provider)?;
        let redirect = redirect_uri(config, provider);
        let secret = Config::client_secret(provider);
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect.as_str()),
            ("code_verifier", pending.verifier.as_str()),
        ];
        let mut request = agent()
            .post(&discovery.token_endpoint)
            .header("Accept", "application/json");
        match &secret {
            // client_secret_basic, the default of every provider.
            Some(secret) => {
                let basic = STANDARD.encode(format!(
                    "{}:{}",
                    encode(&provider.client_id),
                    encode(secret)
                ));
                request = request.header("Authorization", &format!("Basic {basic}"));
            }
            // A public client.
            None => fields.push(("client_id", provider.client_id.as_str())),
        }
        let mut response = request
            .content_type("application/x-www-form-urlencoded")
            .send(form(&fields))
            .map_err(|e| format!("token endpoint: {e}"))?;
        let body: serde_json::Value = response
            .body_mut()
            .read_json()
            .map_err(|e| format!("token endpoint: {e}"))?;
        if !response.status().is_success() {
            let error = body["error_description"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("refused");
            return Err(format!("token endpoint: {error}"));
        }
        let id_token = body["id_token"]
            .as_str()
            .ok_or("the provider sent no ID token")?;
        check_id_token(
            id_token,
            &discovery.issuer,
            &provider.client_id,
            &pending.nonce,
            now(),
        )
    }
}

/// Reads and checks an ID token's claims.
fn check_id_token(
    token: &str,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now: u64,
) -> Result<Claims, String> {
    let payload = token.split('.').nth(1).ok_or("the ID token is not a JWT")?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| "the ID token is not a JWT")?;
    let claims: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| "the ID token is not a JWT")?;
    if claims["iss"].as_str() != Some(issuer) {
        return Err("the ID token comes from another issuer".into());
    }
    let audiences: Vec<&str> = match &claims["aud"] {
        serde_json::Value::String(aud) => vec![aud.as_str()],
        serde_json::Value::Array(list) => list.iter().filter_map(|a| a.as_str()).collect(),
        _ => Vec::new(),
    };
    if !audiences.contains(&client_id) {
        return Err("the ID token is for another client".into());
    }
    if audiences.len() > 1 && claims["azp"].as_str() != Some(client_id) {
        return Err("the ID token is for another client".into());
    }
    // A minute of leeway for clocks that drift.
    if claims["exp"].as_u64().is_none_or(|exp| exp + 60 < now) {
        return Err("the ID token expired".into());
    }
    let sent = claims["nonce"].as_str().unwrap_or_default();
    if !crate::auth::equal(sent.as_bytes(), nonce.as_bytes()) {
        return Err("the ID token answers another sign-in".into());
    }
    let subject = claims["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("the ID token names no user")?;
    let email_verified = match &claims["email_verified"] {
        serde_json::Value::Bool(v) => *v,
        // Some providers send a string.
        serde_json::Value::String(v) => v == "true",
        _ => false,
    };
    Ok(Claims {
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
        email: claims["email"].as_str().map(str::to_lowercase),
        email_verified,
        preferred_username: claims["preferred_username"].as_str().map(str::to_owned),
    })
}

/// Whether `email` may create an account: it matches a `*@domain` or full
/// address pattern of `allow`.
pub fn allowed(allow: &[String], email: &str) -> bool {
    let email = email.to_lowercase();
    allow.iter().any(|pattern| {
        let pattern = pattern.to_lowercase();
        match pattern.strip_prefix('*') {
            Some(domain) => domain.starts_with('@') && email.ends_with(domain),
            None => pattern == email,
        }
    })
}

/// Percent-encodes `value` for a query or form.
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256"}"#);
        let body = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{body}.sig")
    }

    #[test]
    fn id_tokens_are_checked() {
        let good = serde_json::json!({
            "iss": "https://sso", "aud": "app", "exp": 1000, "nonce": "n",
            "sub": "42", "email": "Me@Example.com", "email_verified": true
        });
        let claims = check_id_token(&token(good.clone()), "https://sso", "app", "n", 900).unwrap();
        assert_eq!(claims.subject, "42");
        assert_eq!(claims.email.as_deref(), Some("me@example.com"));
        assert!(claims.email_verified);

        let check = |change: serde_json::Value, now| {
            let mut claims = good.clone();
            for (k, v) in change.as_object().unwrap() {
                claims[k] = v.clone();
            }
            check_id_token(&token(claims), "https://sso", "app", "n", now)
        };
        assert!(check(serde_json::json!({"iss": "https://evil"}), 900).is_err());
        assert!(check(serde_json::json!({"aud": "other"}), 900).is_err());
        assert!(check(serde_json::json!({"aud": ["app", "other"]}), 900).is_err());
        assert!(
            check(
                serde_json::json!({"aud": ["app", "other"], "azp": "app"}),
                900
            )
            .is_ok()
        );
        assert!(check(serde_json::json!({}), 2000).is_err());
        assert!(check(serde_json::json!({"nonce": "m"}), 900).is_err());
        assert!(check(serde_json::json!({"sub": ""}), 900).is_err());
        assert!(check_id_token("garbage", "https://sso", "app", "n", 900).is_err());
    }

    #[test]
    fn allow_patterns_and_encoding() {
        let allow = ["*@example.com".to_owned(), "Friend@Other.org".to_owned()];
        assert!(allowed(&allow, "a@example.com"));
        assert!(allowed(&allow, "friend@other.org"));
        assert!(!allowed(&allow, "a@evil-example.com"));
        assert!(!allowed(&allow, "a@example.com.evil"));
        assert!(!allowed(&["*example.com".to_owned()], "a@badexample.com"));
        assert_eq!(encode("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
    }
}
