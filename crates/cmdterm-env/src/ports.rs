//! Forwarding of loopback ports between isolated sandboxes and the host.
//!
//! An isolated sandbox (see `sandbox.rs`) has a loopback of its own, so a
//! server it starts binds no host port. A [`PortHub`] is the host's end of
//! the forwarding for one scope, such as a project. It listens on a Unix
//! socket that the sandboxes of the scope get at a fixed path, and
//! `cmdterm-proxy-bridge --ports` (the bridge) inside each sandbox talks to
//! it. The protocol is line based, ASCII, and every line ends in `\n`.
//!
//! The bridge opens one control connection and sends `HELLO <sandbox-id>`.
//! Then:
//!
//! - bridge to hub: `LISTEN <port>` and `CLOSED <port>` for the TCP ports
//!   that listen on 127.0.0.1, ::1 or every address inside the sandbox. The
//!   bridge reads `/proc/net/tcp` and `/proc/net/tcp6` every 500 ms
//!   ([`listening_ports`]) and leaves out the ports it listens on itself.
//! - hub to bridge: `MIRROR <port>` and `UNMIRROR <port>`. The bridge listens
//!   on 127.0.0.1:`port` inside its sandbox and sends each connection to the
//!   sandbox that owns the port, so the sandboxes of a scope see each
//!   other's servers as if they shared a loopback. A failed bind is ignored
//!   and tried again on the next `MIRROR`.
//! - hub to bridge: `OPEN <conn-id> <port>`. The bridge opens a new
//!   connection to the hub, sends `DATA <conn-id>`, dials the port inside
//!   (127.0.0.1, then ::1) and splices. It closes the connection if the dial
//!   fails.
//!
//! A connection that a mirror accepted becomes a new hub connection that
//! starts with `DIAL <port>`. The hub picks the owner, sends it `OPEN` and
//! splices the two data connections. After their first line, data
//! connections carry raw bytes. When the control connection closes, the hub
//! forgets the ports of the sandbox and unmirrors them elsewhere.
//!
//! A hub listens on a loopback TCP port only after [`PortHub::start_tcp`]
//! and then speaks the same protocol there. Every TCP connection must start
//! with `AUTH <token>`, a random token that `start_tcp` returns, because other
//! local users and processes can reach loopback. Only the runtime of a
//! Windows project calls it. The Windows launcher uses it
//! (`--ports-tcp PORT:TOKEN`), because an AppContainer cannot use the Unix
//! socket and Windows has none for the hub. That bridge runs inside the
//! launcher, outside the container, finds the listeners of the job's
//! processes in the TCP tables, and ignores `MIRROR` and `UNMIRROR`: the
//! sandboxes share the host's loopback, so a mirror would bind host ports.
//!
//! The host reaches the ports too: [`PortHub::expose`] listens on a host
//! port and forwards to a sandbox port, [`PortHub::connect`] opens one
//! stream, and [`PortHub::http_proxy`] is a loopback HTTP proxy that sends
//! requests for `localhost` into the sandbox ports and every other host to
//! an upstream proxy (the sandbox's own filtering proxy, with the token its
//! port takes), or refuses them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crate::proxy::{
    Duplex, ProxyToken, authorized, connect, read_head, respond, splice, split_authority,
    unauthorized,
};

/// A connection of the protocol: a Unix socket on Unix, or TCP on the
/// hub's loopback port, which AppContainers on Windows use.
#[derive(Debug)]
pub enum Stream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(match self {
            #[cfg(unix)]
            Self::Unix(s) => Self::Unix(s.try_clone()?),
            Self::Tcp(s) => Self::Tcp(s.try_clone()?),
        })
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.shutdown(how),
            Self::Tcp(s) => s.shutdown(how),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.set_read_timeout(timeout),
            Self::Tcp(s) => s.set_read_timeout(timeout),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.set_write_timeout(timeout),
            Self::Tcp(s) => s.set_write_timeout(timeout),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
            Self::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

impl Write for &Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Stream::Unix(s) => (&*s).write(buf),
            Stream::Tcp(s) => (&*s).write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Duplex for Stream {
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

/// Where a bridge finds its hub.
#[derive(Clone, Debug)]
pub(crate) enum Hub {
    #[cfg(unix)]
    Unix(PathBuf),
    /// The address and the token of a hub's TCP listener.
    Tcp(SocketAddr, String),
}

impl Hub {
    fn connect(&self) -> io::Result<Stream> {
        Ok(match self {
            #[cfg(unix)]
            Self::Unix(path) => Stream::Unix(UnixStream::connect(path)?),
            Self::Tcp(addr, token) => {
                let mut stream = Stream::Tcp(TcpStream::connect(addr)?);
                writeln!(stream, "AUTH {token}")?;
                stream
            }
        })
    }
}

/// Reports the TCP ports that listen inside the sandbox.
pub(crate) type Listeners = Box<dyn Fn() -> BTreeSet<u16> + Send + Sync>;

/// How long the hub waits for a bridge to answer an `OPEN`.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a first line or a request head may take.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the bridge reads the listening ports.
const POLL: Duration = Duration::from_millis(500);

/// A TCP port that something listens on inside a sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListeningPort {
    pub port: u16,
    /// The host port that forwards to it, while it is exposed.
    pub exposed: Option<u16>,
}

struct Sandbox {
    id: u64,
    control: Stream,
    ports: BTreeSet<u16>,
    /// Ports the hub told the bridge to mirror.
    mirrored: BTreeSet<u16>,
}

struct Exposure {
    host_port: u16,
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    sandboxes: Vec<Sandbox>,
    exposed: BTreeMap<u16, Exposure>,
    pending: HashMap<u64, Sender<Stream>>,
    next: u64,
    proxy: Option<(u16, ProxyToken)>,
    upstream: Option<(SocketAddr, ProxyToken)>,
}

struct Inner {
    /// The Unix socket, where there is one.
    socket: Option<PathBuf>,
    /// The loopback TCP port and token of the hub, once started.
    tcp: Mutex<Option<(u16, String)>>,
    state: Mutex<State>,
    revision: AtomicU64,
    stop: AtomicBool,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Tells every bridge which ports of the others to mirror.
    fn sync_mirrors(state: &mut State) {
        let all: BTreeSet<u16> = state
            .sandboxes
            .iter()
            .flat_map(|s| s.ports.iter().copied())
            .collect();
        for sandbox in &mut state.sandboxes {
            let wanted: BTreeSet<u16> = all.difference(&sandbox.ports).copied().collect();
            let add: Vec<u16> = wanted.difference(&sandbox.mirrored).copied().collect();
            let remove: Vec<u16> = sandbox.mirrored.difference(&wanted).copied().collect();
            for port in add {
                if !tell(&sandbox.control, &format!("MIRROR {port}")) {
                    break;
                }
            }
            for port in remove {
                if !tell(&sandbox.control, &format!("UNMIRROR {port}")) {
                    break;
                }
            }
            sandbox.mirrored = wanted;
        }
    }

