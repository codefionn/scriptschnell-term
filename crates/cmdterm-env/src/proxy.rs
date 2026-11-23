//! Network access for sandboxes through a filtering proxy on the host.
//!
//! [`NetworkProxy`] is an HTTP proxy that only reaches allowed hosts. It
//! answers `CONNECT host:port` (HTTPS, and anything else tunnelled) and
//! plain HTTP requests in absolute form, and refuses every other host with
//! `403`. It listens on a Unix socket and on a loopback TCP port (the TCP
//! port only on Windows).
//!
//! A local bubblewrap sandbox is isolated unless it has the host's network
//! (`network` and `host_network`): it has a network namespace with only a
//! loopback, so servers inside never bind the host's ports. The proxy
//! socket is bound into it, and [`proxy_bridge_main`] (the
//! `cmdterm-proxy-bridge` program) listens on the sandbox's loopback at the
//! same port, forwards each connection to the socket, and runs the command.
//! That makes the proxy the only way out, for a sandbox with the network
//! and one without. The same bridge joins the port hub (`ports.rs`) when
//! the sandbox has one. Seatbelt cannot remount, so a Seatbelt sandbox may
//! connect to the TCP port only. Inside, `HTTP_PROXY`, `HTTPS_PROXY` and
//! `ALL_PROXY` point to the port, and names are resolved on the host, so
//! the sandbox needs no DNS.
//!
//! Every process on the machine can connect to a loopback port, so each
//! TCP port has a random [`ProxyToken`]. The proxy URL a sandbox gets
//! carries it as the password (`http://cmdterm:TOKEN@127.0.0.1:PORT`), and
//! HTTP clients send it as `Proxy-Authorization: Basic`. The first request
//! of a TCP connection without it gets `407`, and the header never goes
//! upstream. The Unix socket needs no token: only sandboxes it is bound
//! into reach it, so the bridge's URL has none.
//!
//! [`NetworkProxy::intercept_tls`] makes the proxy end TLS itself, with a
//! CA the sandbox trusts for hosts with valid certificates and a second,
//! untrusted CA for the others (see `proxy/server/tls.rs`).
//!
//! Bytes from the sandbox only ever reach an allowed host: after the request
//! head, a connection is spliced to the one upstream it named, so further
//! requests on it go to the same server.
//!
//! [`ProxyRules`] (see `proxy/rules.rs`) make the proxy read the requests
//! to some hosts: it swaps the fake credentials sandboxes hold for real
//! ones, and holds back requests that publish, such as a push, until the
//! user allows them through [`Approvals`]. Tunnels to these hosts are
//! always intercepted, and refused while interception is off.
//!
//! [`Blocking`] (see `proxy/blocklist.rs`) refuses hosts on lists of ad,
//! tracker and malware domains, whatever the allowed hosts say.
//!
//! A sandbox that shares the host's network can use the proxy too, for its
//! rules and blocklists only ([`NetworkProxy::allow_any_host`]). It reaches
//! the TCP port directly, and programs that ignore the proxy variables go
//! around it; they reach the hosts with fake credentials only. An isolated
//! sandbox that is allowed any host ([`NetworkProxy::allow_any_host`]) still
//! sends only proxy-aware traffic out: HTTP, HTTPS, CONNECT tunnels and
//! git over SSH.
//!
//! Git over SSH (see `proxy/ssh.rs`) is an `SSH` request that names a host
//! and one of git's remote commands. The proxy runs this machine's `ssh`
//! for it, with the user's keys, which stay outside the sandbox, and
//! checks pushes against the same guards as pushes over HTTPS.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

mod approval;
mod blocklist;
mod rules;
mod server;
mod ssh;

pub(crate) use ssh::GIT_SSH_FLAG;

pub use approval::{Approval, Approvals};
pub use blocklist::{Blocking, Blocklist};
pub use rules::{
    AGENT_SETTINGS_GUARD, GuardKind, GuardMode, GuardRule, KEYRING_GUARD, ProxyRules, SecretRule,
    builtin_guards, fake_secret, named_fake_secret, valid_request_pattern,
};
#[cfg(unix)]
pub use server::proxy_bridge_main;
pub(crate) use server::{
    Duplex, authorized, connect, read_head, respond, splice, split_authority, unauthorized,
};
pub use server::{NetworkProxy, Refusal, RefusalLog};

