//! The proxy server, and on Unix the bridge inside the sandbox.
//!
//! On Windows the server has its TCP listeners only; the Unix socket of
//! [`SandboxProxy`] stays unused there.
//!
//! Each TCP listener has a [`ProxyToken`], which the first request of every
//! connection to it must carry in `Proxy-Authorization`; see `proxy.rs`.
//! Unix socket connections need none.
//!
//! The bridge listens on the sandbox's loopback and forwards to the proxy's
//! socket (`--port`, `--socket`), and talks to the port hub of the host
//! (`--ports`, see `ports.rs`). Either works without the other.

use std::collections::VecDeque;
#[cfg(unix)]
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use super::approval::Approvals;
use super::blocklist::Blocking;
use super::rules::ProxyRules;
use super::{ProxyToken, SandboxProxy, host_allowed};

mod http;
mod tls;

use http::RequestFilter;

struct Shared {
    allowed: RwLock<Vec<String>>,
    /// Every host is allowed, whatever `allowed` holds.
    any_host: AtomicBool,
    /// Set while the proxy can end TLS itself.
    tls: RwLock<Option<Arc<tls::Interceptor>>>,
    /// With `tls`, every tunnel is intercepted, not only the ones to hosts
    /// the rules inspect.
    intercept_all: AtomicBool,
    rules: RwLock<Arc<ProxyRules>>,
    /// Hosts refused even when allowed.
    blocking: RwLock<Arc<Blocking>>,
    /// Where guards in `Ask` mode ask. Without it they refuse.
    approvals: RwLock<Option<Arc<Approvals>>>,
    /// The latest refused hosts, newest first, for agents to look up.
    refused: RefusalLog,
    stop: AtomicBool,
}

/// A host the proxy refused, see [`NetworkProxy::refusals`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub host: String,
    /// Why, as the refusal's body says it.
    pub reason: String,
    pub at: std::time::SystemTime,
}

/// The refused hosts a proxy remembers, shared with whoever reports them.
#[derive(Clone, Debug, Default)]
pub struct RefusalLog(Arc<Mutex<VecDeque<Refusal>>>);