    /// Asks the sandbox that owns `port` for a new connection to it.
    fn open(&self, port: u16) -> io::Result<Stream> {
        let (tx, rx) = mpsc::channel();
        let conn = {
            let mut state = self.lock();
            let Some(owner) = state.sandboxes.iter().find(|s| s.ports.contains(&port)) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("nothing listens on port {port} in a sandbox"),
                ));
            };
            let control = owner.control.try_clone()?;
            let conn = loop {
                let conn = random_u64()?;
                if !state.pending.contains_key(&conn) {
                    break conn;
                }
            };
            state.pending.insert(conn, tx);
            if !tell(&control, &format!("OPEN {conn} {port}")) {
                state.pending.remove(&conn);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the sandbox does not read its control connection",
                ));
            }
            conn
        };
        rx.recv_timeout(OPEN_TIMEOUT).map_err(|_| {
            self.lock().pending.remove(&conn);
            io::Error::new(io::ErrorKind::TimedOut, "the sandbox did not answer")
        })
    }
}

/// How long a write to a bridge's control socket may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Writes one line to a bridge's control connection. A write that fails or
/// times out cuts the connection, so the control thread drops the sandbox
/// and later writes fail at once instead of timing out again.
fn tell(mut control: &Stream, line: &str) -> bool {
    let sent = writeln!(control, "{line}").is_ok();
    if !sent {
        let _ = control.shutdown(Shutdown::Both);
    }
    sent
}

/// A random connection id, so a bridge cannot guess the id of another's pending connection.
fn random_u64() -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(u64::from_le_bytes(bytes))
}

/// The host's end of the port forwarding of one scope. Dropping it stops
/// everything and removes its socket.
pub struct PortHub {
    inner: Arc<Inner>,
}

impl PortHub {
    /// Starts a hub on a new socket at `socket` (an old file there is
    /// replaced) and on a free loopback TCP port ([`Self::tcp_port`]).
    /// Windows has no socket, so `socket` is unused there.
    pub fn start(socket: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        let unix = {
            let _ = fs::remove_file(socket);
            UnixListener::bind(socket)?
        };
        let inner = Arc::new(Inner {
            socket: cfg!(unix).then(|| socket.to_path_buf()),
            tcp: Mutex::new(None),
            state: Mutex::new(State::default()),
            revision: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        });
        #[cfg(unix)]
        {
            let accept = Arc::clone(&inner);
            thread::Builder::new()
                .name("cmdterm-ports".into())
                .spawn(move || {
                    for client in unix.incoming() {
                        if accept.stop.load(Ordering::Relaxed) {
                            break;
                        }
                        if let Ok(client) = client {
                            let inner = Arc::clone(&accept);
                            thread::spawn(move || serve(&inner, Stream::Unix(client)));
                        }
                    }
                })?;
        }
        Ok(Self { inner })
    }

    /// Starts the loopback TCP listener, for bridges that cannot use the
    /// Unix socket, and returns its port and token. Later calls return the
    /// same pair. Connections must start with `AUTH <token>`. Until this is
    /// called, the hub is reachable only through its socket.
    pub fn start_tcp(&self) -> io::Result<(u16, String)> {
        let mut slot = self.inner.tcp.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(started) = &*slot {
            return Ok(started.clone());
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = format!("{:016x}{:016x}", random_u64()?, random_u64()?);
        let accept = Arc::clone(&self.inner);
        let expected = token.clone();
        thread::Builder::new()
            .name("cmdterm-ports-tcp".into())
            .spawn(move || {
                for client in listener.incoming() {
                    if accept.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(client) = client {
                        let inner = Arc::clone(&accept);
                        let token = expected.clone();
                        thread::spawn(move || {
                            let mut client = Stream::Tcp(client);
                            let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
                            if read_line(&mut client).as_deref() == Some(&format!("AUTH {token}")) {
                                serve(&inner, client);
                            }
                        });
                    }
                }
            })?;
        *slot = Some((port, token.clone()));
        Ok((port, token))
    }

    /// The ports that listen in some sandbox, sorted.
    pub fn ports(&self) -> Vec<ListeningPort> {
        let state = self.inner.lock();
        let all: BTreeSet<u16> = state
            .sandboxes
            .iter()
            .flat_map(|s| s.ports.iter().copied())
            .collect();
        all.into_iter()
            .map(|port| ListeningPort {
                port,
                exposed: state.exposed.get(&port).map(|e| e.host_port),
            })
            .collect()
    }

    /// A number that changes whenever [`Self::ports`] may have.
    pub fn revision(&self) -> u64 {
        self.inner.revision.load(Ordering::Relaxed)
    }

    /// Forwards a host port to `port` of the sandboxes and returns it. It is
    /// `host_port`, else `port`, else a free port when that one is taken.
    /// The exposure lasts until [`Self::unexpose`], also while nothing
    /// listens on `port`.
    pub fn expose(&self, port: u16, host_port: Option<u16>) -> io::Result<u16> {
        let existing = self.inner.lock().exposed.get(&port).map(|e| e.host_port);
        if let Some(existing) = existing {
            if host_port.is_none_or(|wanted| wanted == existing) {
                return Ok(existing);
            }
            self.unexpose(port);
        }
        let wanted = host_port.unwrap_or(port);
        let listener = match TcpListener::bind(("127.0.0.1", wanted)) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                TcpListener::bind(("127.0.0.1", 0))?
            }
            Err(error) => return Err(error),
        };
        let host_port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let accept = Arc::clone(&self.inner);
        let flag = Arc::clone(&stop);
        thread::Builder::new()
            .name("cmdterm-expose".into())
            .spawn(move || {
                for client in listener.incoming() {
                    if flag.load(Ordering::Relaxed) || accept.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(client) = client {
                        let inner = Arc::clone(&accept);
                        thread::spawn(move || {
                            if let Ok(owner) = inner.open(port) {
                                splice(client, owner, &[]);
                            }
                        });
                    }
                }
            })?;
        self.inner
            .lock()
            .exposed
            .insert(port, Exposure { host_port, stop });
        self.inner.bump();
        Ok(host_port)
    }

    /// Closes the host port of [`Self::expose`]. Open connections finish.
    pub fn unexpose(&self, port: u16) {
        let Some(exposure) = self.inner.lock().exposed.remove(&port) else {
            return;
        };
        exposure.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", exposure.host_port));
        self.inner.bump();
    }

    /// A new stream to `port` inside a sandbox.
    pub fn connect(&self, port: u16) -> io::Result<Stream> {
        self.inner.open(port)
    }

    /// The port of the hub's loopback HTTP proxy and the token it takes, as
    /// the password of `Proxy-Authorization: Basic` on the first request of
    /// every connection (see `proxy.rs`). The first call starts it.
    /// Requests for `localhost`, 127.0.0.1 and [::1] reach the sandbox
    /// ports. Other hosts go to the proxy of [`Self::set_upstream`], or get
    /// a `403` without one.
    pub fn http_proxy(&self) -> io::Result<(u16, ProxyToken)> {
        if let Some(proxy) = &self.inner.lock().proxy {
            return Ok(proxy.clone());
        }
        let token = ProxyToken::generate()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let accept = Arc::clone(&self.inner);
        let expected = Arc::new(token.clone());
        thread::Builder::new()
            .name("cmdterm-ports-http".into())
            .spawn(move || {
                for client in listener.incoming() {
                    if accept.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(client) = client {
                        let inner = Arc::clone(&accept);
                        let token = Arc::clone(&expected);
                        thread::spawn(move || serve_http(&inner, client, &token));
                    }
                }
            })?;
        let mut state = self.inner.lock();
        // A racing call may have won. Its listener stays, ours leaks until
        // the drop wakes it, which is harmless.
        Ok(state.proxy.get_or_insert((port, token)).clone())
    }

    /// Sets where [`Self::http_proxy`] sends requests for other hosts, such
    /// as the plain port of the sandbox's filtering proxy, and the token
    /// that port takes ([`crate::NetworkProxy::plain_port`]).
    pub fn set_upstream(&self, upstream: Option<(SocketAddr, ProxyToken)>) {
        self.inner.lock().upstream = upstream;
    }
}

