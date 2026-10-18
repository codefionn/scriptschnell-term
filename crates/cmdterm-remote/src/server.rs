//! The remote machine server.
//!
//! `cmdterm-remote serve` runs it as a dedicated server, and the desktop app
//! runs it in a thread with `cmdterm --serve`. Each TCP connection gets a
//! thread: handshake, then [`mux::serve`] until the client leaves.
//! `authorized_keys` is read per connection, so `cmdterm-remote authorize`
//! takes effect without a restart. A server also dials the web servers in
//! `links.json` ([`crate::link`]) and serves them over that connection.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::exec::{self, Channel, Outbox};
use crate::keys::{self, Identity, KeyOptions, PublicKey};
use crate::link::{self, Link};
use crate::mux::{self, Backend};
use crate::projects;
use crate::proto::OpenSpec;
use crate::session::{END_PROGRAM, LIST_PROGRAM, Sessions};
use crate::vm::{FirecrackerConfig, VmChannel, VmManager};
use crate::{Paths, handshake};

/// Most connections served at once.
const MAX_CONNECTIONS: usize = 256;
/// How often `links.json` is read again for links added meanwhile.
const LINKS_RESCAN: Duration = Duration::from_secs(10);
/// How long a client may take to finish the whole handshake, however
/// slowly it sends.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// `server.json`. A missing file means the defaults: the host is offered,
/// VMs are not.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Run channels without a VM directly on this machine. Keys with their
    /// own `host` or `vm-only` option in `authorized_keys` ignore it.
    #[serde(default = "yes")]
    pub offer_host: bool,
    #[serde(default)]
    pub firecracker: Option<FirecrackerConfig>,
    /// Seconds a session runs without a client before it ends. 0 keeps it
    /// until its program exits. Default: seven days.
    #[serde(default)]
    pub detached_secs: Option<u64>,
    /// Tell clients which repositories this machine's own `state.json`
    /// lists, so their sidebars show them. Needs host access. Keys with
    /// their own `projects` or `no-projects` option ignore it.
    #[serde(default = "yes")]
    pub share_projects: bool,
    /// Serve the web servers in `links.json` only in VMs, never on this
    /// machine itself. Default false: a linked web server gets the same
    /// host access as any client key whose `offer_host` allows, and every
    /// browser session of its account can open a shell here.
    #[serde(default)]
    pub web_links_vm_only: bool,
}

fn yes() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            offer_host: true,
            firecracker: None,
            detached_secs: None,
            share_projects: true,
            web_links_vm_only: false,
        }
    }
}

impl ServerConfig {
    /// Whether a key with `options` may run commands on the host itself.
    pub fn host_for(&self, options: &KeyOptions) -> bool {
        options.host.unwrap_or(self.offer_host)
    }

    /// Whether a key with `options` may ask for the shared projects.
    pub fn projects_for(&self, options: &KeyOptions) -> bool {
        self.host_for(options) && options.projects.unwrap_or(self.share_projects)
    }

    /// The options a web link in `links.json` serves with: VMs only when
    /// `web_links_vm_only` is set, else the same defaults as any client key.
    pub fn link_options(&self) -> KeyOptions {
        if self.web_links_vm_only {
            KeyOptions {
                host: Some(false),
                projects: Some(false),
            }
        } else {
            KeyOptions::default()
        }
    }

    /// How long sessions wait for a client, `None` without a limit.
    pub fn detached_limit(&self) -> Option<Duration> {
        match self.detached_secs {
            None => Some(crate::session::DEFAULT_DETACHED),
            Some(0) => None,
            Some(secs) => Some(Duration::from_secs(secs)),
        }
    }

    pub fn load(paths: &Paths) -> io::Result<Self> {
        match std::fs::read_to_string(paths.server_config()) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", paths.server_config().display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    /// Changes `server.json` as JSON, so fields `edit` does not touch keep
    /// their values. The result must still be a valid config; otherwise the
    /// file stays as it was and the error says why.
    pub fn edit(
        paths: &Paths,
        edit: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    ) -> io::Result<Self> {
        let path = paths.server_config();
        let mut value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
            Err(error) => return Err(error),
        };
        let serde_json::Value::Object(map) = &mut value else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a JSON object", path.display()),
            ));
        };
        edit(map);
        let config: Self = serde_json::from_value(value.clone())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        crate::create_private_dir(&paths.dir)?;
        let partial = path.with_extension("partial");
        let mut text = serde_json::to_string_pretty(&value).map_err(io::Error::other)?;
        text.push('\n');
        std::fs::write(&partial, text)?;
        std::fs::rename(partial, path)?;
        Ok(config)
    }
}

