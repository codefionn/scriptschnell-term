//! Links to scriptschnell-term-web servers, which let browsers and phones reach this
//! machine.
//!
//! `scriptschnell-term-remote link URL CODE` hands this machine's public key and a
//! pairing code from the web app to the web server, which answers with its
//! own key and the address machines connect to. Both go into `links.json`.
//!
//! Every server on this machine then dials each link (see
//! [`crate::server::Server::spawn`]) and runs the usual handshake as the
//! client, with its host key, checking the web server's key against the
//! pinned one. After the handshake the roles turn around: this machine
//! serves channels to the web server like to any client, so the web server's
//! terminals are ordinary sessions owned by its key.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Paths;
use crate::keys::PublicKey;

/// How long the first retry waits after a link dropped. Each further one
/// waits twice as long, up to [`RETRY_MAX`].
pub const RETRY_FIRST: Duration = Duration::from_secs(1);
pub const RETRY_MAX: Duration = Duration::from_secs(60);
/// A connection that lasted this long resets the retry wait.
pub const STABLE: Duration = Duration::from_secs(60);

/// One web server this machine is linked to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// The web server as the user named it, `https://web.example`.
    pub url: String,
    /// `host:port` this machine connects to.
    pub address: String,
    /// The web server's key line, pinned when linked.
    pub server_key: String,
    /// The id the web server gave this machine.
    pub machine: String,
}

impl Link {
    pub fn key(&self) -> io::Result<PublicKey> {
        PublicKey::parse(&self.server_key).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the pinned key of {} is not valid", self.url),
            )
        })
    }
}

fn file(paths: &Paths) -> PathBuf {
    paths.dir.join("links.json")
}

/// The links in `links.json`. A missing file has none.
pub fn load(paths: &Paths) -> io::Result<Vec<Link>> {
    let path = file(paths);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn save(paths: &Paths, links: &[Link]) -> io::Result<()> {
    crate::create_private_dir(&paths.dir)?;
    let path = file(paths);
    let partial = path.with_extension("partial");
    let mut text = serde_json::to_vec_pretty(links).map_err(io::Error::other)?;
    text.push(b'\n');
    let mut out = crate::sys::mode(
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true),
        0o600,
    )
    .open(&partial)?;
    io::Write::write_all(&mut out, &text)?;
    drop(out);
    std::fs::rename(partial, path)
}

/// `url` without a trailing slash. Plain HTTP is refused except to this
/// machine, because the answer pins the web server's key.
pub fn normalize(url: &str) -> io::Result<String> {
    let url = url.trim().trim_end_matches('/');
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', '?']).next().unwrap_or_default();
        let host = host.rsplit_once(':').map_or(host, |(host, port)| {
            if port.parse::<u16>().is_ok() {
                host
            } else {
                rest
            }
        });
        if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "link over https://, plain http:// only reaches this machine",
            ));
        }
        rest
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the web server's address starts with https://",
        ));
    };
    if rest.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no host"));
    }
    Ok(url.to_owned())
}

#[derive(Serialize)]
struct Redeem<'a> {
    code: &'a str,
    name: &'a str,
    key: String,
}

#[derive(Deserialize)]
struct Redeemed {
    machine: String,
    server_key: String,
    address: String,
}

/// Links this machine to the web server at `url` with a pairing `code`
/// from its web app, and keeps the link. A link to the same URL is
/// replaced. `fingerprint` is the web server's key fingerprint to compare
/// the answer against before anything is pinned; `None` trusts the answer's
/// TLS alone.
pub fn redeem(
    paths: &Paths,
    url: &str,
    code: &str,
    name: &str,
    fingerprint: Option<&str>,
) -> io::Result<Link> {
    let url = normalize(url)?;
    let identity = paths.load_identity()?;
    let body = Redeem {
        code: code.trim(),
        name,
        key: identity.public().to_line(""),
    };
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut response = agent
        .post(format!("{url}/v1/link/redeem"))
        .send_json(&body)
        .map_err(|error| io::Error::other(format!("{url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let message = response
            .body_mut()
            .read_json::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, message));
    }
    let redeemed: Redeemed = response
        .body_mut()
        .read_json()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let link = Link {
        url,
        address: redeemed.address,
        server_key: redeemed.server_key,
        machine: redeemed.machine,
    };
    let key = link.key()?;
    if let Some(expected) = fingerprint
        && key.fingerprint() != expected
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} answered with key {}, not {expected}; not linking",
                link.url,
                key.fingerprint()
            ),
        ));
    }
    let mut links = load(paths)?;
    links.retain(|old| old.url != link.url);
    links.push(link.clone());
    save(paths, &links)?;
    Ok(link)
}

/// Removes the link to `url`. False when there was none.
pub fn remove(paths: &Paths, url: &str) -> io::Result<bool> {
    let url = url.trim().trim_end_matches('/');
    let mut links = load(paths)?;
    let before = links.len();
    links.retain(|link| link.url != url);
    if links.len() == before {
        return Ok(false);
    }
    save(paths, &links)?;
    Ok(true)
}

/// How long to wait after `misses` connections in a row failed.
pub fn retry_after(misses: u32) -> Duration {
    RETRY_FIRST
        .saturating_mul(1 << misses.min(6))
        .min(RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_need_https_except_to_this_machine() {
        assert_eq!(
            normalize("https://web.example/").unwrap(),
            "https://web.example"
        );
        assert!(normalize("http://127.0.0.1:8080").is_ok());
        assert!(normalize("http://localhost/").is_ok());
        assert!(normalize("http://web.example").is_err());
        assert!(normalize("http://localhost.evil.example").is_err());
        assert!(normalize("web.example").is_err());
        assert!(normalize("https://").is_err());
    }

    #[test]
    fn links_round_trip_privately_and_retries_back_off() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_owned()));
        assert!(load(&paths).unwrap().is_empty());
        let key = crate::keys::Identity::generate().unwrap();
        let link = Link {
            url: "https://web.example".into(),
            address: "web.example:7724".into(),
            server_key: key.public().to_line(""),
            machine: "m1".into(),
        };
        save(&paths, std::slice::from_ref(&link)).unwrap();
        assert_eq!(load(&paths).unwrap(), std::slice::from_ref(&link));
        assert_eq!(link.key().unwrap(), *key.public());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file(&paths))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(remove(&paths, "https://web.example/").unwrap());
        assert!(!remove(&paths, "https://web.example").unwrap());

        let waits: Vec<_> = (0..9).map(|m| retry_after(m).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }
}