impl Drop for PortHub {
    fn drop(&mut self) {
        let inner = &self.inner;
        inner.stop.store(true, Ordering::Relaxed);
        let (exposed, proxy) = {
            let mut state = inner.lock();
            for sandbox in state.sandboxes.drain(..) {
                let _ = sandbox.control.shutdown(Shutdown::Both);
            }
            state.pending.clear();
            (std::mem::take(&mut state.exposed), state.proxy.clone())
        };
        #[cfg(unix)]
        if let Some(socket) = &inner.socket {
            let _ = UnixStream::connect(socket);
        }
        let tcp = inner.tcp.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((port, _)) = tcp {
            let _ = TcpStream::connect(("127.0.0.1", port));
        }
        for exposure in exposed.values() {
            exposure.stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(("127.0.0.1", exposure.host_port));
        }
        if let Some((port, _)) = proxy {
            let _ = TcpStream::connect(("127.0.0.1", port));
        }
        if let Some(socket) = &inner.socket {
            let _ = fs::remove_file(socket);
        }
    }
}

/// Reads one line without reading past it, so raw bytes after it stay in
/// the stream.
fn read_line(stream: &mut impl Read) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 256 {
        match stream.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(1) => line.push(byte[0]),
            _ => return None,
        }
    }
    None
}

fn serve(inner: &Arc<Inner>, mut client: Stream) {
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
    let Some(line) = read_line(&mut client) else {
        return;
    };
    let _ = client.set_read_timeout(None);
    let mut words = line.split(' ');
    match (words.next(), words.next()) {
        (Some("HELLO"), Some(_)) => control(inner, client),
        (Some("DATA"), Some(conn)) => {
            let Some(tx) = conn
                .parse()
                .ok()
                .and_then(|conn: u64| inner.lock().pending.remove(&conn))
            else {
                return;
            };
            let _ = tx.send(client);
        }
        (Some("DIAL"), Some(port)) => {
            if let Some(owner) = port.parse().ok().and_then(|port| inner.open(port).ok()) {
                splice(client, owner, &[]);
            }
        }
        _ => {}
    }
}

fn control(inner: &Arc<Inner>, client: Stream) {
    let Ok(writer) = client.try_clone() else {
        return;
    };
    // Writes happen under the hub's lock, so a stuck bridge must not hold it.
    if writer.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
        return;
    }
    let id = {
        let mut state = inner.lock();
        let id = state.next;
        state.next += 1;
        state.sandboxes.push(Sandbox {
            id,
            control: writer,
            ports: BTreeSet::new(),
            mirrored: BTreeSet::new(),
        });
        Inner::sync_mirrors(&mut state);
        id
    };
    for line in BufReader::new(client).lines() {
        let Ok(line) = line else { break };
        let mut words = line.split(' ');
        let (Some(verb), Some(port)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        let mut state = inner.lock();
        let Some(sandbox) = state.sandboxes.iter_mut().find(|s| s.id == id) else {
            return;
        };
        match verb {
            "LISTEN" => {
                sandbox.ports.insert(port);
            }
            "CLOSED" => {
                sandbox.ports.remove(&port);
                // A mirror that failed to bind because of this server
                // gets another chance.
                sandbox.mirrored.remove(&port);
            }
            _ => continue,
        }
        Inner::sync_mirrors(&mut state);
        drop(state);
        inner.bump();
    }
    let mut state = inner.lock();
    state.sandboxes.retain(|s| s.id != id);
    Inner::sync_mirrors(&mut state);
    drop(state);
    inner.bump();
}

/// True when `host` names the sandbox's own loopback: `localhost`, a name
/// under `.localhost`, or a loopback address, with or without a final dot.
/// Anything else goes to the network proxy, which would reach this
/// machine's loopback instead.
fn is_local(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    host.eq_ignore_ascii_case("localhost")
        || host
            .len()
            .checked_sub(".localhost".len())
            .and_then(|at| host.get(at..))
            .is_some_and(|tail| tail.eq_ignore_ascii_case(".localhost"))
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| match ip {
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
            v4 => v4.is_loopback(),
        })
}