pub struct Server {
    identity: Identity,
    paths: Paths,
    config: ServerConfig,
    vms: Option<Arc<VmManager>>,
    sessions: Arc<Sessions>,
    connections: AtomicUsize,
    /// Where [`Server::spawn`] listens, for [`Server::stop`].
    bound: Mutex<Option<SocketAddr>>,
    stopping: AtomicBool,
    /// Set once the accept loop has closed its listener.
    closed: AtomicBool,
    /// The sockets of the links to web servers that are connected now, so
    /// [`Server::stop`] can end them.
    links: Mutex<Vec<TcpStream>>,
    /// The URLs of the links that have a thread.
    linked: Mutex<std::collections::HashSet<String>>,
    /// Pre-handshake attempt limit per address, so unauthenticated callers
    /// cannot keep the post-quantum handshake busy.
    attempts: crate::rate::Attempts,
}

impl Server {
    /// Loads or creates the host key and reads `server.json`.
    pub fn new(paths: Paths) -> io::Result<Arc<Self>> {
        let identity = paths.load_identity()?;
        let config = ServerConfig::load(&paths)?;
        Ok(Self::with_config(paths, identity, config))
    }

    pub fn with_config(paths: Paths, identity: Identity, config: ServerConfig) -> Arc<Self> {
        let vms = config.firecracker.clone().map(VmManager::new);
        let sessions = Sessions::new(config.detached_limit());
        Arc::new(Self {
            identity,
            paths,
            config,
            vms,
            sessions,
            connections: AtomicUsize::new(0),
            bound: Mutex::new(None),
            stopping: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            links: Mutex::default(),
            linked: Mutex::default(),
            attempts: crate::rate::Attempts::new(),
        })
    }

    pub fn public_key(&self) -> &PublicKey {
        self.identity.public()
    }