impl RefusalLog {
    /// Newest first.
    pub fn list(&self) -> Vec<Refusal> {
        (self.0.lock())
            .map(|refused| refused.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// How many refused hosts the proxy remembers.
const REFUSALS_KEPT: usize = 32;

/// Adds `host` to the refused hosts, replacing an older entry for it.
fn remember(shared: &Shared, host: &str, reason: String) {
    if let Ok(mut refused) = shared.refused.0.lock() {
        refused.retain(|r| r.host != host);
        refused.push_front(Refusal {
            host: host.to_owned(),
            reason,
            at: std::time::SystemTime::now(),
        });
        refused.truncate(REFUSALS_KEPT);
    }
}

/// Writes a 403 for `host`, remembers it, and points agents in the app's
/// terminals at the control socket, through which they can suggest a fix.
fn refuse(shared: &Shared, client: &mut impl Write, host: &str, reason: String, fix: &str) {
    remember(shared, host, reason.clone());
    let body = format!(
        "cmdterm sandbox: {reason}\n\
         An agent in a scriptschnell-term terminal can suggest a fix to the user: {fix}\n"
    );
    respond(client, "403 Forbidden", &body);
}

/// A running filtering proxy. Dropping it stops it and removes its sockets.
pub struct NetworkProxy {
    shared: Arc<Shared>,
    endpoint: SandboxProxy,
    /// The endpoints of [`Self::endpoint_for`], by scope.
    scoped: Mutex<Vec<(String, SandboxProxy)>>,
    /// The loopback port and token of the listener without TLS
    /// interception.
    plain: Mutex<Option<(u16, ProxyToken)>>,
}

/// Serves the connections of `tcp` on a thread, with `scope` for the
/// questions their requests raise. Each connection must show `token`.
/// With `plain`, see [`serve`].
fn listen_tcp(
    shared: &Arc<Shared>,
    tcp: TcpListener,
    scope: Option<Arc<str>>,
    token: ProxyToken,
    plain: bool,
) -> io::Result<()> {
    let accept_tcp = Arc::clone(shared);
    let token = Arc::new(token);
    let name = if plain {
        "cmdterm-proxy-plain"
    } else {
        "cmdterm-proxy-tcp"
    };
    thread::Builder::new().name(name.into()).spawn(move || {
        for client in tcp.incoming() {
            if accept_tcp.stop.load(Ordering::Relaxed) {
                break;
            }
            if let Ok(client) = client {
                let shared = Arc::clone(&accept_tcp);
                let scope = scope.clone();
                let token = Arc::clone(&token);
                thread::spawn(move || serve(&shared, client, scope, Some(&token), plain));
            }
        }
    })?;
    Ok(())
}

/// Like [`listen_tcp`] for a Unix socket.
#[cfg(unix)]
fn listen_unix(
    shared: &Arc<Shared>,
    unix: UnixListener,
    scope: Option<Arc<str>>,
) -> io::Result<()> {
    let accept_unix = Arc::clone(shared);
    thread::Builder::new()
        .name("cmdterm-proxy-unix".into())
        .spawn(move || {
            for client in unix.incoming() {
                if accept_unix.stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(client) = client {
                    let shared = Arc::clone(&accept_unix);
                    let scope = scope.clone();
                    thread::spawn(move || serve(&shared, client, scope, None, false));
                }
            }
        })?;
    Ok(())
}

/// Binds the listeners of one endpoint: a Unix socket at `socket` (an old
/// file there is replaced) and a free loopback port, or the port alone on
/// Windows. Returns the port and the token it takes.
fn listen(
    shared: &Arc<Shared>,
    socket: &Path,
    scope: Option<Arc<str>>,
) -> io::Result<(u16, ProxyToken)> {
    #[cfg(unix)]
    let unix = {
        let _ = std::fs::remove_file(socket);
        UnixListener::bind(socket)?
    };
    #[cfg(not(unix))]
    let _ = socket;
    let token = ProxyToken::generate()?;
    let tcp = TcpListener::bind(("127.0.0.1", 0))?;
    let port = tcp.local_addr()?.port();
    #[cfg(unix)]
    listen_unix(shared, unix, scope.clone())?;
    listen_tcp(shared, tcp, scope, token.clone(), false)?;
    Ok((port, token))
}

impl NetworkProxy {
    /// Starts a proxy on a new socket at `socket` (an old one is replaced)
    /// and on a free loopback port.
    pub fn start(socket: &Path, allowed: Vec<String>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            allowed: RwLock::new(allowed),
            any_host: AtomicBool::new(false),
            tls: RwLock::new(None),
            intercept_all: AtomicBool::new(false),
            rules: RwLock::default(),
            blocking: RwLock::default(),
            approvals: RwLock::new(None),
            refused: RefusalLog::default(),
            stop: AtomicBool::new(false),
        });
        let (port, token) = listen(&shared, socket, None)?;
        Ok(Self {
            shared,
            endpoint: SandboxProxy {
                socket: socket.to_path_buf(),
                port,
                ca_bundle: None,
                token: Some(token),
            },
            scoped: Mutex::new(Vec::new()),
            plain: Mutex::new(None),
        })
    }

    /// Like [`Self::endpoint`], but connections to it carry `scope`, such
    /// as a project, into the questions their requests raise, so the user
    /// can allow a guard for good there. The first call for a scope opens
    /// a socket at `socket` and a loopback port of its own.
    pub fn endpoint_for(&self, scope: &str, socket: &Path) -> io::Result<SandboxProxy> {
        let mut scoped = self
            .scoped
            .lock()
            .map_err(|_| io::Error::other("proxy state poisoned"))?;
        let endpoint = match scoped.iter().find(|(s, _)| s == scope) {
            Some((_, endpoint)) => endpoint.clone(),
            None => {
                let (port, token) = listen(&self.shared, socket, Some(Arc::from(scope)))?;
                let endpoint = SandboxProxy {
                    socket: socket.to_path_buf(),
                    port,
                    ca_bundle: None,
                    token: Some(token),
                };
                scoped.push((scope.to_owned(), endpoint.clone()));
                endpoint
            }
        };
        let mut endpoint = endpoint;
        endpoint.ca_bundle = self.endpoint().ca_bundle;
        Ok(endpoint)
    }

    /// A loopback port whose tunnels are never intercepted, for clients
    /// that do not trust the CA of [`Self::intercept_tls`], such as the
    /// debug browser behind a port hub. Allowed hosts and blocklists apply
    /// as everywhere. Hosts the rules inspect are refused on it. Opens the
    /// listener on first use. Returns its port and the token it takes, like
    /// the other TCP ports.
    pub fn plain_port(&self) -> io::Result<(u16, ProxyToken)> {
        let mut plain = self
            .plain
            .lock()
            .map_err(|_| io::Error::other("proxy state poisoned"))?;
        if let Some(plain) = &*plain {
            return Ok(plain.clone());
        }
        let token = ProxyToken::generate()?;
        let tcp = TcpListener::bind(("127.0.0.1", 0))?;
        let port = tcp.local_addr()?.port();
        listen_tcp(&self.shared, tcp, None, token.clone(), true)?;
        *plain = Some((port, token.clone()));
        Ok((port, token))
    }

    /// Replaces the allowed hosts. Open connections keep their upstream.
    pub fn set_allowed(&self, allowed: Vec<String>) {
        if let Ok(mut list) = self.shared.allowed.write() {
            *list = allowed;
        }
    }

    /// Lets every host through, or only the allowed ones again. Sandboxes
    /// with the whole network use the proxy this way, for its rules.
    pub fn allow_any_host(&self, any: bool) {
        self.shared.any_host.store(any, Ordering::Relaxed);
    }

    /// Replaces the secrets and guards. Requests already read keep the
    /// rules they were read with.
    pub fn set_rules(&self, rules: ProxyRules) {
        if let Ok(mut current) = self.shared.rules.write() {
            *current = Arc::new(rules);
        }
    }

    /// Replaces the blocked hosts and lists. Open connections keep their
    /// upstream.
    pub fn set_blocking(&self, blocking: Blocking) {
        if let Ok(mut current) = self.shared.blocking.write() {
            *current = Arc::new(blocking);
        }
    }

    /// The latest refused hosts, newest first: not allowed, blocked, or in
    /// need of HTTPS inspection.
    pub fn refusals(&self) -> RefusalLog {
        self.shared.refused.clone()
    }

    /// Where guards in `Ask` mode put their questions. Without approvals
    /// they refuse.
    pub fn set_approvals(&self, approvals: Option<Arc<Approvals>>) {
        if let Ok(mut current) = self.shared.approvals.write() {
            *current = approvals;
        }
    }

    /// Where sandboxes reach the proxy, with the CA bundle they must trust
    /// while tunnels are intercepted.
    pub fn endpoint(&self) -> SandboxProxy {
        let mut endpoint = self.endpoint.clone();
        if let Ok(tls) = self.shared.tls.read() {
            endpoint.ca_bundle = tls.as_ref().map(|tls| tls.bundle().to_path_buf());
        }
        endpoint
    }

    /// Turns TLS interception on, with the bundle sandboxes trust written to
    /// `bundle`, or off with `None`. See `tls.rs`. With `all_hosts` every
    /// tunnel is intercepted, else only those to hosts the rules inspect.
    /// Tunnels opened before keep their mode. Turning it on again with the
    /// same bundle keeps the CAs of this run.
    ///
    /// While it is off, tunnels to hosts the rules inspect are refused.
    pub fn intercept_tls(&self, bundle: Option<&Path>, all_hosts: bool) -> io::Result<()> {
        self.shared
            .intercept_all
            .store(all_hosts, Ordering::Relaxed);
        let mut tls = self
            .shared
            .tls
            .write()
            .map_err(|_| io::Error::other("proxy state poisoned"))?;
        match bundle {
            None => {
                if let Some(old) = tls.take() {
                    let _ = std::fs::remove_file(old.bundle());
                }
            }
            Some(bundle) if tls.as_ref().is_some_and(|tls| tls.bundle() == bundle) => {}
            Some(bundle) => *tls = Some(Arc::new(tls::Interceptor::new(bundle)?)),
        }
        Ok(())
    }
}

impl Drop for NetworkProxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the accept loops so they see the flag.
        let scoped = self
            .scoped
            .get_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        for endpoint in std::iter::once(&self.endpoint).chain(scoped.iter().map(|(_, e)| e)) {
            #[cfg(unix)]
            let _ = UnixStream::connect(&endpoint.socket);
            let _ = TcpStream::connect(("127.0.0.1", endpoint.port));
            #[cfg(unix)]
            let _ = std::fs::remove_file(&endpoint.socket);
        }
        if let Ok(Some((port, _))) = self.plain.get_mut().map(|p| p.take()) {
            let _ = TcpStream::connect(("127.0.0.1", port));
        }
        let _ = self.intercept_tls(None, false);
    }
}