/// Where a sandbox reaches a [`NetworkProxy`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxProxy {
    /// The proxy's Unix socket on the host. Unused on Windows.
    pub socket: PathBuf,
    /// The proxy's port on the host's loopback, and the bridge's port on
    /// the sandbox's loopback.
    pub port: u16,
    /// The certificates sandboxes trust while the proxy intercepts TLS:
    /// this machine's roots plus the proxy's CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_bundle: Option<PathBuf>,
    /// The password [`Self::port`] takes. `None` for a sandbox that only
    /// reaches the socket, through the bridge, which needs none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<ProxyToken>,
}

impl SandboxProxy {
    /// The proxy URL processes inside use, with the token as the password
    /// when there is one.
    pub fn url(&self) -> String {
        match &self.token {
            Some(token) => token.url(self.port),
            None => format!("http://127.0.0.1:{}", self.port),
        }
    }

    /// This endpoint without its token, for a bubblewrap sandbox with a
    /// network namespace of its own: the bridge takes it to the socket, and
    /// the token stays off bwrap's command line, which every user of the
    /// machine can read.
    pub fn without_token(mut self) -> Self {
        self.token = None;
        self
    }

    /// Variables that send HTTP clients through the proxy. Loopback stays
    /// direct, so servers inside the sandbox stay reachable. `ca` is where
    /// the sandbox sees [`Self::ca_bundle`]; the common TLS stacks are
    /// pointed to it.
    pub fn vars(&self, ca: Option<&str>) -> Vec<(String, String)> {
        let url = self.url();
        let no_proxy = "localhost,127.0.0.1,::1".to_owned();
        let mut vars = Vec::new();
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            vars.push((key.to_owned(), url.clone()));
            vars.push((key.to_ascii_lowercase(), url.clone()));
        }
        vars.push(("NO_PROXY".into(), no_proxy.clone()));
        vars.push(("no_proxy".into(), no_proxy));
        if let Some(ca) = ca {
            for key in [
                "SSL_CERT_FILE",
                "CURL_CA_BUNDLE",
                "REQUESTS_CA_BUNDLE",
                "NODE_EXTRA_CA_CERTS",
                "GIT_SSL_CAINFO",
                "CARGO_HTTP_CAINFO",
                "DENO_CERT",
                "PIP_CERT",
            ] {
                vars.push((key.into(), ca.to_owned()));
            }
        }
        vars
    }
}

/// The user name of the proxy URL. The proxy checks only the password.
pub const PROXY_USER: &str = "cmdterm";

/// The random password of a proxy's TCP port. It shows as `ProxyToken(..)`
/// in debug output, so logs of a [`SandboxProxy`] do not carry it.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProxyToken(String);

impl fmt::Debug for ProxyToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProxyToken(..)")
    }
}

impl ProxyToken {
    /// 32 random bytes in hex, which a URL takes without escapes.
    pub(crate) fn generate() -> io::Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
        let mut hex = String::with_capacity(64);
        for byte in bytes {
            use fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(Self(hex))
    }

    /// The URL of a proxy on `port` of this machine's loopback, with this
    /// token as the password.
    pub fn url(&self, port: u16) -> String {
        format!("http://{PROXY_USER}:{}@127.0.0.1:{port}", self.0)
    }

    /// The `Proxy-Authorization` value that carries this token.
    pub(crate) fn header(&self) -> String {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{PROXY_USER}:{}", self.0))
        )
    }

    /// True when `value`, a `Proxy-Authorization` header, is `Basic` with
    /// this token as the password. The user name is not checked. The
    /// password is compared in constant time.
    pub(crate) fn accepts(&self, value: &str) -> bool {
        let value = value.trim();
        let Some((scheme, credentials)) = value.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("basic") {
            return false;
        }
        let Ok(decoded) = STANDARD.decode(credentials.trim()) else {
            return false;
        };
        let Some(colon) = decoded.iter().position(|&b| b == b':') else {
            return false;
        };
        constant_time_eq(&decoded[colon + 1..], self.0.as_bytes())
    }
}