    /// Binds `address` and serves in a background thread. Also keeps a
    /// connection to every web server in `links.json`.
    pub fn spawn(self: &Arc<Self>, address: &str) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind(address)?;
        let local = listener.local_addr()?;
        *self.bound.lock().unwrap() = Some(local);
        let server = self.clone();
        thread::spawn(move || server.serve(listener));
        let server = self.clone();
        thread::spawn(move || server.watch_links());
        Ok(local)
    }

    /// Starts a connection for every link in `links.json`, also for links
    /// added while the server runs, until the server stops.
    fn watch_links(self: Arc<Self>) {
        let mut reported = String::new();
        while !self.stopping.load(Ordering::SeqCst) {
            match link::load(&self.paths) {
                Ok(links) => {
                    for link in links {
                        let new = self.linked.lock().unwrap().insert(link.url.clone());
                        if new {
                            let server = self.clone();
                            thread::spawn(move || {
                                let url = link.url.clone();
                                server.clone().keep_link(link);
                                server.linked.lock().unwrap().remove(&url);
                            });
                        }
                    }
                }
                Err(error) if error.to_string() != reported => {
                    reported = error.to_string();
                    log(&format!("links: {reported}"));
                }
                Err(_) => {}
            }
            let until = Instant::now() + LINKS_RESCAN;
            while Instant::now() < until && !self.stopping.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(250));
            }
        }
    }

    /// Connects to the web server of `link` and serves it, again and again,
    /// until the server stops or the link is removed.
    fn keep_link(self: Arc<Self>, link: Link) {
        let mut misses = 0;
        let mut last_error = String::new();
        while !self.stopping.load(Ordering::SeqCst) {
            let started = Instant::now();
            match self.link_once(&link) {
                Ok(()) => last_error.clear(),
                // Say it once, not on every retry of a machine that is
                // offline for hours.
                Err(error) if error.to_string() != last_error => {
                    last_error = error.to_string();
                    log(&format!("link to {}: {last_error}", link.url));
                }
                Err(_) => {}
            }
            if started.elapsed() >= link::STABLE {
                misses = 0;
            }
            let until = Instant::now() + link::retry_after(misses);
            misses = misses.saturating_add(1);
            while Instant::now() < until {
                if self.stopping.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(250));
            }
            let still =
                link::load(&self.paths).is_ok_and(|links| links.iter().any(|l| l.url == link.url));
            if !still {
                return;
            }
        }
    }

    /// One connection to the web server of `link`, served until it ends.
    fn link_once(&self, link: &Link) -> io::Result<()> {
        let pinned = link.key()?;
        let stream = crate::client::tcp_connect(&link.address)?;
        stream.set_nodelay(true)?;
        crate::transport::keep_alive(&stream);
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let control = stream.try_clone()?;
        let deadline = handshake_deadline(&stream)?;
        let session = handshake::client(stream, &self.identity, |key| {
            if *key == pinned {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "the key of {} changed to {}; link again if that was on purpose",
                        link.url,
                        key.fingerprint()
                    ),
                ))
            }
        })?;
        drop(deadline);
        control.set_read_timeout(None)?;
        if let Ok(clone) = control.try_clone() {
            self.links.lock().unwrap().push(clone);
        }
        log(&format!("linked to {}", link.url));
        // The web server opens channels like a client: its terminals are
        // sessions of its key. `web_links_vm_only` limits it to VMs.
        let options = self.config.link_options();
        if !self.config.web_links_vm_only {
            log(&format!(
                "serving the web link {} with host access; set web_links_vm_only in {} \
                 to restrict it to VMs",
                link.url,
                self.paths.server_config().display()
            ));
        }
        let backend = ConnectionBackend {
            server: self,
            client: session.peer.clone(),
            options,
        };
        let result =
            mux::serve_closing(session.reader, session.writer, &backend, closer(&control)?);
        if let Ok(local) = control.local_addr() {
            self.links
                .lock()
                .unwrap()
                .retain(|s| s.local_addr().ok() != Some(local));
        }
        result
    }

    /// Accepts connections until the listener fails or [`Server::stop`].
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            if self.stopping.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            // Refuse addresses that hammer the port before any handshake
            // work is spent on them.
            if let Some(ip) = stream.peer_addr().ok().map(|a| a.ip())
                && !self.attempts.allow(ip)
            {
                continue;
            }
            if self.connections.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                self.connections.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let server = self.clone();
            thread::spawn(move || {
                let peer = stream.peer_addr().ok();
                if let Err(error) = server.connection(stream) {
                    log(&format!(
                        "connection from {}: {error}",
                        peer.map(|p| p.to_string()).unwrap_or_default()
                    ));
                }
                server.connections.fetch_sub(1, Ordering::SeqCst);
            });
        }
        drop(listener);
        self.closed.store(true, Ordering::SeqCst);
    }

    fn connection(&self, stream: TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        crate::transport::keep_alive(&stream);
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        // Clones share the socket, so this one clears the timeout for both
        // halves after the handshake.
        let control = stream.try_clone()?;
        let deadline = handshake_deadline(&stream)?;
        let authorized_keys = self.paths.authorized_keys();
        let session = handshake::server(stream, &self.identity, |key| {
            keys::authorized(&authorized_keys, key)
        })?;
        drop(deadline);
        control.set_read_timeout(None)?;
        // A login with an authorized key does not count against the
        // address, so `exec --direct` can run many commands a minute.
        if let Ok(peer) = control.peer_addr() {
            self.attempts.forgive(peer.ip());
        }
        let backend = ConnectionBackend {
            server: self,
            client: session.peer.clone(),
            options: session.options,
        };
        mux::serve_closing(session.reader, session.writer, &backend, closer(&control)?)
    }

    /// Ends sessions and stops VMs. Call before the process exits.
    pub fn shutdown(&self) {
        self.sessions.stop_all();
        if let Some(vms) = &self.vms {
            vms.stop_all();
        }
    }

    /// Stops accepting connections on the address [`Server::spawn`] bound,
    /// waits until the port is free, and stops VMs. Sessions already logged in run until their client
    /// leaves; their channels into VMs end with the VMs.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for link in self.links.lock().unwrap().drain(..) {
            let _ = link.shutdown(Shutdown::Both);
        }
        if let Some(mut bound) = self.bound.lock().unwrap().take() {
            // Wake the accept loop so it sees the flag.
            if bound.ip().is_unspecified() {
                bound.set_ip(if bound.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                });
            }
            let _ = TcpStream::connect_timeout(&bound, Duration::from_secs(1));
            // The port is free again once the loop dropped the listener, so
            // a new server can bind it right after this returns.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !self.closed.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
        }
        self.shutdown();
    }
}