/// A stream that can be split into a reading and a writing half.
pub(crate) trait Duplex: Read + Write + Send + Sized + 'static {
    fn split(&self) -> io::Result<Self>;
    fn close_write(&self);
    fn set_timeout(&self, timeout: Option<Duration>);
}

impl Duplex for TcpStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn set_timeout(&self, timeout: Option<Duration>) {
        let _ = self.set_read_timeout(timeout);
    }
}

#[cfg(unix)]
impl Duplex for UnixStream {
    fn split(&self) -> io::Result<Self> {
        self.try_clone()
    }
    fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
    fn set_timeout(&self, timeout: Option<Duration>) {
        let _ = self.set_read_timeout(timeout);
    }
}

/// Copies both ways until both sides are done. `early` goes to `b` first.
pub(crate) fn splice<A: Duplex, B: Duplex>(a: A, b: B, early: &[u8]) {
    let (Ok(mut a_read), Ok(mut b_write)) = (a.split(), b.split()) else {
        return;
    };
    let early = early.to_vec();
    let forward = thread::spawn(move || {
        if b_write.write_all(&early).is_ok() {
            let _ = io::copy(&mut a_read, &mut b_write);
        }
        b_write.close_write();
    });
    let (mut a, mut b) = (a, b);
    let _ = io::copy(&mut b, &mut a);
    a.close_write();
    let _ = forward.join();
}

/// [`splice`] with `filter` between the client `a` and the upstream `b`.
fn splice_filtered<A: Duplex, B: Duplex>(a: A, b: B, mut filter: RequestFilter) {
    let (Ok(mut a_read), Ok(mut a_reply), Ok(mut b_write)) = (a.split(), a.split(), b.split())
    else {
        return;
    };
    let forward = thread::spawn(move || {
        let mut chunk = [0u8; 16 * 1024];
        let mut out = Vec::new();
        loop {
            let n = match a_read.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            out.clear();
            let fed = filter.feed(&chunk[..n], &mut out);
            if b_write.write_all(&out).is_err() {
                break;
            }
            if let Err(response) = fed {
                let _ = a_reply.write_all(&response);
                a_reply.close_write();
                linger(&mut a_read);
                break;
            }
        }
        b_write.close_write();
    });
    let (mut a, mut b) = (a, b);
    let _ = io::copy(&mut b, &mut a);
    a.close_write();
    let _ = forward.join();
}

/// How long a refused client may keep sending after its refusal.
const LINGER: Duration = Duration::from_secs(5);

/// Reads and drops what the client still sends, until it closes or
/// [`LINGER`] passes. Closing a socket with unread input resets the
/// connection, and a reset can destroy the refusal before the client
/// reads it.
fn linger<S: Duplex>(client: &mut S) {
    client.set_timeout(Some(LINGER));
    let deadline = std::time::Instant::now() + LINGER;
    let mut sink = [0u8; 4096];
    while std::time::Instant::now() < deadline {
        match client.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

const MAX_HEAD: usize = 64 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads up to the end of the request head. Returns the head and the bytes
/// read after it.
pub(crate) fn read_head(client: &mut impl Read) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = from + end + 4;
            let rest = buf.split_off(end);
            return Some((buf, rest));
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
    }
}

/// `host[:port]` or `[v6][:port]` with `default` as the port.
pub(crate) fn split_authority(authority: &str, default: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port = match port {
        Some(port) => port.parse().ok()?,
        None => default,
    };
    (!host.is_empty()).then(|| (host.to_owned(), port))
}