fn serve_http(inner: &Arc<Inner>, mut client: TcpStream, token: &ProxyToken) {
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
    let Some((head, rest)) = read_head(&mut client) else {
        return;
    };
    let _ = client.set_read_timeout(None);
    let Ok(text) = std::str::from_utf8(&head) else {
        return respond(&mut client, "400 Bad Request", "bad request head\n");
    };
    if !authorized(token, text.split("\r\n").skip(1)) {
        return unauthorized(&mut client);
    }
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next())
    else {
        return respond(&mut client, "400 Bad Request", "bad request line\n");
    };
    let tunnel = method.eq_ignore_ascii_case("CONNECT");
    let (authority, path) = if tunnel {
        (target, "")
    } else {
        let Some(url) = target
            .get(..7)
            .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
            .map(|_| &target[7..])
        else {
            return respond(
                &mut client,
                "400 Bad Request",
                "cmdterm ports: use CONNECT or an http:// URL\n",
            );
        };
        match url.find(['/', '?']) {
            Some(at) => (&url[..at], &url[at..]),
            None => (url, "/"),
        }
    };
    let Some((host, port)) = split_authority(authority, if tunnel { 443 } else { 80 }) else {
        return respond(&mut client, "400 Bad Request", "bad host\n");
    };
    if is_local(&host) {
        let owner = match inner.open(port) {
            Ok(owner) => owner,
            Err(error) => {
                let body = format!("cmdterm sandbox: port {port}: {error}\n");
                return respond(&mut client, "502 Bad Gateway", &body);
            }
        };
        if tunnel {
            if client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .is_ok()
            {
                splice(client, owner, &rest);
            }
            return;
        }
        // Origin form for the server, without the headers meant for us.
        // The connection carries this one request, since the next one
        // would reach the server unread, with its `Proxy-Authorization`.
        // An upgrade, such as a dev server's WebSocket, keeps its
        // `Connection`: what follows it is no longer HTTP.
        let path = if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_owned()
        };
        let header = |line: &str| line.split(':').next().unwrap_or_default().to_owned();
        let upgrade = lines
            .clone()
            .any(|line| header(line).eq_ignore_ascii_case("upgrade"));
        let mut hop = vec!["proxy-connection", "proxy-authorization"];
        if !upgrade {
            hop.extend(["connection", "keep-alive"]);
        }
        let mut forwarded = format!("{method} {path} {version}\r\n");
        for line in lines.filter(|line| !line.is_empty()) {
            let name = header(line);
            if hop.iter().any(|hop| name.eq_ignore_ascii_case(hop)) {
                continue;
            }
            forwarded.push_str(line);
            forwarded.push_str("\r\n");
        }
        if !upgrade {
            forwarded.push_str("Connection: close\r\n");
        }
        forwarded.push_str("\r\n");
        // The body stays bytes, so a binary upload arrives intact.
        let mut early = forwarded.into_bytes();
        early.extend_from_slice(&rest);
        splice(client, owner, &early);
        return;
    }
    let upstream = inner.lock().upstream.clone();
    let Some((upstream, token)) = upstream else {
        let body = format!("cmdterm sandbox: {host} is not reachable, there is no network proxy\n");
        return respond(&mut client, "403 Forbidden", &body);
    };
    match TcpStream::connect_timeout(&upstream, Duration::from_secs(10)) {
        Ok(upstream) => {
            // The proxy gets the hub's token, never one the client sent.
            let mut forwarded = String::with_capacity(head.len() + 128);
            for line in text.split("\r\n").filter(|line| !line.is_empty()) {
                let name = line.split(':').next().unwrap_or_default();
                if !name.eq_ignore_ascii_case("proxy-authorization") {
                    forwarded.push_str(line);
                    forwarded.push_str("\r\n");
                }
            }
            forwarded.push_str("Proxy-Authorization: ");
            forwarded.push_str(&token.header());
            forwarded.push_str("\r\n\r\n");
            let mut early = forwarded.into_bytes();
            early.extend_from_slice(&rest);
            splice(client, upstream, &early);
        }
        Err(error) => {
            let body = format!("cmdterm sandbox: network proxy: {error}\n");
            respond(&mut client, "502 Bad Gateway", &body);
        }
    }
}

/// The TCP ports that listen on 127.0.0.1, ::1 or every address, from the
/// text of `/proc/net/tcp` or `/proc/net/tcp6`. Sorted, without repeats.
#[cfg(unix)]
pub(crate) fn listening_ports(text: &str) -> Vec<u16> {
    // Addresses are the kernel's native-endian words in hex, so 127.0.0.1
    // reads as 0100007F on little-endian machines. The tests and the
    // sandboxes are little-endian, which covers every target of bwrap that
    // we run on; big-endian hosts only miss loopback servers.
    const LOCAL: [&str; 6] = [
        "0100007F",
        "00000000",
        "00000000000000000000000000000000",
        "00000000000000000000000001000000",
        "0000000000000000FFFF00000100007F",
        "0000000000000000FFFF000000000000",
    ];
    let mut ports = BTreeSet::new();
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let (Some(local), Some(state)) = (fields.nth(1), fields.nth(1)) else {
            continue;
        };
        if state != "0A" {
            continue;
        }
        let Some((address, port)) = local.split_once(':') else {
            continue;
        };
        if !LOCAL.contains(&address) {
            continue;
        }
        if let Ok(port) = u16::from_str_radix(port, 16) {
            ports.insert(port);
        }
    }
    ports.into_iter().collect()
}

/// The ports that listen in this Linux namespace, from `/proc/net/tcp`
/// and `/proc/net/tcp6`.
#[cfg(unix)]
pub(crate) fn proc_listeners() -> BTreeSet<u16> {
    let mut now = BTreeSet::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = fs::read_to_string(file) {
            now.extend(listening_ports(&text));
        }
    }
    now
}

/// The bridge's end of the protocol for a hub's Unix socket: joins the hub
/// at `hub` and returns once the control connection is up. Threads report
/// the listening ports, mirror ports and open connections. `own` are ports
/// the bridge itself listens on, which it never reports.
#[cfg(unix)]
pub(crate) fn run_bridge(hub: &Path, own: Vec<u16>) -> io::Result<()> {
    join(
        Hub::Unix(hub.to_path_buf()),
        own,
        Box::new(proc_listeners),
        true,
    )
}

/// Like [`run_bridge`] for a hub's TCP port and token. `listeners` finds the
/// listening ports, since there is no `/proc` to read. With `mirror` off
/// the bridge ignores `MIRROR`: sandboxes that share the host's loopback
/// already see each other's servers, and a mirror would bind host ports.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn run_bridge_tcp(
    hub: SocketAddr,
    token: String,
    own: Vec<u16>,
    listeners: Listeners,
    mirror: bool,
) -> io::Result<()> {
    join(Hub::Tcp(hub, token), own, listeners, mirror)
}

pub(crate) fn join(hub: Hub, own: Vec<u16>, listeners: Listeners, mirror: bool) -> io::Result<()> {
    let mut control = hub.connect()?;
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|e| io::Error::other(e.to_string()))?;
    let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(control, "HELLO {id}")?;
    let bridge = Arc::new(Bridge {
        hub,
        control: Mutex::new(control.try_clone()?),
        mirrors: Mutex::new(HashMap::new()),
        own,
        listeners,
        mirror,
        stop: AtomicBool::new(false),
    });
    let poller = Arc::clone(&bridge);
    thread::Builder::new()
        .name("cmdterm-bridge-poll".into())
        .spawn(move || poller.poll())?;
    let reader = Arc::clone(&bridge);
    thread::Builder::new()
        .name("cmdterm-bridge-control".into())
        .spawn(move || {
            for line in BufReader::new(control).lines() {
                let Ok(line) = line else { break };
                let mut words = line.split(' ');
                match (words.next(), words.next(), words.next()) {
                    (Some("MIRROR"), Some(port), None) if reader.mirror => {
                        if let Ok(port) = port.parse() {
                            reader.mirror(port);
                        }
                    }
                    (Some("UNMIRROR"), Some(port), None) if reader.mirror => {
                        if let Ok(port) = port.parse() {
                            reader.unmirror(port);
                        }
                    }
                    (Some("OPEN"), Some(conn), Some(port)) => {
                        let (conn, port) = (conn.to_owned(), port.parse());
                        if let Ok(port) = port {
                            let bridge = Arc::clone(&reader);
                            thread::spawn(move || bridge.open(&conn, port));
                        }
                    }
                    _ => {}
                }
            }
            reader.stop.store(true, Ordering::Relaxed);
        })?;
    Ok(())
}

