//! The client side: direct connections, the per-server master, `exec`, and
//! `trust`.
//!
//! `exec` is what `Environment::Remote` runs for every command and shell. It
//! connects to a master over a Unix socket in [`crate::runtime_dir`], and
//! starts one if none is listening. The master holds the one authenticated
//! TCP connection to the server and maps each `exec` client to a channel on
//! it, like OpenSSH's `ControlMaster`. It exits after `PERSIST` without
//! clients, or when the server connection ends.
//!
//! An `exec` with a session ([`crate::session`]) does not end with the
//! connection. It reconnects, attaches to the session again and asks for
//! the output it has not shown yet, and sends what was typed meanwhile.
//! `SIGUSR1` tells it that its terminal is hidden and `SIGUSR2` that it
//! shows again; it passes that on as [`Msg::Batch`], so the server sends a
//! hidden terminal's output in fewer, larger messages.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use cmdterm_env::Environment;
use sha3::{Digest, Sha3_256};

use crate::Paths;
use crate::handshake::{self, Session};
use crate::keys::{self, HostTrust, PublicKey};
use crate::mux::spawn_writer;
use crate::proto::{Msg, OpenSpec};
use crate::sys::UnixStream;
use crate::transport::{FrameRead, FrameWrite, Plain};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a master waits without clients before it exits.
const PERSIST: Duration = Duration::from_secs(600);
/// How long `exec` waits for a new master to log in.
const MASTER_START_TIMEOUT: Duration = Duration::from_secs(40);
/// Output a master keeps for one `exec` that reads it slowly.
const ROUTE_LIMIT: usize = 16 << 20;

/// One `exec` client of a master: where the server's messages for its
/// channel go.
struct Route {
    queue: crate::queue::Sender<Msg>,
    /// The client's socket, shut down when the client is cut off.
    local: UnixStream,
    /// A terminal in a session, whose `exec` reconnects and resumes.
    session: bool,
}

type Routes = Mutex<HashMap<u32, Arc<Route>>>;

impl Route {
    /// Starts the thread that writes the server's messages to `local`.
    fn start(local: &UnixStream, session: bool) -> io::Result<Arc<Self>> {
        let (queue, rx) = crate::queue::bounded::<Msg>(ROUTE_LIMIT);
        let mut writer = Plain(local.try_clone()?);
        thread::spawn(move || {
            let mut buf = Vec::new();
            while let Some(msg) = rx.recv() {
                msg.encode(&mut buf);
                if writer.write_frame(&buf).is_err() {
                    break;
                }
            }
        });
        Ok(Arc::new(Self {
            queue,
            local: local.try_clone()?,
            session,
        }))
    }

    /// Queues `msg` for the client. When more than [`ROUTE_LIMIT`] waits,
    /// the master's reader waits for a command's client, which holds up
    /// the connection's other channels like a slow network would. A
    /// terminal's client is cut off instead: it reconnects and gets the
    /// output it missed from the session.
    fn deliver(&self, msg: Msg) {
        let cost = match &msg {
            Msg::Data(_, data) | Msg::Stderr(_, data) => data.len(),
            _ => 16,
        };
        let wait = self.session.then_some(Duration::ZERO);
        if self.queue.send(msg, cost, wait).is_err() {
            let _ = self.local.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Connects and logs in to `address`, trusting only keys in `known_hosts`.
pub fn connect(paths: &Paths, address: &str) -> io::Result<Session<TcpStream>> {
    let known_hosts = paths.known_hosts();
    connect_with(paths, address, |key| {
        match keys::host_trust(&known_hosts, address, key)? {
            HostTrust::Trusted => Ok(()),
            HostTrust::Unknown => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the key of {address} is not trusted yet ({}). \
                     Run `scriptschnell-term-remote trust {address}` and compare the fingerprint",
                    key.fingerprint()
                ),
            )),
            HostTrust::Changed => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the key of {address} CHANGED to {}. Someone may be intercepting the \
                     connection. If the server got a new key on purpose, remove its line \
                     from {} and trust it again",
                    key.fingerprint(),
                    known_hosts.display()
                ),
            )),
        }
    })
}

fn connect_with(
    paths: &Paths,
    address: &str,
    check_host: impl FnOnce(&PublicKey) -> io::Result<()>,
) -> io::Result<Session<TcpStream>> {
    let identity = paths.load_identity()?;
    let stream = tcp_connect(address)?;
    stream.set_nodelay(true)?;
    crate::transport::keep_alive(&stream);
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let control = stream.try_clone()?;
    let session = handshake::client(stream, &identity, check_host)?;
    control.set_read_timeout(None)?;
    Ok(session)
}