pub(crate) fn respond(client: &mut impl Write, status: &str, body: &str) {
    let _ = write!(
        client,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Connects to `host:port`. Addresses on this machine's loopback, link-local
/// ones such as a cloud's metadata service, and unspecified ones are only
/// tried when `local` is true; see [`may_reach_local`].
pub(crate) fn connect(host: &str, port: u16, local: bool) -> io::Result<TcpStream> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no address");
    for addr in (host, port).to_socket_addrs()? {
        if !local && is_local_address(addr.ip()) {
            last = io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is on this machine or link-local; only allowed hosts that name such an \
                     address themselves, such as localhost, reach it",
                    addr.ip()
                ),
            );
            continue;
        }
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// True for loopback, link-local and unspecified addresses, also as IPv4
/// mapped into IPv6.
fn is_local_address(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// True for addresses that plain HTTP reaches without crossing the
/// internet: this machine, private IPv4 ranges, the shared range
/// 100.64.0.0/10 that Tailscale and NetBird give their peers, IPv6 unique
/// local and link-local addresses. VPN peers are encrypted on the way.
fn is_nearby(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    is_local_address(ip)
        || match ip {
            IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                v4.is_private() || (a == 100 && (64..128).contains(&b))
            }
            IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
        }
}

/// True when the proxy may connect `host` to an address on this machine or
/// a link-local one: `host` names such an address itself (`localhost`, a
/// loopback or link-local literal) and the allowed hosts list it exactly.
/// Allowing every host does not cover it, and neither does a name that
/// only resolves to such an address, so a sandbox cannot reach this
/// machine's servers through DNS it controls.
fn may_reach_local(shared: &Shared, host: &str) -> bool {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    let names_local = name == "localhost"
        || name.ends_with(".localhost")
        || name.parse::<IpAddr>().is_ok_and(is_local_address);
    names_local
        && shared.allowed.read().is_ok_and(|list| {
            list.iter().any(|pattern| {
                !pattern.starts_with("*.") && host_allowed(std::slice::from_ref(pattern), host)
            })
        })
}

/// Writes a `407` that asks for the token of the proxy URL, and lets the
/// client read it. Clients such as git first try without credentials and
/// send them only after this.
pub(crate) fn unauthorized(client: &mut impl Duplex) {
    let body = "cmdterm sandbox: this proxy port takes the password of its proxy URL, \
                such as the sandbox's HTTP_PROXY\n";
    let _ = write!(
        client,
        "HTTP/1.1 407 Proxy Authentication Required\r\n\
         Proxy-Authenticate: Basic realm=\"cmdterm sandbox\"\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.close_write();
    linger(client);
}

/// True when one of the header lines of a request head (after its first
/// line) is a `Proxy-Authorization` that `token` accepts.
pub(crate) fn authorized<'a>(
    token: &ProxyToken,
    mut headers: impl Iterator<Item = &'a str>,
) -> bool {
    headers.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("proxy-authorization") && token.accepts(value)
        })
    })
}

/// Serves one connection. With `token`, its first request must carry it
/// (see `proxy.rs`). Without it the connection gets a `407` before the
/// proxy looks at the request, and the refusal log stays as it was.
///
/// With `plain`, tunnels are never intercepted, so a client that does not
/// trust the proxy's CA (the debug browser) sees the real certificates.
/// Tunnels to hosts the rules inspect are refused then, as they need
/// interception.
fn serve<S: Duplex>(
    shared: &Arc<Shared>,
    mut client: S,
    scope: Option<Arc<str>>,
    token: Option<&ProxyToken>,
    plain: bool,
) {
    client.set_timeout(Some(HEAD_TIMEOUT));
    let Some((head, rest)) = read_head(&mut client) else {
        return;
    };
    client.set_timeout(None);
    let Ok(text) = std::str::from_utf8(&head) else {
        return respond(&mut client, "400 Bad Request", "bad request head\n");
    };
    if let Some(token) = token
        && !authorized(token, text.split("\r\n").skip(1))
    {
        return unauthorized(&mut client);
    }
    let mut first = text.split("\r\n").next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(_)) = (first.next(), first.next(), first.next()) else {
        return respond(&mut client, "400 Bad Request", "bad request line\n");
    };
    let connect_method = method.eq_ignore_ascii_case("CONNECT");
    let authority = if connect_method {
        target
    } else {
        let Some(rest) = target
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
            .map(|_| &target[7..])
        else {
            return respond(
                &mut client,
                "400 Bad Request",
                "cmdterm proxy: use CONNECT or an http:// URL\n",
            );
        };
        rest.find(['/', '?']).map_or(rest, |at| &rest[..at])
    };
    let default_port = if connect_method { 443 } else { 80 };
    let Some((host, port)) = split_authority(authority, default_port) else {
        return respond(&mut client, "400 Bad Request", "bad host\n");
    };
    if let Ok(blocking) = shared.blocking.read()
        && let Some(by) = blocking.blocked_by(&host)
    {
        let reason = format!("{host} is blocked by {by}");
        let fix = "sh ~/.local/share/cmdterm/control sandbox shows what blocks it";
        return refuse(shared, &mut client, &host, reason, fix);
    }
    let allowed = shared.any_host.load(Ordering::Relaxed)
        || shared
            .allowed
            .read()
            .is_ok_and(|list| host_allowed(&list, &host));
    if !allowed {
        let reason = format!("{host} is not an allowed host");
        let fix = format!("sh ~/.local/share/cmdterm/control allow-host {host} 'why it is needed'");
        return refuse(shared, &mut client, &host, reason, &fix);
    }
    let inspect = shared.rules.read().is_ok_and(|rules| rules.inspects(&host));
    let interceptor = if plain {
        None
    } else {
        shared.tls.read().ok().and_then(|tls| tls.clone())
    };
    // Without interception the guards and secrets of the host cannot apply.
    if connect_method && inspect && interceptor.is_none() {
        let reason = if plain {
            format!("{host} needs HTTPS inspection, which the debug browser cannot take part in")
        } else {
            format!("{host} needs HTTPS inspection, which is off")
        };
        let fix = if plain {
            "the debug browser cannot trust the interception CA; use curl in the sandbox"
        } else {
            "suggest sandbox.intercept_tls = true with control suggest"
        };
        return refuse(shared, &mut client, &host, reason, fix);
    }
    let mut upstream = match connect(&host, port, may_reach_local(shared, &host)) {
        Ok(upstream) => upstream,
        Err(error) => {
            let body = format!("cmdterm sandbox: {host}:{port}: {error}\n");
            return respond(&mut client, "502 Bad Gateway", &body);
        }
    };
    // A plain request is read before anything goes upstream, so a refused
    // one never reaches it, and so are the ones after it on the
    // connection, so `Proxy-Authorization` never reaches the server. Over
    // plain HTTP anyone on the way could read a secret, so only a server on
    // this machine or the local network gets the real one. The plain port
    // serves the debug browser behind the port hub, which takes requests
    // from any local process, so it reveals none.
    let mut filtered = None;
    if !connect_method {
        let mut filter = RequestFilter::new(Arc::clone(shared), scope.clone(), &host);
        if plain || !upstream.peer_addr().is_ok_and(|peer| is_nearby(peer.ip())) {
            filter = filter.without_secrets();
        }
        let mut out = Vec::new();
        let fed = filter
            .feed(&head, &mut out)
            .and_then(|()| filter.feed(&rest, &mut out));
        if let Err(response) = fed {
            drop(upstream);
            let _ = client.write_all(&response);
            client.close_write();
            linger(&mut client);
            return;
        }
        filtered = Some((filter, out));
    }
    if connect_method {
        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .is_err()
        {
            return;
        }
        let served = match interceptor {
            Some(interceptor) if inspect => {
                let filter = RequestFilter::new(Arc::clone(shared), scope, &host);
                interceptor.serve(client, &rest, &host, upstream, Some(filter))
            }
            Some(interceptor) if shared.intercept_all.load(Ordering::Relaxed) => {
                interceptor.serve(client, &rest, &host, upstream, None)
            }
            _ => return splice(client, upstream, &rest),
        };
        // The client got no 403 to read, so the log is the only place that
        // says why its TLS failed.
        if served == tls::Served::Distrusted {
            let reason = format!(
                "a program turned down the proxy's certificate for {host}: \
                 it ignores the sandbox's CA bundle variables"
            );
            remember(shared, &host, reason);
        }
        return;
    }
    if let Some((filter, out)) = filtered
        && upstream.write_all(&out).is_ok()
    {
        splice_filtered(client, upstream, filter);
    }
}

