//! Login: passwords, browser sessions, throttling and the same-origin check.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use axum::http::{HeaderMap, HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha3::{Digest, Sha3_256};

/// 32 random bytes, URL-safe base64.
pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("the system has randomness");
    bytes
}

/// What the store keeps of a token: SHA3-256, hex.
pub fn hash_token(token: &str) -> String {
    Sha3_256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Compares without stopping at the first difference.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Argon2id PHC string of `password`. Slow on purpose: call from a
/// blocking thread.
pub fn hash_password(password: &str) -> Result<String, String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| error.to_string())
}

/// Whether `password` matches `hash`. `None` runs the hash against a fixed
/// one anyway, so an unknown user name takes as long as a wrong password.
pub fn verify_password(password: &str, hash: Option<&str>) -> bool {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let dummy = DUMMY.get_or_init(|| hash_password("not a password").unwrap_or_default());
    let (hash, real) = match hash {
        Some(hash) => (hash, true),
        None => (dummy.as_str(), false),
    };
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    let matches = Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok();
    real && matches
}

/// Passwords are at least this long.
pub const MIN_PASSWORD: usize = 8;

pub fn check_new_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(format!("a password has at least {MIN_PASSWORD} characters"));
    }
    if password.len() > 1024 {
        return Err("that password is too long".into());
    }
    Ok(())
}

/// Failed attempts before a key is locked.
const FREE_FAILURES: u32 = 5;
const FIRST_LOCK: Duration = Duration::from_secs(60);
const LONGEST_LOCK: Duration = Duration::from_secs(60 * 60);
/// Most keys remembered before unlocked ones are dropped.
const MAX_KEYS: usize = 100_000;
/// A key without failures for this long is forgotten.
const FORGET: Duration = Duration::from_secs(24 * 60 * 60);

/// Locks user names and addresses after failed attempts.
#[derive(Default)]
pub struct Throttle {
    keys: Mutex<HashMap<String, Failures>>,
}

struct Failures {
    count: u32,
    last: Instant,
    locked_until: Option<Instant>,
}

impl Throttle {
    /// How long until any of `keys` may try again; `None` when all may.
    pub fn wait(&self, keys: &[String]) -> Option<Duration> {
        let now = Instant::now();
        let map = self.keys.lock().unwrap();
        keys.iter()
            .filter_map(|key| map.get(key)?.locked_until)
            .filter(|until| *until > now)
            .map(|until| until - now)
            .max()
    }

    pub fn failed(&self, keys: &[String]) {
        let now = Instant::now();
        let mut map = self.keys.lock().unwrap();
        map.retain(|_, f| now.duration_since(f.last) < FORGET);
        // Attackers pick the user names; keep only keys that matter when
        // there are too many.
        if map.len() >= MAX_KEYS {
            map.retain(|_, f| f.locked_until.is_some_and(|until| until > now));
        }
        for key in keys {
            let failures = map.entry(key.clone()).or_insert(Failures {
                count: 0,
                last: now,
                locked_until: None,
            });
            failures.count += 1;
            failures.last = now;
            if failures.count >= FREE_FAILURES {
                let doublings = (failures.count - FREE_FAILURES).min(6);
                let lock = FIRST_LOCK.saturating_mul(1 << doublings).min(LONGEST_LOCK);
                failures.locked_until = Some(now + lock);
            }
        }
    }

    pub fn succeeded(&self, keys: &[String]) {
        let mut map = self.keys.lock().unwrap();
        for key in keys {
            map.remove(key);
        }
    }
}

/// The session cookie's name: `__Host-` prefixed over HTTPS, so no other
/// host or path can set it.
pub fn cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-session" } else { "session" }
}

pub fn session_cookie(secure: bool, token: &str) -> HeaderValue {
    let mut cookie = format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        cookie_name(secure),
        crate::store::SESSION_LIFETIME
    );
    if secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).expect("tokens are URL-safe")
}

pub fn clear_cookie(secure: bool) -> HeaderValue {
    let mut cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        cookie_name(secure)
    );
    if secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).expect("static cookie")
}

/// The session token in the request's cookies.
pub fn session_token(headers: &HeaderMap, secure: bool) -> Option<String> {
    let name = cookie_name(secure);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
}

/// Whether the request comes from a page of this server: its `Origin` names
/// the same host and port as `Host`. Browsers send `Origin` with every
/// WebSocket and every request that is not a GET or HEAD.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let (Some(origin), Some(host)) = (
        headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
        headers.get(header::HOST).and_then(|v| v.to_str().ok()),
    ) else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    authority.eq_ignore_ascii_case(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_verify_and_unknown_users_never_match() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", Some(&hash)));
        assert!(!verify_password("wrong horse", Some(&hash)));
        assert!(!verify_password("not a password", None));
        assert!(check_new_password("short").is_err());
        assert!(check_new_password("long enough").is_ok());
    }

    #[test]
    fn failures_lock_after_five_and_success_clears() {
        let throttle = Throttle::default();
        let keys = ["user:a".to_owned(), "ip:1".to_owned()];
        for _ in 0..4 {
            throttle.failed(&keys);
        }
        assert!(throttle.wait(&keys).is_none());
        throttle.failed(&keys);
        let wait = throttle.wait(&keys[..1]).unwrap();
        assert!(wait > Duration::from_secs(50) && wait <= FIRST_LOCK);
        throttle.failed(&keys);
        assert!(throttle.wait(&keys).unwrap() > FIRST_LOCK);
        throttle.succeeded(&keys[..1]);
        assert!(throttle.wait(&keys[..1]).is_none());
        assert!(throttle.wait(&keys[1..]).is_some());
    }

    #[test]
    fn cookies_and_origins() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "a=1; session=tok; b=2".parse().unwrap());
        assert_eq!(session_token(&headers, false).as_deref(), Some("tok"));
        assert_eq!(session_token(&headers, true), None);
        assert!(
            session_cookie(true, "t")
                .to_str()
                .unwrap()
                .contains("__Host-session=t")
        );

        headers.insert(header::HOST, "web.example:8080".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "http://web.example:8080".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(!same_origin(&headers));
        assert!(equal(b"abc", b"abc") && !equal(b"abc", b"abd") && !equal(b"a", b"ab"));
    }
}