pub(crate) fn tcp_connect(address: &str) -> io::Result<TcpStream> {
    let mut last = io::Error::new(
        io::ErrorKind::NotFound,
        format!("{address} did not resolve"),
    );
    for addr in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(io::Error::new(
        last.kind(),
        format!("could not connect to {address}: {last}"),
    ))
}

/// What `trust` saw.
pub enum TrustOutcome {
    AlreadyTrusted,
    Added,
    Declined,
}

/// Fetches the server key of `address` and records it in `known_hosts` once
/// `confirm` accepts its fingerprint. The client key is never sent.
pub fn trust(
    paths: &Paths,
    address: &str,
    confirm: impl FnOnce(&PublicKey, HostTrust) -> bool,
) -> io::Result<TrustOutcome> {
    let (key, state) = fetch_host_key(paths, address)?;
    let known_hosts = paths.known_hosts();
    if state == HostTrust::Trusted {
        return Ok(TrustOutcome::AlreadyTrusted);
    }
    if state == HostTrust::Changed || !confirm(&key, state) {
        return Ok(TrustOutcome::Declined);
    }
    keys::trust_host(&known_hosts, address, &key)?;
    Ok(TrustOutcome::Added)
}

/// Fetches the server key of `address` and what `known_hosts` says about
/// it, without logging in. The client key is never sent.
pub fn fetch_host_key(paths: &Paths, address: &str) -> io::Result<(PublicKey, HostTrust)> {
    let seen: Arc<Mutex<Option<PublicKey>>> = Arc::default();
    let stash = seen.clone();
    let result = connect_with(paths, address, move |key| {
        *stash.lock().unwrap() = Some(key.clone());
        Err(io::Error::other("scriptschnell-term-remote: key fetched"))
    });
    let Some(key) = seen.lock().unwrap().take() else {
        return Err(result
            .err()
            .unwrap_or_else(|| io::Error::other("no key received")));
    };
    let state = keys::host_trust(&paths.known_hosts(), address, &key)?;
    Ok((key, state))
}

/// The control socket of the master for `address` under these paths.
pub fn master_socket(paths: &Paths, address: &str) -> PathBuf {
    let mut hasher = Sha3_256::new();
    hasher.update(paths.dir.as_os_str().as_encoded_bytes());
    hasher.update([0]);
    hasher.update(address.as_bytes());
    let digest = hasher.finalize();
    let name: String = digest[..10].iter().map(|b| format!("{b:02x}")).collect();
    crate::runtime_dir().join(format!("{name}.sock"))
}

