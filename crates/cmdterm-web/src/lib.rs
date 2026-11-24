//! scriptschnell-term-web: reach scriptschnell-term machines from a browser or a phone.
//! The crate README covers setup, the protocol and the HTTP API.

pub mod api;
pub mod auth;
pub mod config;
pub mod hub;
pub mod oidc;
pub mod store;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::auth::Throttle;
use crate::config::Config;
use crate::hub::Hub;
use crate::oidc::Oidc;
use crate::store::Store;

/// The files of the web app, from `web/FILES.txt`.
pub static ASSETS: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// Everything a request handler reaches.
pub struct App {
    pub config: Config,
    pub store: Arc<Store>,
    pub hub: Arc<Hub>,
    pub throttle: Throttle,
    pub oidc: Oidc,
    /// Pairing codes that have not been used, by code without dashes.
    pub pairings: Mutex<HashMap<String, Pairing>>,
    /// The `--dev` login token, hashed.
    pub dev_token: Option<String>,
}

/// A code that links one machine to `user`.
pub struct Pairing {
    pub user: String,
    pub until: Instant,
}

impl App {
    pub fn new(
        config: Config,
        store: Arc<Store>,
        hub: Arc<Hub>,
        dev_token: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            store,
            hub,
            throttle: Throttle::default(),
            oidc: Oidc::default(),
            pairings: Mutex::default(),
            dev_token: dev_token.map(|t| auth::hash_token(&t)),
        })
    }
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A short random id for users and machines.
pub fn random_id() -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(auth::random_bytes::<9>())
}