struct ConnectionBackend<'a> {
    server: &'a Server,
    client: PublicKey,
    options: KeyOptions,
}

impl Backend for ConnectionBackend<'_> {
    fn open(&self, id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
        if !spec.shell && spec.program == projects::PROGRAM && spec.vm.is_empty() {
            return self.projects(id, out);
        }
        if !spec.shell && spec.program == LIST_PROGRAM && spec.vm.is_empty() {
            let owner = self.client.short_id();
            return Ok(self.server.sessions.open_list(&owner, id, out));
        }
        if !spec.session.is_empty() && (spec.pty.is_some() || spec.program == END_PROGRAM) {
            // An attach reaches the session's program, so the key must
            // still be allowed where it runs: the session only attaches
            // to opens for its own VM name. Ending one needs no access.
            if spec.shell || spec.program != END_PROGRAM {
                self.may_run(&spec.vm)?;
            }
            return self.server.sessions.open(
                &self.client.short_id(),
                id,
                spec,
                out,
                |spec, out| self.start(0, spec, out),
            );
        }
        self.start(id, spec, out)
    }
}

impl ConnectionBackend<'_> {
    /// Answers a [`projects::PROGRAM`] open. A key that may not run
    /// commands on the host learns nothing about it.
    fn projects(&self, id: u32, out: Outbox) -> Result<Box<dyn Channel>, String> {
        if !self.server.config.projects_for(&self.options) {
            return Err("this server shares no projects".into());
        }
        let shared = projects::list(&self.server.paths.app_state());
        Ok(projects::open(id, &shared, out))
    }

    /// Whether this key may run commands in `vm`, or on the host when it
    /// is empty.
    fn may_run(&self, vm: &str) -> Result<(), String> {
        if vm.is_empty() {
            if self.options.vm_only() {
                return Err("this key may only use VMs; add ?vm=<name>".into());
            }
            if !self.server.config.host_for(&self.options) {
                return Err("this server only offers VMs; add ?vm=<name>".into());
            }
        } else if self.server.vms.is_none() {
            return Err("this server does not offer VMs".into());
        }
        Ok(())
    }

    /// Starts `spec` on the host or in its VM.
    fn start(&self, id: u32, spec: &OpenSpec, out: Outbox) -> Result<Box<dyn Channel>, String> {
        self.may_run(&spec.vm)?;
        match &self.server.vms {
            Some(vms) if !spec.vm.is_empty() => Ok(VmChannel::open(
                vms.clone(),
                self.client.short_id(),
                id,
                spec,
                out,
            )),
            _ => exec::open_local(id, spec, out),
        }
    }
}

/// Shuts `stream` down when the handshake is not done in time.
fn handshake_deadline(stream: &TcpStream) -> io::Result<crate::transport::Deadline> {
    let expired = stream.try_clone()?;
    Ok(crate::transport::deadline(HANDSHAKE_TIMEOUT, move || {
        let _ = expired.shutdown(Shutdown::Both);
    }))
}

/// Shuts the connection of `stream` down, for [`mux::serve_closing`].
fn closer(stream: &TcpStream) -> io::Result<impl FnOnce() + Send + 'static> {
    let stream = stream.try_clone()?;
    Ok(move || {
        let _ = stream.shutdown(Shutdown::Both);
    })
}

pub(crate) fn log(message: &str) {
    eprintln!("cmdterm-remote: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_links_get_host_access_unless_vm_only_is_set() {
        let mut config = ServerConfig::default();
        assert!(config.host_for(&config.link_options()));
        config.web_links_vm_only = true;
        let options = config.link_options();
        assert!(options.vm_only());
        assert!(
            !config.host_for(&options),
            "a vm-only web link gets no host"
        );
        assert!(!config.projects_for(&options), "and shares no projects");
    }
}