/// Runs the master for `address`. Prints `ready` or `error: ...` on stdout
/// once, then serves until idle. Never writes to stdout or stderr afterwards,
/// because nobody reads them by then.
pub fn run_master(paths: &Paths, address: &str) -> io::Result<()> {
    // Ignore hangups from the terminal of the exec that started us.
    crate::sys::ignore_hangup();
    let socket = master_socket(paths, address);
    crate::create_runtime_dir()?;
    let lock = std::fs::File::create(socket.with_extension("lock"))?;
    lock.lock()?;
    if UnixStream::connect(&socket).is_ok() {
        println!("ready");
        return Ok(());
    }
    let session = match connect(paths, address) {
        Ok(session) => session,
        Err(error) => {
            println!("error: {error}");
            return Err(error);
        }
    };
    let _ = std::fs::remove_file(&socket);
    // Mode 0600 from creation, so no local user can drive this connection
    // even if the runtime directory is ever reachable.
    let listener = crate::sys::bind_private_socket(&socket)?;
    println!("ready");
    let _ = io::stdout().flush();

    let routes: Arc<Routes> = Arc::default();
    let (remote_tx, remote_rx) = mpsc::sync_channel::<Msg>(crate::exec::OUTBOX_DEPTH);
    spawn_writer(session.writer, remote_rx);

    let cleanup_socket = socket.clone();
    let reader_routes = routes.clone();
    let mut remote_reader = session.reader;
    thread::spawn(move || {
        let mut buf = Vec::new();
        while let Ok(true) = remote_reader.read_frame(&mut buf) {
            let Ok(msg) = Msg::decode(&buf) else { break };
            // Delivered without the lock, which a waiting route would hold.
            let route = reader_routes.lock().unwrap().get(&msg.channel()).cloned();
            if let Some(route) = route {
                route.deliver(msg);
            }
        }
        // The server is gone. Clients see their sockets close.
        let _ = std::fs::remove_file(&cleanup_socket);
        reader_routes.lock().unwrap().clear();
        thread::sleep(Duration::from_millis(200));
        std::process::exit(0);
    });

    let idle_routes = routes.clone();
    let idle_socket = socket.clone();
    let persist = std::env::var("CMDTERM_REMOTE_PERSIST_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(PERSIST, Duration::from_secs);
    thread::spawn(move || {
        let mut idle_since = Instant::now();
        loop {
            thread::sleep(Duration::from_secs(1).min(persist));
            if !idle_routes.lock().unwrap().is_empty() {
                idle_since = Instant::now();
            } else if idle_since.elapsed() >= persist {
                let _ = std::fs::remove_file(&idle_socket);
                std::process::exit(0);
            }
        }
    });

    let next_id = Arc::new(AtomicU32::new(1));
    for local in listener.incoming() {
        let Ok(local) = local else { continue };
        let routes = routes.clone();
        let remote_tx = remote_tx.clone();
        let next_id = next_id.clone();
        thread::spawn(move || {
            let _ = forward_client(local, &routes, &remote_tx, &next_id);
        });
    }
    Ok(())
}

/// Carries one exec client's channel between its Unix socket and the server.
fn forward_client(
    local: UnixStream,
    routes: &Routes,
    remote: &mpsc::SyncSender<Msg>,
    next_id: &AtomicU32,
) -> io::Result<()> {
    let mut reader = Plain(local.try_clone()?);
    let mut buf = Vec::new();
    if !reader.read_frame(&mut buf)? {
        return Ok(());
    }
    let Msg::Open(_, spec) = Msg::decode(&buf)? else {
        return Ok(());
    };
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let session = !spec.session.is_empty() && spec.pty.is_some();
    routes
        .lock()
        .unwrap()
        .insert(id, Route::start(&local, session)?);
    // Closes the socket; Windows' stand-in for it has nothing to close.
    #[cfg_attr(not(unix), allow(clippy::drop_non_drop))]
    drop(local);
    let result = (|| {
        remote
            .send(Msg::Open(id, spec))
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        while reader.read_frame(&mut buf)? {
            let msg = Msg::decode(&buf)?;
            if matches!(msg, Msg::Close(_)) {
                break;
            }
            remote
                .send(msg.with_channel(id))
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        }
        Ok(())
    })();
    let _ = remote.send(Msg::Close(id));
    routes.lock().unwrap().remove(&id);
    result
}

/// Connects to the master for `address`, starting one if needed.
pub fn master_connection(paths: &Paths, address: &str) -> io::Result<UnixStream> {
    helper_connection(
        paths,
        &master_socket(paths, address),
        &["master", "--target", address],
        &format!("{address}: login"),
    )
}

/// Connects to the background helper listening on `socket`. If none
/// listens, starts `scriptschnell-term-remote <args>`, which prints `ready` or
/// `error: ...` once it listens or gives up.
pub(crate) fn helper_connection(
    paths: &Paths,
    socket: &std::path::Path,
    args: &[&str],
    what: &str,
) -> io::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    let exe = std::env::current_exe()?;
    let mut command = Environment::Local.command(exe);
    if paths.single {
        command = command.arg("--config-dir").arg(&paths.config_dir);
    }
    let mut command = command.args(args).to_std();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Out of the terminal's process group, so closing the panel that
    // started it does not end the connection other panels share.
    crate::sys::own_process_group(&mut command);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    // Reap the master when it exits so it does not linger as a zombie.
    thread::spawn(move || {
        let _ = child.wait();
    });
    let line = rx
        .recv_timeout(MASTER_START_TIMEOUT)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, format!("{what} timed out")))?;
    let line = line.trim();
    if line != "ready" {
        let message = line.strip_prefix("error: ").unwrap_or(line);
        let message = if message.is_empty() {
            "could not start the background helper"
        } else {
            message
        };
        return Err(io::Error::other(message.to_owned()));
    }
    UnixStream::connect(socket)
}

/// Where an `exec` runs.
pub enum ExecTarget {
    /// A server, `host:port`.
    Remote(String),
    /// The local VM daemon. The VM name is in the spec.
    LocalVm,
}