/// Compares without stopping at the first difference, so the time taken
/// does not tell how much of a guess was right. The length is not secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// True when `pattern` is a host the allowed list can hold: a name or
/// address such as `crates.io`, or `*.example.com` for every subdomain.
pub fn valid_host_pattern(pattern: &str) -> bool {
    let name = pattern.strip_prefix("*.").unwrap_or(pattern);
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._:[]".contains(&b))
}

/// True when one of `allowed` matches `host`. `*.example.com` matches
/// subdomains, not `example.com` itself. Case and a trailing dot are ignored.
pub fn host_allowed(allowed: &[String], host: &str) -> bool {
    let host = normalize(host);
    allowed.iter().any(|pattern| {
        let pattern = normalize(pattern);
        match pattern.strip_prefix("*.") {
            Some(domain) => host
                .strip_suffix(domain)
                .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
            None => host == pattern,
        }
    })
}

fn normalize(host: &str) -> String {
    let host = host.trim();
    let host = host.strip_prefix('[').unwrap_or(host);
    let host = host.strip_suffix(']').unwrap_or(host);
    host.trim_end_matches('.').to_ascii_lowercase()
}

static BRIDGE: OnceLock<Vec<OsString>> = OnceLock::new();
static BUILT_IN_BRIDGE: OnceLock<OsString> = OnceLock::new();

/// Sets the `cmdterm-proxy-bridge` executable for this process. Only the
/// first call counts.
pub fn set_proxy_bridge(program: PathBuf) {
    let _ = BRIDGE.set(vec![program.into_os_string()]);
}

/// Says that the running executable calls [`proxy_bridge_main`] when its
/// first argument is `flag`. Sandboxes then run it as the bridge when no
/// `cmdterm-proxy-bridge` sits next to it, as after `cargo build -p cmdterm`.
pub fn set_built_in_proxy_bridge(flag: &str) {
    let _ = BUILT_IN_BRIDGE.set(flag.into());
}

/// The command line that starts the bridge: what [`set_proxy_bridge`] set,
/// see [`bridge_command`] otherwise.
pub(crate) fn proxy_bridge() -> &'static [OsString] {
    BRIDGE.get_or_init(|| {
        let exe = std::env::current_exe().ok();
        bridge_command(exe.as_deref(), BUILT_IN_BRIDGE.get(), Path::is_file)
    })
}

/// [`proxy_bridge`], unless it is only the name of a program on `PATH`
/// that may not be there, as in test binaries.
pub(crate) fn known_proxy_bridge() -> Option<&'static [OsString]> {
    let bridge = proxy_bridge();
    let found = bridge
        .first()
        .is_some_and(|program| Path::new(program).is_absolute());
    found.then_some(bridge)
}

/// A `cmdterm-proxy-bridge` next to `exe`, otherwise `exe` itself with the
/// `built_in` flag, otherwise the bridge on `PATH`.
fn bridge_command(
    exe: Option<&Path>,
    built_in: Option<&OsString>,
    exists: impl Fn(&Path) -> bool,
) -> Vec<OsString> {
    let name = format!("cmdterm-proxy-bridge{}", std::env::consts::EXE_SUFFIX);
    if let Some(beside) = exe.and_then(|exe| Some(exe.parent()?.join(&name)))
        && exists(&beside)
    {
        return vec![beside.into_os_string()];
    }
    match (exe, built_in) {
        (Some(exe), Some(flag)) => vec![exe.as_os_str().to_owned(), flag.clone()],
        _ => vec![name.into()],
    }
}

/// Where the bridge puts the proxy socket inside a bubblewrap sandbox. The
/// sandbox's `/tmp` is a fresh tmpfs, so bwrap can create it.
pub(crate) const INNER_SOCKET: &str = "/tmp/.cmdterm-proxy.sock";

