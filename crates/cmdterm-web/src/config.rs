//! `web.json` in the data directory. The crate README describes every key.

use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub public_url: Option<String>,
    pub link_listen: String,
    pub link_address: Option<String>,
    pub tls: Option<Tls>,
    pub password_login: bool,
    pub oidc: Vec<Provider>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// One single sign-on provider.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// E-mail patterns that may create an account: `*@domain` or a full
    /// address.
    #[serde(default)]
    pub allow: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    ["openid", "email", "profile"].map(String::from).to_vec()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".into(),
            public_url: None,
            link_listen: "0.0.0.0:7724".into(),
            link_address: None,
            tls: None,
            password_login: true,
            oidc: Vec::new(),
        }
    }
}

impl Config {
    /// Reads `web.json` in `dir`. A missing file means the defaults.
    pub fn load(dir: &Path) -> io::Result<Self> {
        let path = dir.join("web.json");
        let config: Self = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error),
        };
        for (i, provider) in config.oidc.iter().enumerate() {
            let valid = !provider.id.is_empty()
                && provider
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !valid || config.oidc[..i].iter().any(|p| p.id == provider.id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "oidc id {:?} must be unique letters, digits, - or _",
                        provider.id
                    ),
                ));
            }
        }
        Ok(config)
    }

    /// The address browsers use, without a trailing slash.
    pub fn public_url(&self) -> String {
        match &self.public_url {
            Some(url) => url.trim_end_matches('/').to_owned(),
            None => {
                let scheme = if self.tls.is_some() { "https" } else { "http" };
                format!("{scheme}://{}", self.listen)
            }
        }
    }

    /// Whether browsers reach the server over HTTPS.
    pub fn secure(&self) -> bool {
        self.public_url().starts_with("https://")
    }

    /// `host:port` that machines connect to.
    pub fn link_address(&self) -> String {
        if let Some(address) = &self.link_address {
            return address.clone();
        }
        let url = self.public_url();
        let authority = url.split("://").nth(1).unwrap_or(&url);
        let authority = authority.split('/').next().unwrap_or_default();
        // Drop the web port; keep IPv6 brackets.
        let host = match authority.rsplit_once(':') {
            Some((host, port)) if port.parse::<u16>().is_ok() && !host.ends_with(':') => host,
            _ => authority,
        };
        let port = self.link_listen.rsplit_once(':').map_or("7724", |(_, p)| p);
        format!("{host}:{port}")
    }

    /// The client secret of `provider`, from the file or the environment.
    pub fn client_secret(provider: &Provider) -> Option<String> {
        let name = format!(
            "CMDTERM_WEB_OIDC_{}_SECRET",
            provider.id.to_ascii_uppercase().replace('-', "_")
        );
        std::env::var(name)
            .ok()
            .or_else(|| provider.client_secret.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_follow_the_public_url() {
        let config = Config {
            public_url: Some("https://web.example/".into()),
            ..Config::default()
        };
        assert_eq!(config.public_url(), "https://web.example");
        assert!(config.secure());
        assert_eq!(config.link_address(), "web.example:7724");

        let local = Config::default();
        assert_eq!(local.public_url(), "http://127.0.0.1:8080");
        assert!(!local.secure());
        assert_eq!(local.link_address(), "127.0.0.1:7724");

        let v6 = Config {
            public_url: Some("https://[::1]:8443".into()),
            link_listen: "[::]:9000".into(),
            ..Config::default()
        };
        assert_eq!(v6.link_address(), "[::1]:9000");
    }

    #[test]
    fn provider_ids_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let provider = r#"{"id": "a b", "name": "A", "issuer": "https://i", "client_id": "c"}"#;
        std::fs::write(
            dir.path().join("web.json"),
            format!(r#"{{"oidc": [{provider}]}}"#),
        )
        .unwrap();
        assert!(Config::load(dir.path()).is_err());
        assert!(
            Config::load(&dir.path().join("none"))
                .unwrap()
                .oidc
                .is_empty()
        );
    }
}