impl std::fmt::Display for ExecTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote(address) => f.write_str(address),
            Self::LocalVm => f.write_str("local VMs"),
        }
    }
}

/// Options of one `exec`.
pub struct ExecOptions {
    pub target: ExecTarget,
    pub spec: OpenSpec,
    /// Unix sockets to forward while the command runs, as (path on the
    /// remote machine, path here). See [`crate::relay`].
    pub forwards: Vec<(String, PathBuf)>,
    /// Connect straight to the server instead of through a master.
    pub direct: bool,
}

/// Runs one command remotely, wired to this process's stdio. Returns the
/// exit code to exit with.
pub fn exec(paths: &Paths, mut options: ExecOptions) -> i32 {
    let result = if options.spec.session.is_empty() || options.spec.pty.is_none() {
        exec_inner(paths, &mut options)
    } else {
        exec_session(paths, &options)
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("scriptschnell-term-remote: {}: {error}", options.target);
            255
        }
    }
}

/// A connection to `target` that carries one channel.
fn channel_connection(
    paths: &Paths,
    target: &ExecTarget,
    direct: bool,
) -> io::Result<(Box<dyn FrameRead>, Box<dyn FrameWrite>)> {
    Ok(match target {
        ExecTarget::Remote(address) if direct => {
            let session = connect(paths, address)?;
            (Box::new(session.reader), Box::new(session.writer))
        }
        ExecTarget::Remote(address) => {
            let stream = master_connection(paths, address)?;
            (
                Box::new(Plain(stream.try_clone()?)),
                Box::new(Plain(stream)),
            )
        }
        ExecTarget::LocalVm => {
            let stream = crate::local::vmd_connection(paths)?;
            (
                Box::new(Plain(stream.try_clone()?)),
                Box::new(Plain(stream)),
            )
        }
    })
}

/// Starts the relays of `options.forwards`, each on a connection of its
/// own, and waits until each listens or gave up. A forward that fails
/// leaves the command without it.
fn start_forwards(paths: &Paths, options: &ExecOptions) {
    let mut waiting = Vec::new();
    for (inside, local) in &options.forwards {
        let Ok((reader, writer)) = channel_connection(paths, &options.target, options.direct)
        else {
            continue;
        };
        let (ready, listening) = mpsc::sync_channel(1);
        let (vm, inside, local) = (options.spec.vm.clone(), inside.clone(), local.clone());
        thread::spawn(move || crate::relay::forward(reader, writer, vm, inside, local, ready));
        waiting.push(listening);
    }
    let deadline = Instant::now() + crate::relay::READY_TIMEOUT;
    for listening in waiting {
        let _ = listening.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }
}

fn exec_inner(paths: &Paths, options: &mut ExecOptions) -> io::Result<i32> {
    let (mut reader, writer) = channel_connection(paths, &options.target, options.direct)?;
    start_forwards(paths, options);
    let writer = Arc::new(Mutex::new(writer));
    let send = {
        let writer = writer.clone();
        move |msg: Msg| {
            let mut buf = Vec::new();
            msg.encode(&mut buf);
            writer.lock().unwrap().write_frame(&buf)
        }
    };

    let pty = options.spec.pty.is_some();
    let _raw = if pty {
        options.spec.pty = Some(terminal_size().unwrap_or((80, 24)));
        RawMode::enter()
    } else {
        None
    };
    send(Msg::Open(0, options.spec.clone()))?;

    let stdin_send = send.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_send(Msg::Data(0, buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = stdin_send(Msg::Eof(0));
    });
    #[cfg(unix)]
    if pty
        && let Ok(mut signals) =
            signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
    {
        let resize_send = send.clone();
        thread::spawn(move || {
            for _ in signals.forever() {
                if let Some((cols, rows)) = terminal_size()
                    && resize_send(Msg::Resize(0, cols, rows)).is_err()
                {
                    break;
                }
            }
        });
    }

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut buf = Vec::new();
    while reader.read_frame(&mut buf)? {
        match Msg::decode(&buf)? {
            Msg::Data(_, data) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Msg::Stderr(_, data) => {
                stderr.write_all(&data)?;
                stderr.flush()?;
            }
            Msg::Exit(_, code) => {
                let _ = send(Msg::Close(0));
                return Ok(code);
            }
            Msg::OpenFailed(_, message) => {
                let _ = send(Msg::Close(0));
                return Err(io::Error::other(message));
            }
            _ => {}
        }
    }
    Err(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "connection lost",
    ))
}