/// Where a bubblewrap sandbox sees [`SandboxProxy::ca_bundle`].
pub(crate) const INNER_CA: &str = "/tmp/.cmdterm-proxy-ca.pem";

/// Where the bridge finds the port hub's socket inside a bubblewrap
/// sandbox (see `ports.rs`).
pub(crate) const INNER_PORTS: &str = "/tmp/.cmdterm-ports.sock";

#[cfg(test)]
mod tests {
    use super::*;

    fn list(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|h| (*h).to_owned()).collect()
    }

    #[test]
    fn the_bridge_is_the_program_beside_the_app_else_the_app_itself() {
        let exe = Path::new("/opt/term/scriptschnell-term");
        let beside = Path::new("/opt/term").join(format!(
            "cmdterm-proxy-bridge{}",
            std::env::consts::EXE_SUFFIX
        ));
        let flag = OsString::from("--proxy-bridge");
        assert_eq!(
            bridge_command(Some(exe), Some(&flag), |path| path == Path::new(&beside)),
            [OsString::from(&beside)]
        );
        // `cargo build -p cmdterm` builds no bridge: the app stands in, so
        // sandboxed terminals do not die with `execvp cmdterm-proxy-bridge`.
        assert_eq!(
            bridge_command(Some(exe), Some(&flag), |_| false),
            [exe.as_os_str().to_owned(), flag]
        );
        assert_eq!(
            bridge_command(Some(exe), None, |_| false),
            [OsString::from(format!(
                "cmdterm-proxy-bridge{}",
                std::env::consts::EXE_SUFFIX
            ))]
        );
    }

    #[test]
    fn tokens_go_into_the_url_and_come_back_as_basic_credentials() {
        let token = ProxyToken::generate().unwrap();
        assert_eq!(token.0.len(), 64);
        assert_ne!(token, ProxyToken::generate().unwrap());
        assert_eq!(format!("{token:?}"), "ProxyToken(..)");
        let proxy = SandboxProxy {
            socket: PathBuf::from("/run/p.sock"),
            port: 4321,
            ca_bundle: None,
            token: Some(token.clone()),
        };
        assert_eq!(
            proxy.url(),
            format!("http://cmdterm:{}@127.0.0.1:4321", token.0)
        );
        assert!(!format!("{proxy:?}").contains(&token.0));
        assert_eq!(proxy.clone().without_token().url(), "http://127.0.0.1:4321");
        // What curl sends for that URL.
        assert!(token.accepts(&token.header()));
        assert!(token.accepts(&format!(
            "basic  {}",
            STANDARD.encode(format!("someone:{}", token.0))
        )));
        assert!(!token.accepts(&format!("Bearer {}", token.0)));
        assert!(!token.accepts(&format!("Basic {}", token.0)));
        assert!(!token.accepts(&format!("Basic {}", STANDARD.encode("cmdterm:"))));
        assert!(!token.accepts(&format!(
            "Basic {}",
            STANDARD.encode(format!("cmdterm:{}x", token.0))
        )));
    }

    #[test]
    fn hosts_match_exactly_or_as_subdomains() {
        let allowed = list(&["crates.io", "*.GitHub.com", "[::1]"]);
        assert!(host_allowed(&allowed, "crates.io"));
        assert!(host_allowed(&allowed, "Crates.IO."));
        assert!(!host_allowed(&allowed, "static.crates.io"));
        assert!(!host_allowed(&allowed, "evilcrates.io"));
        assert!(host_allowed(&allowed, "api.github.com"));
        assert!(host_allowed(&allowed, "a.b.github.com"));
        assert!(!host_allowed(&allowed, "github.com"));
        assert!(!host_allowed(&allowed, "evilgithub.com"));
        assert!(host_allowed(&allowed, "::1"));
        assert!(valid_host_pattern("*.example.com"));
        assert!(!valid_host_pattern("*"));
        assert!(!valid_host_pattern("a b"));
        assert!(!valid_host_pattern("http://x/"));
    }
}