#[cfg(unix)]
/// The `cmdterm-proxy-bridge` program: `[--scope] [--port PORT --socket
/// PATH] [--ports PATH] -- PROGRAM [ARG]...`. With `--scope` it first
/// keeps itself and the program from the abstract Unix sockets and the
/// processes outside the sandbox (see `landlock.rs`), or exits when it
/// cannot. `--try-scope` does the same where the kernel has Landlock ABI
/// 6 and nothing elsewhere. With `--port` and `--socket` it listens on loopback `PORT` and
/// forwards every connection to the Unix socket `PATH`. With `--ports` it
/// joins the port hub at `PATH` (see `ports.rs`). It runs the program and
/// exits with its status; with nothing to forward it becomes the program.
pub fn proxy_bridge_main(args: impl IntoIterator<Item = OsString>) -> i32 {
    let mut args = args.into_iter();
    let mut port = None;
    let mut socket = None;
    let mut ports = None;
    let mut scope = false;
    loop {
        let Some(flag) = args.next() else {
            eprintln!("cmdterm-proxy-bridge: missing `-- PROGRAM`");
            return 125;
        };
        if flag == "--" {
            break;
        }
        if flag == "--scope" || flag == "--try-scope" {
            // `--try-scope` asks only where the kernel can, as on a remote
            // machine, whose kernel the client does not know.
            scope = flag == "--scope" || crate::landlock::landlock_scopes();
            continue;
        }
        let value = args.next();
        match (flag.to_str(), value) {
            (Some("--port"), Some(value)) => port = value.to_str().and_then(|v| v.parse().ok()),
            (Some("--socket"), Some(value)) => socket = Some(PathBuf::from(value)),
            (Some("--ports"), Some(value)) => ports = Some(PathBuf::from(value)),
            _ => {
                eprintln!("cmdterm-proxy-bridge: bad flag {}", flag.to_string_lossy());
                return 125;
            }
        }
    }
    let Some(program) = args.next() else {
        eprintln!("cmdterm-proxy-bridge: needs PROGRAM");
        return 125;
    };
    if port.is_some() != socket.is_some() {
        eprintln!("cmdterm-proxy-bridge: --port and --socket go together");
        return 125;
    }
    // Before any thread starts, since a restriction covers only the
    // calling thread and what it starts later.
    if scope && let Err(error) = crate::landlock::scope() {
        eprintln!("cmdterm-proxy-bridge: Landlock scope: {error}");
        return 125;
    }
    if port.is_none() && ports.is_none() {
        use std::os::unix::process::CommandExt;
        let error = std::process::Command::new(&program).args(args).exec();
        eprintln!(
            "cmdterm-proxy-bridge: {}: {error}",
            program.to_string_lossy()
        );
        return 127;
    }
    if let (Some(port), Some(socket)) = (port, socket) {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("cmdterm-proxy-bridge: 127.0.0.1:{port}: {error}");
                return 125;
            }
        };
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let socket = socket.clone();
                thread::spawn(move || {
                    if let Ok(proxy) = UnixStream::connect(&socket) {
                        splice(client, proxy, &[]);
                    }
                });
            }
        });
    }
    if let Some(hub) = ports
        && let Err(error) = crate::ports::run_bridge(&hub, port.into_iter().collect())
    {
        eprintln!(
            "cmdterm-proxy-bridge: {}: {error}; running without port forwarding",
            hub.display()
        );
    }
    let status = match std::process::Command::new(&program).args(args).status() {
        Ok(status) => status,
        Err(error) => {
            eprintln!(
                "cmdterm-proxy-bridge: {}: {error}",
                program.to_string_lossy()
            );
            return 127;
        }
    };
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn authorities_split_into_host_and_port() {
        assert_eq!(split_authority("a.io", 80), Some(("a.io".into(), 80)));
        assert_eq!(
            split_authority("a.io:8080", 80),
            Some(("a.io".into(), 8080))
        );
        assert_eq!(split_authority("[::1]:443", 80), Some(("::1".into(), 443)));
        assert_eq!(split_authority("[::1]", 80), Some(("::1".into(), 80)));
        assert_eq!(split_authority(":1", 80), None);
        assert_eq!(split_authority("a.io:x", 80), None);
    }

    /// An HTTP server on loopback that answers with the request line it got.
    fn echo_server() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let Some((head, _)) = read_head(&mut stream) else {
                    continue;
                };
                let head = String::from_utf8_lossy(&head);
                respond(&mut stream, "200 OK", head.lines().next().unwrap());
            }
        });
        port
    }

    fn ask(mut stream: impl Read + Write, request: &str) -> String {
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply
    }

    /// A TCP connection to a proxy port that puts the port's token after
    /// the request line of its first write, as curl does with the password
    /// of the proxy URL.
    struct Authed(TcpStream, Option<String>);

    fn authed(port: u16, token: &ProxyToken) -> Authed {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        Authed(stream, Some(token.header()))
    }

    /// [`authed`] for an endpoint's TCP port.
    fn tcp_to(endpoint: &SandboxProxy) -> Authed {
        authed(endpoint.port, endpoint.token.as_ref().unwrap())
    }

    impl Read for Authed {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Write for Authed {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let Some(header) = self.1.take() else {
                return self.0.write(buf);
            };
            let at = buf.windows(2).position(|w| w == b"\r\n").unwrap() + 2;
            let mut out = buf[..at].to_vec();
            out.extend_from_slice(format!("Proxy-Authorization: {header}\r\n").as_bytes());
            out.extend_from_slice(&buf[at..]);
            self.0.write_all(&out)?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    /// An HTTP server on loopback that keeps every request head it gets,
    /// any number per connection, and answers each with its request line.
    /// It closes the connection after a request with `Connection: close`.
    fn recording_server() -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&heads);
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let kept = Arc::clone(&kept);
                thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head: Vec<u8> = buf.drain(..end + 4).collect();
                            let head = String::from_utf8_lossy(&head).into_owned();
                            let line = head.lines().next().unwrap_or_default().to_owned();
                            let close = head.to_ascii_lowercase().contains("connection: close");
                            kept.lock().unwrap().push(head);
                            let _ = write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{line}",
                                line.len()
                            );
                            if close {
                                return;
                            }
                        }
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                });
            }
        });
        (port, heads)
    }

    #[test]
    fn tcp_connections_need_the_token_and_it_never_goes_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let (server, heads) = recording_server();
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        proxy.allow_any_host(true);
        let endpoint = proxy.endpoint();
        let token = endpoint.token.clone().unwrap();
        let (plain, plain_token) = proxy.plain_port().unwrap();
        assert_ne!(plain_token, token);
        let scoped = proxy
            .endpoint_for("p1", &dir.path().join("p1.sock"))
            .unwrap();
        let scoped_token = scoped.token.clone().unwrap();
        assert_ne!(scoped_token, token);
        let tcp = |port: u16| TcpStream::connect(("127.0.0.1", port)).unwrap();
        let get = format!("GET http://localhost:{server}/a HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let tunnel = format!(
            "CONNECT localhost:{server} HTTP/1.1\r\n\r\nGET /t HTTP/1.1\r\nConnection: close\r\n\r\n"
        );
        let with_token = |token: &ProxyToken, request: &str| {
            let (line, rest) = request.split_once("\r\n").unwrap();
            let header = format!(
                "Basic {}",
                base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    format!("cmdterm:{}", token.0),
                )
            );
            format!("{line}\r\nProxy-Authorization: {header}\r\n{rest}")
        };

        // Without the token, or with another port's, nothing goes upstream
        // and nothing is remembered.
        for port in [endpoint.port, plain, scoped.port] {
            for request in [&get, &tunnel] {
                let reply = ask(tcp(port), request);
                assert!(reply.starts_with("HTTP/1.1 407"), "{reply}");
                assert!(
                    reply.contains("Proxy-Authenticate: Basic realm=\"cmdterm sandbox\""),
                    "{reply}"
                );
            }
        }
        let reply = ask(tcp(endpoint.port), &with_token(&scoped_token, &get));
        assert!(reply.starts_with("HTTP/1.1 407"), "{reply}");
        let reply = ask(tcp(scoped.port), &with_token(&token, &tunnel));
        assert!(reply.starts_with("HTTP/1.1 407"), "{reply}");
        assert!(heads.lock().unwrap().is_empty());
        assert!(proxy.refusals().list().is_empty());

        // With it, a tunnel and plain requests work, also a second request
        // on the connection, which carries the token again.
        let reply = ask(tcp_to(&endpoint), &tunnel);
        assert!(reply.ends_with("GET /t HTTP/1.1"), "{reply}");
        let reply = ask(authed(plain, &plain_token), &tunnel);
        assert!(reply.ends_with("GET /t HTTP/1.1"), "{reply}");
        let mut stream = tcp_to(&scoped);
        let again = with_token(&scoped_token, &get.replace("/a", "/b"));
        stream
            .write_all(format!("{get}{again}").as_bytes())
            .unwrap();
        stream.0.shutdown(Shutdown::Write).unwrap();
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        assert!(reply.contains("GET /a HTTP/1.1"), "{reply}");
        assert!(reply.ends_with("GET /b HTTP/1.1"), "{reply}");

        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 4, "{heads:?}");
        for head in heads.iter() {
            let lower = head.to_ascii_lowercase();
            assert!(!lower.contains("proxy-authorization"), "{head}");
            for token in [&token, &plain_token, &scoped_token] {
                assert!(!head.contains(&token.0), "{head}");
            }
        }
    }

    #[test]
    fn the_proxy_forwards_allowed_hosts_and_refuses_others() {
        let dir = tempfile::tempdir().unwrap();
        let server = echo_server();
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        let tcp = || tcp_to(&proxy.endpoint());

        let reply = ask(
            tcp(),
            &format!(
                "GET http://localhost:{server}/a?b HTTP/1.1\r\nHost: localhost:{server}\r\n\
                 Proxy-Connection: keep-alive\r\n\r\n"
            ),
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.ends_with("GET /a?b HTTP/1.1"), "{reply}");

        let reply = ask(
            tcp(),
            &format!("GET http://127.0.0.1:{server}/ HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(reply.contains("control allow-host"), "{reply}");
        assert!(
            !proxy.refusals().list().is_empty(),
            "the refusal is remembered"
        );
        assert!(
            reply.contains("127.0.0.1 is not an allowed host"),
            "{reply}"
        );

        // A tunnel carries whatever the client sends after the 200.
        let reply = ask(
            UnixStream::connect(dir.path().join("p.sock")).unwrap(),
            &format!("CONNECT localhost:{server} HTTP/1.1\r\n\r\nPUT /t HTTP/1.1\r\n\r\n"),
        );
        assert!(
            reply.starts_with("HTTP/1.1 200 Connection Established"),
            "{reply}"
        );
        assert!(reply.ends_with("PUT /t HTTP/1.1"), "{reply}");

        proxy.set_allowed(Vec::new());
        let reply = ask(
            tcp(),
            &format!("CONNECT localhost:{server} HTTP/1.1\r\n\r\n"),
        );
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");

        let socket = proxy.endpoint().socket.clone();
        drop(proxy);
        assert!(!socket.exists());
    }

    #[test]
    fn the_plain_port_reveals_no_secret() {
        use crate::proxy::rules::SecretRule;
        let dir = tempfile::tempdir().unwrap();
        let (server, heads) = recording_server();
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        proxy.set_rules(ProxyRules {
            secrets: vec![SecretRule {
                fake: "FAKE".into(),
                real: "REAL".into(),
                hosts: vec!["localhost".into()],
            }],
            guards: Vec::new(),
        });
        let get = format!(
            "GET http://localhost:{server}/ HTTP/1.1\r\nAuthorization: FAKE\r\n\
             Connection: close\r\n\r\n"
        );
        let (port, token) = proxy.plain_port().unwrap();
        assert!(ask(authed(port, &token), &get).starts_with("HTTP/1.1 200"));
        assert!(ask(tcp_to(&proxy.endpoint()), &get).starts_with("HTTP/1.1 200"));
        let heads = heads.lock().unwrap();
        assert!(heads[0].contains("Authorization: FAKE"), "{heads:?}");
        assert!(heads[1].contains("Authorization: REAL"), "{heads:?}");
    }

    #[test]
    fn plain_http_secrets_stay_on_this_machine_or_the_local_network() {
        for nearby in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "100.101.102.103",
            "::1",
            "fd00::1",
            "fd7a:115c:a1e0::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_nearby(nearby.parse().unwrap()), "{nearby}");
        }
        for far in [
            "1.1.1.1",
            "140.82.112.3",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_nearby(far.parse().unwrap()), "{far}");
        }
    }

    #[test]
    fn this_machines_loopback_takes_an_allowed_local_name() {
        let dir = tempfile::tempdir().unwrap();
        let server = echo_server();
        let proxy = NetworkProxy::start(&dir.path().join("p.sock"), Vec::new()).unwrap();
        proxy.allow_any_host(true);
        let tcp = || tcp_to(&proxy.endpoint());
        for host in ["localhost", "127.0.0.1", "127.0.0.2", "[::ffff:127.0.0.1]"] {
            let get = format!("GET http://{host}:{server}/ HTTP/1.1\r\n\r\n");
            let reply = ask(tcp(), &get);
            assert!(reply.starts_with("HTTP/1.1 502"), "{host}: {reply}");
        }
        // A name that is not local never reaches a local address, even
        // when it is allowed.
        proxy.set_allowed(vec!["localtest.me".into(), "*.localhost".into()]);
        let tunnel = format!("CONNECT x.localhost:{server} HTTP/1.1\r\n\r\n");
        assert!(ask(tcp(), &tunnel).starts_with("HTTP/1.1 502"));
        proxy.set_allowed(vec!["127.0.0.1".into()]);
        let get = format!("GET http://127.0.0.1:{server}/ HTTP/1.1\r\n\r\n");
        assert!(ask(tcp(), &get).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn blocked_hosts_are_refused_even_with_the_whole_network() {
        use crate::proxy::blocklist::Blocklist;
        let dir = tempfile::tempdir().unwrap();
        let server = echo_server();
        // Every host, and `localhost` by name, since allowing every host
        // does not cover this machine's loopback.
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        proxy.allow_any_host(true);
        let tcp = || tcp_to(&proxy.endpoint());
        let get = format!("GET http://localhost:{server}/ HTTP/1.1\r\n\r\n");
        assert!(ask(tcp(), &get).starts_with("HTTP/1.1 200"));
        proxy.set_blocking(Blocking {
            lists: vec![Arc::new(Blocklist::parse(
                "ads",
                "0.0.0.0 localhost.\nlocalhost",
            ))],
            ..Blocking::default()
        });
        // Hosts files map `localhost` without blocking it.
        assert!(ask(tcp(), &get).starts_with("HTTP/1.1 200"));
        proxy.set_blocking(Blocking {
            hosts: vec!["localhost".into()],
            ..Blocking::default()
        });
        let reply = ask(
            tcp(),
            &format!("CONNECT localhost:{server} HTTP/1.1\r\n\r\n"),
        );
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(
            reply.contains("localhost is blocked by localhost"),
            "{reply}"
        );
        proxy.set_blocking(Blocking {
            hosts: vec!["localhost".into()],
            exceptions: vec!["localhost".into()],
            ..Blocking::default()
        });
        assert!(ask(tcp(), &get).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn the_plain_port_never_intercepts_and_refuses_inspected_hosts() {
        use crate::proxy::rules::{GuardMode, GuardRule};
        let dir = tempfile::tempdir().unwrap();
        let server = echo_server();
        let proxy =
            NetworkProxy::start(&dir.path().join("p.sock"), vec!["localhost".into()]).unwrap();
        proxy
            .intercept_tls(Some(&dir.path().join("ca.pem")), true)
            .unwrap();
        let (port, token) = proxy.plain_port().unwrap();
        assert_eq!(proxy.plain_port().unwrap(), (port, token.clone()));
        let tunnel = format!("CONNECT localhost:{server} HTTP/1.1\r\n\r\nGET /x HTTP/1.1\r\n\r\n");
        let reply = ask(authed(port, &token), &tunnel);
        assert!(reply.contains("200 Connection Established"), "{reply}");
        assert!(reply.ends_with("GET /x HTTP/1.1"), "{reply}");
        // Other hosts stay refused.
        let other = ask(
            authed(port, &token),
            "CONNECT evil.org:443 HTTP/1.1\r\n\r\n",
        );
        assert!(other.starts_with("HTTP/1.1 403"), "{other}");
        proxy.set_rules(ProxyRules {
            secrets: Vec::new(),
            guards: vec![GuardRule {
                id: "t.push".into(),
                title: "push".into(),
                hosts: vec!["localhost".into()],
                requests: vec!["POST *".into()],
                mode: GuardMode::Ask,
                kind: None,
            }],
        });
        let inspected = ask(authed(port, &token), &tunnel);
        assert!(inspected.starts_with("HTTP/1.1 403"), "{inspected}");
    }

    #[test]
    fn plain_requests_to_inspected_hosts_are_filtered() {
        use crate::proxy::rules::{GuardMode, GuardRule, SecretRule};
        let dir = tempfile::tempdir().unwrap();
        let server = echo_server();
        let proxy = NetworkProxy::start(&dir.path().join("p.sock"), Vec::new()).unwrap();
        let tcp = || tcp_to(&proxy.endpoint());
        let get = format!(
            "GET http://localhost:{server}/?k=FAKE HTTP/1.1\r\nAuthorization: FAKE\r\n\r\n"
        );
        assert!(ask(tcp(), &get).starts_with("HTTP/1.1 403"));
        proxy.set_allowed(vec!["localhost".into()]);
        proxy.set_rules(ProxyRules {
            secrets: vec![SecretRule {
                fake: "FAKE".into(),
                real: "REAL".into(),
                hosts: vec!["localhost".into()],
            }],
            guards: vec![GuardRule {
                id: "t.push".into(),
                title: "push".into(),
                hosts: vec!["*".into()],
                requests: vec!["POST *".into()],
                mode: GuardMode::Ask,
                kind: None,
            }],
        });
        // The target keeps its fake.
        let reply = ask(tcp(), &get);
        assert!(reply.ends_with("GET /?k=FAKE HTTP/1.1"), "{reply}");
        // Nobody to ask: refused.
        let post = format!("POST http://localhost:{server}/ HTTP/1.1\r\n\r\n");
        assert!(ask(tcp(), &post).starts_with("HTTP/1.1 403"));
        let approvals = Arc::new(Approvals::new(|| {}));
        proxy.set_approvals(Some(Arc::clone(&approvals)));
        let asking = thread::spawn({
            let endpoint = proxy.endpoint();
            let post = post.clone();
            move || ask(tcp_to(&endpoint), &post)
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pending = loop {
            if let Some(pending) = approvals.pending().pop() {
                break pending;
            }
            assert!(std::time::Instant::now() < deadline, "no question came");
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            (pending.method.as_str(), pending.host.as_str()),
            ("POST", "localhost")
        );
        assert_eq!(pending.scope, None);
        approvals.answer(pending.id, true);
        let reply = asking.join().unwrap();
        assert!(reply.ends_with("POST / HTTP/1.1"), "{reply}");

        // A project's endpoint names the project, so a standing answer there
        // lets the request through without a question.
        let scoped = proxy
            .endpoint_for("p1", &dir.path().join("p1.sock"))
            .unwrap();
        assert_ne!(scoped.port, proxy.endpoint().port);
        approvals.set_always("p1", vec![("t.push".into(), "localhost".into())]);
        let reply = ask(tcp_to(&scoped), &post);
        assert!(reply.ends_with("POST / HTTP/1.1"), "{reply}");
        let unix = UnixStream::connect(&scoped.socket).unwrap();
        assert!(ask(unix, &post).ends_with("POST / HTTP/1.1"));
        assert!(approvals.pending().is_empty());
        assert_eq!(
            proxy.endpoint_for("p1", Path::new("/unused")).unwrap(),
            scoped
        );
    }
}