/// Input typed while the connection is down, sent once it is back.
const MAX_PENDING: usize = 64 * 1024;
/// Longest wait between reconnect attempts.
const MAX_RETRY: Duration = Duration::from_secs(15);

/// The connection of a session's `exec`, while there is one.
#[derive(Default)]
struct Link {
    writer: Option<Box<dyn FrameWrite>>,
    /// Input that waits for the next connection.
    pending: Vec<u8>,
    buf: Vec<u8>,
    /// The server of this connection takes [`Msg::Batch`].
    batches: bool,
    /// The terminal is hidden: `SIGUSR1` set it, `SIGUSR2` cleared it.
    hidden: bool,
}

impl Link {
    /// False when there is no connection or the write failed.
    fn send(&mut self, msg: &Msg) -> bool {
        let Some(writer) = &mut self.writer else {
            return false;
        };
        msg.encode(&mut self.buf);
        if writer.write_frame(&self.buf).is_err() {
            self.writer = None;
            return false;
        }
        true
    }

    /// Tells the server whether the terminal is hidden, where it takes
    /// that.
    #[cfg(unix)]
    fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
        if self.batches {
            self.send(&Msg::Batch(0, hidden));
        }
    }

    fn input(&mut self, data: &[u8]) {
        if !self.send(&Msg::Data(0, data.to_vec())) {
            let room = MAX_PENDING.saturating_sub(self.pending.len());
            self.pending
                .extend_from_slice(&data[..data.len().min(room)]);
        }
    }
}

/// How one connection of a session's `exec` ended.
enum Step {
    Exit(i32),
    Fatal(io::Error),
    /// The connection failed or dropped; try again.
    Lost(String),
}

/// [`exec_inner`] for a session: survives lost connections.
fn exec_session(paths: &Paths, options: &ExecOptions) -> io::Result<i32> {
    let link = Arc::new(Mutex::new(Link::default()));
    // First, so the app's signals never meet their default action, which
    // ends the process.
    #[cfg(unix)]
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGUSR1,
        signal_hook::consts::SIGUSR2,
    ]) {
        let batch_link = link.clone();
        thread::spawn(move || {
            for signal in signals.forever() {
                let hidden = signal == signal_hook::consts::SIGUSR1;
                batch_link.lock().unwrap().set_hidden(hidden);
            }
        });
    }
    let _raw = RawMode::enter();
    let stdin_link = link.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 16 * 1024];
        // A PTY has no end of input to pass on.
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            stdin_link.lock().unwrap().input(&buf[..n]);
        }
    });
    #[cfg(unix)]
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]) {
        let resize_link = link.clone();
        thread::spawn(move || {
            for _ in signals.forever() {
                if let Some((cols, rows)) = terminal_size() {
                    resize_link
                        .lock()
                        .unwrap()
                        .send(&Msg::Resize(0, cols, rows));
                }
            }
        });
    }

    // Bytes of the session's output shown so far, once attached.
    let mut received: Option<u64> = None;
    // An open went out before. Until `Attached` arrived, it may have
    // started the program, which may have exited since.
    let mut opened = false;
    // Sent with every open of this run, so the server tells its exit from
    // that of an earlier program under the same name.
    let mut run = [0u8; 8];
    getrandom::fill(&mut run).map_err(io::Error::other)?;
    let run = Some(u64::from_be_bytes(run));
    let mut retry = Duration::from_secs(1);
    let mut shown_problem = String::new();
    loop {
        let step = match channel_connection(paths, &options.target, options.direct) {
            Err(error) => Step::Lost(error.to_string()),
            Ok((reader, writer)) => {
                start_forwards(paths, options);
                let mut spec = options.spec.clone();
                spec.pty = Some(terminal_size().unwrap_or((80, 24)));
                spec.resume = received;
                spec.exit_if_ended = opened && received.is_none();
                spec.run = run;
                // Even a failed write may have reached the server.
                opened = true;
                {
                    let mut link = link.lock().unwrap();
                    link.writer = Some(writer);
                    link.batches = false;
                    if link.send(&Msg::Open(0, spec)) {
                        let pending = std::mem::take(&mut link.pending);
                        if !pending.is_empty() {
                            link.input(&pending);
                        }
                    }
                }
                let step = session_connection(reader, &link, &mut received);
                link.lock().unwrap().writer = None;
                step
            }
        };
        match step {
            Step::Exit(code) => return Ok(code),
            Step::Fatal(error) => return Err(error),
            Step::Lost(problem) => {
                if received.is_some() && shown_problem.is_empty() {
                    retry = Duration::from_secs(1);
                }
                if problem != shown_problem {
                    // Goes to the terminal, outside the session's output.
                    let mut stderr = io::stderr();
                    let _ = write!(
                        stderr,
                        "\r\n\x1b[2m[{}: {problem}; reconnecting]\x1b[0m\r\n",
                        options.target
                    );
                    let _ = stderr.flush();
                    shown_problem = problem;
                }
                thread::sleep(retry);
                retry = (retry * 2).min(MAX_RETRY);
            }
        }
    }
}