struct Bridge {
    hub: Hub,
    control: Mutex<Stream>,
    /// Mirror listeners by port.
    mirrors: Mutex<HashMap<u16, Arc<Mirror>>>,
    own: Vec<u16>,
    listeners: Listeners,
    mirror: bool,
    stop: AtomicBool,
}

/// How often, and how far apart, a mirror tries to bind its port again
/// after it stopped, see [`Bridge::rebind`].
const REBIND_TRIES: u32 = 40;
const REBIND_WAIT: Duration = Duration::from_millis(25);

/// The flags of one mirror listener. Both change under the `mirrors` lock.
#[derive(Default)]
struct Mirror {
    /// Set by `UNMIRROR`: the listener stops at its next wake.
    stop: AtomicBool,
    /// Set by a `MIRROR` that came while the listener was stopping: it binds
    /// again once the old listener is gone.
    again: AtomicBool,
}

impl Bridge {
    fn send(&self, line: &str) -> bool {
        let mut control = self.control.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(control, "{line}").is_ok()
    }

    fn poll(&self) {
        let mut known = BTreeSet::new();
        while !self.stop.load(Ordering::Relaxed) {
            // Hold the lock across the listener read. A mirror's listener
            // thread drops its listener before it takes the lock to remove
            // its entry, so a snapshot that still shows the port is read
            // while the entry is present and the port is filtered out.
            let mirrors = self.mirrors.lock().unwrap_or_else(|e| e.into_inner());
            let mut now = (self.listeners)();
            now.retain(|port| !self.own.contains(port) && !mirrors.contains_key(port));
            drop(mirrors);
            for port in now.difference(&known) {
                if !self.send(&format!("LISTEN {port}")) {
                    return;
                }
            }
            for port in known.difference(&now) {
                if !self.send(&format!("CLOSED {port}")) {
                    return;
                }
            }
            known = now;
            thread::sleep(POLL);
        }
    }

    fn mirror(self: &Arc<Self>, port: u16) {
        let mut mirrors = self.mirrors.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = mirrors.get(&port) {
            // A stopping listener still holds the port, so bind once it
            // has let go of it.
            if old.stop.load(Ordering::Relaxed) {
                old.again.store(true, Ordering::Relaxed);
            }
            return;
        }
        let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
            return;
        };
        let mirror = Arc::new(Mirror::default());
        mirrors.insert(port, Arc::clone(&mirror));
        let hub = self.hub.clone();
        let bridge = Arc::clone(self);
        thread::spawn(move || {
            let mut listener = listener;
            loop {
                for client in listener.incoming() {
                    if mirror.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(client) = client else { continue };
                    let hub = hub.clone();
                    thread::spawn(move || {
                        if let Ok(mut owner) = hub.connect()
                            && writeln!(owner, "DIAL {port}").is_ok()
                        {
                            splice(client, owner, &[]);
                        }
                    });
                }
                // Free the port first. The entry stays while a MIRROR that
                // came meanwhile binds again, so the poll never sees this
                // mirror's own port as a foreign listener.
                drop(listener);
                match bridge.rebind(port, &mirror) {
                    Some(again) => listener = again,
                    None => break,
                }
            }
        });
    }

    /// After `mirror`'s listener stopped: a new listener on `port` when a
    /// `MIRROR` came while it stopped, or `None` once the entry is gone.
    /// The port can stay taken for a moment after the old listener closed,
    /// such as while a child forked elsewhere in this process still holds
    /// a copy of it until it runs its program, so binding is tried for a
    /// while, as long as no `UNMIRROR` comes.
    fn rebind(&self, port: u16, mirror: &Arc<Mirror>) -> Option<TcpListener> {
        // `again` stays set while binding, and an UNMIRROR clears it; `stop`
        // stays set until a listener holds the port again.
        let wanted = || mirror.again.load(Ordering::Relaxed) && !self.stop.load(Ordering::Relaxed);
        for attempt in 0..REBIND_TRIES {
            if !wanted() {
                break;
            }
            if attempt > 0 {
                thread::sleep(REBIND_WAIT);
            }
            let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
                continue;
            };
            let _mirrors = self.mirrors.lock().unwrap_or_else(|e| e.into_inner());
            if mirror.again.load(Ordering::Relaxed) {
                mirror.again.store(false, Ordering::Relaxed);
                mirror.stop.store(false, Ordering::Relaxed);
                return Some(listener);
            }
            break;
        }
        let mut mirrors = self.mirrors.lock().unwrap_or_else(|e| e.into_inner());
        if mirrors.get(&port).is_some_and(|m| Arc::ptr_eq(m, mirror)) {
            mirrors.remove(&port);
        }
        None
    }

    fn unmirror(&self, port: u16) {
        // The entry stays until the listener thread has dropped its listener.
        let mirrors = self.mirrors.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mirror) = mirrors.get(&port) else {
            return;
        };
        mirror.stop.store(true, Ordering::Relaxed);
        mirror.again.store(false, Ordering::Relaxed);
        drop(mirrors);
        let _ = TcpStream::connect(("127.0.0.1", port));
    }

    fn open(&self, conn: &str, port: u16) {
        let Ok(mut data) = self.hub.connect() else {
            return;
        };
        if writeln!(data, "DATA {conn}").is_err() {
            return;
        }
        let Ok(inside) = connect("127.0.0.1", port, true).or_else(|_| connect("::1", port, true))
        else {
            return;
        };
        splice(data, inside, &[]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 11111 1 0 100 0 0 10 0
   1: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 22222 1 0 100 0 0 10 0
   2: 0100007F:2328 0100007F:9C40 01 00000000:00000000 00:00000000 00000000  1000        0 33333 1 0 100 0 0 10 0
   3: 0A00A8C0:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 44444 1 0 100 0 0 10 0
";

    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 55555 1 0 100 0 0 10 0
   1: 00000000000000000000000000000000:1388 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 66666 1 0 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:1770 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 77777 1 0 100 0 0 10 0
   3: 20010DB8000000000000000000000001:1F40 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 88888 1 0 100 0 0 10 0
   4: 00000000000000000000000001000000:1B58 00000000000000000000000000000000:0000 06 00000000:00000000 00:00000000 00000000  1000        0 99999 1 0 100 0 0 10 0
";

    #[test]
    fn proc_net_tcp_gives_the_local_listeners() {
        // 8080 on loopback and 80 on every address. 9000 is established and
        // 8081 listens on an address no other sandbox shares.
        assert_eq!(listening_ports(TCP), [80, 8080]);
        // 3000 on ::1, 5000 on ::, 6000 on a mapped loopback. 8000 is on a
        // global address and 7000 is not listening.
        assert_eq!(listening_ports(TCP6), [3000, 5000, 6000]);
        assert!(listening_ports("").is_empty());
    }

    /// A TCP server that echoes every byte.
    fn echo_server() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut read = stream.try_clone().unwrap();
                    let mut write = stream;
                    let _ = io::copy(&mut read, &mut write);
                });
            }
        });
        port
    }

    /// An HTTP server that answers with the request it got.
    fn http_server() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                if let Some((head, _)) = read_head(&mut stream) {
                    let head = String::from_utf8_lossy(&head);
                    respond(&mut stream, "200 OK", head.lines().next().unwrap());
                }
            }
        });
        port
    }

    /// `request` with `Proxy-Authorization` for `token` after its first line.
    fn with_token(request: &str, token: &ProxyToken) -> String {
        let (line, rest) = request.split_once("\r\n").unwrap();
        format!(
            "{line}\r\nProxy-Authorization: {}\r\n{rest}",
            token.header()
        )
    }

    #[test]
    fn the_http_proxy_takes_its_token_and_keeps_it_from_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = PortHub::start(&socket).unwrap();
        // A server in the sandbox that hands over every request head.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let server = listener.local_addr().unwrap().port();
        let (heads, received) = mpsc::channel();
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                if let Some((head, _)) = read_head(&mut stream) {
                    let _ = heads.send(String::from_utf8_lossy(&head).into_owned());
                    respond(&mut stream, "200 OK", "inside");
                }
            }
        });
        let bridge = FakeBridge::join(&socket);
        bridge.say(&format!("LISTEN {server}"));
        until(|| !hub.ports().is_empty());
        let (proxy, token) = hub.http_proxy().unwrap();
        let ask = |request: &str| {
            let mut stream = TcpStream::connect(("127.0.0.1", proxy)).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
            reply
        };
        let get = format!(
            "GET http://localhost:{server}/ HTTP/1.1\r\nHost: localhost\r\n\
             Connection: keep-alive\r\n\r\n"
        );
        let tunnel = format!("CONNECT localhost:{server} HTTP/1.1\r\n\r\n");
        let other = ProxyToken::generate().unwrap();
        for request in [get.clone(), tunnel, with_token(&get, &other)] {
            let reply = ask(&request);
            assert!(reply.starts_with("HTTP/1.1 407"), "{reply}");
            assert!(reply.contains("Proxy-Authenticate: Basic"), "{reply}");
        }
        assert!(received.recv_timeout(Duration::from_millis(200)).is_err());

        let reply = ask(&with_token(&get, &token));
        assert!(reply.ends_with("inside"), "{reply}");
        let head = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            !head.to_ascii_lowercase().contains("proxy-authorization"),
            "{head}"
        );
        assert!(!head.contains("keep-alive"), "{head}");
        assert!(head.contains("Connection: close"), "{head}");
        // An upgrade keeps its `Connection`.
        let upgrade = format!(
            "GET http://localhost:{server}/ws HTTP/1.1\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\n\r\n"
        );
        assert!(ask(&with_token(&upgrade, &token)).ends_with("inside"));
        let head = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.contains("Connection: Upgrade"), "{head}");
        assert!(!head.contains("Connection: close"), "{head}");
    }

    /// A bridge in a test: it speaks the protocol and answers `OPEN` by
    /// dialing the real port on the host's loopback, and it records what the
    /// hub tells it.
    struct FakeBridge {
        control: UnixStream,
        told: mpsc::Receiver<String>,
    }

    impl FakeBridge {
        fn join(socket: &Path) -> Self {
            let control = UnixStream::connect(socket).unwrap();
            writeln!(&control, "HELLO test").unwrap();
            let (tx, told) = mpsc::channel();
            let reader = control.try_clone().unwrap();
            let hub = socket.to_path_buf();
            thread::spawn(move || {
                for line in BufReader::new(reader).lines().map_while(Result::ok) {
                    if let Some(rest) = line.strip_prefix("OPEN ") {
                        let (conn, port) = rest.split_once(' ').unwrap();
                        let (conn, port): (String, u16) = (conn.into(), port.parse().unwrap());
                        let hub = hub.clone();
                        thread::spawn(move || {
                            let mut data = UnixStream::connect(&hub).unwrap();
                            writeln!(data, "DATA {conn}").unwrap();
                            if let Ok(inside) = TcpStream::connect(("127.0.0.1", port)) {
                                splice(data, inside, &[]);
                            }
                        });
                    } else {
                        let _ = tx.send(line);
                    }
                }
            });
            Self { control, told }
        }

        fn say(&self, line: &str) {
            writeln!(&self.control, "{line}").unwrap();
        }

        fn heard(&self) -> String {
            self.told.recv_timeout(Duration::from_secs(5)).unwrap()
        }
    }

    impl Drop for FakeBridge {
        fn drop(&mut self) {
            let _ = self.control.shutdown(Shutdown::Both);
        }
    }

    /// A free port below the ephemeral ranges (Linux from 32768, macOS from
    /// 49152), for a test that binds it again after letting it go. A port
    /// from `bind(0)` can meanwhile become the source port of another test's
    /// connection: macOS hands out the same range for both, so a rebind
    /// there fails until that connection ends.
    fn unshared_port() -> u16 {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        // Apart per process, so test binaries running side by side differ.
        let base = 20_000 + (std::process::id() % 1_000) * 10;
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let port = u16::try_from(base + n % 12_000).unwrap();
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                return port;
            }
        }
    }

    fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..200 {
            if done() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("timed out");
    }

    fn echo_through(mut stream: impl Read + Write) -> String {
        stream.write_all(b"ping").unwrap();
        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).unwrap();
        String::from_utf8_lossy(&reply).into_owned()
    }

    #[test]
    fn the_hub_routes_between_bridges() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = PortHub::start(&socket).unwrap();
        let server = echo_server();
        let a = FakeBridge::join(&socket);
        let b = FakeBridge::join(&socket);
        a.say(&format!("LISTEN {server}"));
        assert_eq!(b.heard(), format!("MIRROR {server}"));
        assert_eq!(
            hub.ports(),
            [ListeningPort {
                port: server,
                exposed: None
            }]
        );
        assert!(hub.revision() > 0);

        // A mirror's connection: DIAL, and the hub opens the owner.
        let mut dial = UnixStream::connect(&socket).unwrap();
        writeln!(dial, "DIAL {server}").unwrap();
        assert_eq!(echo_through(dial), "ping");
        // The same through connect.
        assert_eq!(echo_through(hub.connect(server).unwrap()), "ping");
        assert!(hub.connect(1).is_err());

        a.say(&format!("CLOSED {server}"));
        assert_eq!(b.heard(), format!("UNMIRROR {server}"));
        assert!(hub.ports().is_empty());

        // A sandbox that goes away takes its ports with it.
        a.say(&format!("LISTEN {server}"));
        assert_eq!(b.heard(), format!("MIRROR {server}"));
        drop(a);
        assert_eq!(b.heard(), format!("UNMIRROR {server}"));
        until(|| hub.ports().is_empty());
        drop(hub);
        assert!(!socket.exists());
    }

    #[test]
    fn exposing_forwards_a_host_port_until_it_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = PortHub::start(&dir.path().join("hub.sock")).unwrap();
        let server = echo_server();
        let bridge = FakeBridge::join(&dir.path().join("hub.sock"));
        bridge.say(&format!("LISTEN {server}"));
        until(|| !hub.ports().is_empty());

        // The port is taken on the host by the echo server itself, so the
        // hub picks another.
        let host = hub.expose(server, None).unwrap();
        assert_ne!(host, server);
        assert_eq!(hub.expose(server, None).unwrap(), host);
        assert_eq!(hub.ports()[0].exposed, Some(host));
        assert_eq!(
            echo_through(TcpStream::connect(("127.0.0.1", host)).unwrap()),
            "ping"
        );

        // It stays exposed when the port goes away and comes back.
        bridge.say(&format!("CLOSED {server}"));
        until(|| hub.ports().is_empty());
        bridge.say(&format!("LISTEN {server}"));
        until(|| hub.ports().first().is_some_and(|p| p.exposed == Some(host)));
        assert_eq!(
            echo_through(TcpStream::connect(("127.0.0.1", host)).unwrap()),
            "ping"
        );

        hub.unexpose(server);
        assert_eq!(hub.ports()[0].exposed, None);
        until(|| TcpStream::connect(("127.0.0.1", host)).is_err());
    }

    #[test]
    fn the_http_proxy_sends_localhost_into_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = PortHub::start(&socket).unwrap();
        let server = http_server();
        let bridge = FakeBridge::join(&socket);
        bridge.say(&format!("LISTEN {server}"));
        until(|| !hub.ports().is_empty());
        let (proxy, hub_token) = hub.http_proxy().unwrap();
        assert_eq!(hub.http_proxy().unwrap(), (proxy, hub_token.clone()));
        let ask = |request: String| {
            let mut stream = TcpStream::connect(("127.0.0.1", proxy)).unwrap();
            let request = with_token(&request, &hub_token);
            stream.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
            reply
        };

        let reply = ask(format!(
            "GET http://localhost:{server}/a?b HTTP/1.1\r\nHost: x\r\n\
             Proxy-Connection: keep-alive\r\n\r\n"
        ));
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.ends_with("GET /a?b HTTP/1.1"), "{reply}");
        // A tunnel stays open, so read only its answer, then use it.
        let mut tunnel = TcpStream::connect(("127.0.0.1", proxy)).unwrap();
        tunnel
            .write_all(
                with_token(
                    &format!("CONNECT [::1]:{server} HTTP/1.1\r\n\r\n"),
                    &hub_token,
                )
                .as_bytes(),
            )
            .unwrap();
        let (head, _) = read_head(&mut tunnel).unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200 Connection"));
        tunnel
            .write_all(b"GET /t HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        let _ = tunnel.read_to_string(&mut reply);
        assert!(reply.ends_with("GET /t HTTP/1.1"), "{reply}");
        let reply = ask("GET http://127.0.0.1:1/ HTTP/1.1\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 502"), "{reply}");

        // Other hosts: refused, then sent to the upstream.
        let reply = ask("GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        let upstream = http_server();
        let token = ProxyToken::generate().unwrap();
        hub.set_upstream(Some((([127, 0, 0, 1], upstream).into(), token)));
        let reply = ask("GET http://example.com/x HTTP/1.1\r\nHost: example.com\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(
            reply.ends_with("GET http://example.com/x HTTP/1.1"),
            "{reply}"
        );
        // A filtering proxy's plain port takes the hub's token in place of
        // the client's: its 403 says the token passed.
        let proxy_dir = tempfile::tempdir().unwrap();
        let network =
            crate::NetworkProxy::start(&proxy_dir.path().join("p.sock"), Vec::new()).unwrap();
        let (plain, token) = network.plain_port().unwrap();
        hub.set_upstream(Some((([127, 0, 0, 1], plain).into(), token)));
        let get = "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\
                   Proxy-Authorization: Basic Y21kdGVybTp3cm9uZw==\r\n\r\n";
        let reply = ask(get.into());
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
        assert!(
            reply.contains("example.com is not an allowed host"),
            "{reply}"
        );
        let wrong = ProxyToken::generate().unwrap();
        hub.set_upstream(Some((([127, 0, 0, 1], plain).into(), wrong)));
        let reply = ask(get.into());
        assert!(reply.starts_with("HTTP/1.1 407"), "{reply}");
        hub.set_upstream(None);
        let reply = ask("GET http://example.com/ HTTP/1.1\r\n\r\n".into());
        assert!(reply.starts_with("HTTP/1.1 403"), "{reply}");
    }

    #[test]
    fn connection_ids_are_random() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = Arc::new(PortHub::start(&socket).unwrap());
        let server = echo_server();
        // A bridge that reads OPEN lines and never answers them.
        let mut bridge = UnixStream::connect(&socket).unwrap();
        writeln!(bridge, "HELLO ids").unwrap();
        writeln!(bridge, "LISTEN {server}").unwrap();
        until(|| !hub.ports().is_empty());
        let mut lines = BufReader::new(bridge.try_clone().unwrap()).lines();
        let mut ids = Vec::new();
        for _ in 0..3 {
            let hub = Arc::clone(&hub);
            // Nobody answers the OPEN, so this gives up by itself later.
            thread::spawn(move || {
                let _ = hub.connect(server);
            });
            let line = lines.next().unwrap().unwrap();
            let id: u64 = line.split(' ').nth(1).unwrap().parse().unwrap();
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert!(
            ids.windows(2).all(|w| w[1] - w[0] > 1000),
            "ids look sequential: {ids:?}"
        );
    }

    #[test]
    fn a_bridge_that_stops_reading_does_not_block_the_hub() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = PortHub::start(&socket).unwrap();
        // This bridge joins and never reads what the hub tells it.
        let mut stuck = UnixStream::connect(&socket).unwrap();
        writeln!(stuck, "HELLO stuck").unwrap();
        let talker = FakeBridge::join(&socket);
        // More MIRROR lines than the socket buffers hold.
        let ports = 3_000;
        for port in 1000..1000 + ports {
            talker.say(&format!("LISTEN {port}"));
        }
        let started = std::time::Instant::now();
        while hub.ports().len() < ports as usize {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "the hub is stuck on a bridge that does not read"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_closing_mirror_is_not_reported_as_a_listener() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = PortHub::start(&socket).unwrap();
        let free = unshared_port();
        let owner = FakeBridge::join(&socket);
        run_bridge(&socket, vec![]).unwrap();
        owner.say(&format!("LISTEN {free}"));
        // The real bridge now mirrors the port on its own loopback.
        until(|| TcpStream::connect(("127.0.0.1", free)).is_ok());
        owner.say(&format!("CLOSED {free}"));
        until(|| TcpStream::connect(("127.0.0.1", free)).is_err());
        // Several polls later the bridge has not reported the port as its own.
        thread::sleep(POLL * 3);
        assert!(hub.ports().iter().all(|p| p.port != free));
    }

    /// The bridge finds listeners in `/proc/net/tcp`, which only Linux has.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_bridge_mirrors_and_reports() {
        // The bridge side against a hub, both in this process: a listener
        // in the "sandbox" (the test's own loopback) shows up as a port.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = PortHub::start(&socket).unwrap();
        let server = echo_server();
        run_bridge(&socket, vec![]).unwrap();
        until(|| hub.ports().iter().any(|p| p.port == server));
        // Opening a connection through the hub reaches the server.
        assert_eq!(echo_through(hub.connect(server).unwrap()), "ping");
    }

    /// A fake bridge over the hub's TCP port, as the Windows launcher's
    /// bridge connects.
    #[test]
    fn the_tcp_control_path_routes_like_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let hub = PortHub::start(&dir.path().join("hub.sock")).unwrap();
        let (port, token) = hub.start_tcp().unwrap();
        assert_eq!(hub.start_tcp().unwrap(), (port, token.clone()));
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let server = echo_server();
        let connect = move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            writeln!(stream, "AUTH {token}").unwrap();
            stream
        };

        // Without the token, nothing is served, not even a dial.
        let mut stranger = TcpStream::connect(addr).unwrap();
        writeln!(stranger, "HELLO stranger").unwrap();
        let mut wrong = TcpStream::connect(addr).unwrap();
        writeln!(wrong, "AUTH nope").unwrap();
        writeln!(wrong, "HELLO wrong").unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(hub.ports().is_empty());

        let mut owner = connect();
        writeln!(owner, "HELLO tcp-owner").unwrap();
        let mut other = connect();
        writeln!(other, "HELLO tcp-other").unwrap();
        let mut told = BufReader::new(other.try_clone().unwrap()).lines();
        writeln!(owner, "LISTEN {server}").unwrap();
        assert_eq!(told.next().unwrap().unwrap(), format!("MIRROR {server}"));
        assert_eq!(hub.ports()[0].port, server);

        // The owner answers OPEN over TCP, dialing the real server.
        let reader = owner.try_clone().unwrap();
        let spawner = connect.clone();
        thread::spawn(move || {
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("OPEN ") {
                    let (conn, port) = rest.split_once(' ').unwrap();
                    let (conn, port): (String, u16) = (conn.into(), port.parse().unwrap());
                    let connect = spawner.clone();
                    thread::spawn(move || {
                        let mut data = connect();
                        writeln!(data, "DATA {conn}").unwrap();
                        if let Ok(inside) = TcpStream::connect(("127.0.0.1", port)) {
                            splice(data, inside, &[]);
                        }
                    });
                }
            }
        });
        let mut dial = connect();
        writeln!(dial, "DIAL {server}").unwrap();
        assert_eq!(echo_through(dial), "ping");
        assert_eq!(echo_through(hub.connect(server).unwrap()), "ping");

        // Exposing works for a port owned over TCP.
        let host = hub.expose(server, None).unwrap();
        assert_eq!(
            echo_through(TcpStream::connect(("127.0.0.1", host)).unwrap()),
            "ping"
        );

        let _ = owner.shutdown(Shutdown::Both);
        assert_eq!(told.next().unwrap().unwrap(), format!("UNMIRROR {server}"));
        until(|| hub.ports().is_empty());
    }

    #[test]
    fn a_mirror_asked_for_while_it_stops_comes_back() {
        let (control, _hub) = UnixStream::pair().unwrap();
        let bridge = Arc::new(Bridge {
            hub: Hub::Unix(PathBuf::from("/nonexistent")),
            control: Mutex::new(Stream::Unix(control)),
            mirrors: Mutex::new(HashMap::new()),
            own: vec![],
            listeners: Box::new(BTreeSet::new),
            mirror: true,
            stop: AtomicBool::new(false),
        });
        let port = unshared_port();
        let mirrored = || {
            let mirrors = bridge.mirrors.lock().unwrap();
            mirrors
                .get(&port)
                .is_some_and(|m| !m.stop.load(Ordering::Relaxed))
        };
        for _ in 0..20 {
            // A first MIRROR whose bind fails waits for the next one, as
            // the hub sends it; other tests' children can hold the port
            // for a moment after the last listener closed.
            until(|| {
                bridge.mirror(port);
                mirrored()
            });
            // UNMIRROR and MIRROR back to back: the second must not be lost
            // while the first listener is still letting go of the port.
            bridge.unmirror(port);
            bridge.mirror(port);
            until(&mirrored);
            assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
            bridge.unmirror(port);
            until(|| bridge.mirrors.lock().unwrap().is_empty());
        }
        // UNMIRROR, MIRROR, UNMIRROR ends unmirrored.
        bridge.mirror(port);
        bridge.unmirror(port);
        bridge.mirror(port);
        bridge.unmirror(port);
        until(|| bridge.mirrors.lock().unwrap().is_empty());
        thread::sleep(Duration::from_millis(50));
        assert!(bridge.mirrors.lock().unwrap().is_empty());
    }

    #[test]
    fn a_tcp_bridge_reports_ports_and_ignores_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let hub = PortHub::start(&dir.path().join("hub.sock")).unwrap();
        let server = echo_server();
        let (port, token) = hub.start_tcp().unwrap();
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let seen = Arc::new(Mutex::new(BTreeSet::from([server])));
        let source = Arc::clone(&seen);
        run_bridge_tcp(
            addr,
            token,
            vec![],
            Box::new(move || source.lock().unwrap().clone()),
            false,
        )
        .unwrap();
        until(|| hub.ports().iter().any(|p| p.port == server));
        assert_eq!(echo_through(hub.connect(server).unwrap()), "ping");
        seen.lock().unwrap().clear();
        until(|| hub.ports().is_empty());

        // A port another sandbox owns is not mirrored on this loopback.
        let free = unshared_port();
        let owner = FakeBridge::join(&dir.path().join("hub.sock"));
        owner.say(&format!("LISTEN {free}"));
        until(|| !hub.ports().is_empty());
        thread::sleep(POLL);
        assert!(TcpStream::connect(("127.0.0.1", free)).is_err());
    }
}