/// Shows a session's output from one connection until it ends.
fn session_connection(
    mut reader: Box<dyn FrameRead>,
    link: &Mutex<Link>,
    received: &mut Option<u64>,
) -> Step {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut attached = false;
    let mut heard = false;
    let mut buf = Vec::new();
    let problem = loop {
        match reader.read_frame(&mut buf) {
            Ok(true) => {}
            Ok(false) => break "connection lost".to_owned(),
            Err(error) => break error.to_string(),
        }
        let msg = match Msg::decode(&buf) {
            Ok(msg) => msg,
            Err(error) => break error.to_string(),
        };
        heard = true;
        let shown = match msg {
            Msg::Data(_, data) => {
                if attached {
                    *received = Some(received.unwrap_or(0) + data.len() as u64);
                }
                stdout.write_all(&data).and_then(|()| stdout.flush())
            }
            Msg::Stderr(_, data) => stderr.write_all(&data).and_then(|()| stderr.flush()),
            Msg::Attached(_, offset, batches) => {
                attached = true;
                *received = Some(offset);
                let mut link = link.lock().unwrap();
                link.batches = batches;
                if batches && link.hidden {
                    link.send(&Msg::Batch(0, true));
                }
                Ok(())
            }
            Msg::Exit(_, code) => {
                link.lock().unwrap().send(&Msg::Close(0));
                return Step::Exit(code);
            }
            Msg::OpenFailed(_, message) => {
                link.lock().unwrap().send(&Msg::Close(0));
                return Step::Fatal(io::Error::other(message));
            }
            _ => Ok(()),
        };
        if let Err(error) = shown {
            return Step::Fatal(error);
        }
    };
    if heard && !attached {
        // A server from before sessions ran a plain channel, which ended
        // with the connection.
        return Step::Fatal(io::Error::new(io::ErrorKind::ConnectionAborted, problem));
    }
    Step::Lost(problem)
}

#[cfg(windows)]
fn terminal_size() -> Option<(u16, u16)> {
    None
}

#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let size = rustix::termios::tcgetwinsize(rustix::stdio::stdin()).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// Puts stdin's terminal in raw mode until dropped, so keys reach the remote
/// PTY unchanged, like `ssh -t`. Unix only: on Windows the console keeps
/// its mode.
struct RawMode {
    #[cfg(unix)]
    saved: rustix::termios::Termios,
}

impl RawMode {
    #[cfg(windows)]
    fn enter() -> Option<Self> {
        None
    }

    #[cfg(unix)]
    fn enter() -> Option<Self> {
        let stdin = rustix::stdio::stdin();
        if !rustix::termios::isatty(stdin) {
            return None;
        }
        let saved = rustix::termios::tcgetattr(stdin).ok()?;
        let mut raw = saved.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(stdin, rustix::termios::OptionalActions::Now, &raw).ok()?;
        Some(Self { saved })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = rustix::termios::tcsetattr(
            rustix::stdio::stdin(),
            rustix::termios::OptionalActions::Now,
            &self.saved,
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A terminal's `exec` that stops reading is cut off once too much
    /// waits for it, instead of holding up the master's reader.
    #[test]
    fn a_terminal_route_that_does_not_read_is_cut_off() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let route = Route::start(&ours, true).unwrap();
        drop(ours);
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            for _ in 0..(ROUTE_LIMIT >> 20) + 8 {
                route.deliver(Msg::Data(1, vec![b'x'; 1 << 20]));
            }
            let _ = done.send(route);
        });
        let _route = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the master waited for a terminal that does not read");
        theirs
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut got = Vec::new();
        let _ = theirs.read_to_end(&mut got);
        assert!(got.len() < ((ROUTE_LIMIT >> 20) + 8) << 20);
    }
}
