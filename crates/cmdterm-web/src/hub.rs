//! Linked machines: the port they connect to, and the channels the web app
//! opens on them.
//!
//! A machine dials in and logs in with its host key, as the client of the
//! usual handshake ([`cmdterm_remote::handshake`]). Only keys of linked
//! machines get in. After that this side opens channels like any client of
//! a `cmdterm-remote` server: shells in sessions, and the programs that list
//! projects and sessions. One thread per machine reads its frames and hands
//! each message to the channel it belongs to.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::Duration;

use cmdterm_remote::handshake;
use cmdterm_remote::keys::{Identity, KeyOptions};
use cmdterm_remote::mux;
use cmdterm_remote::projects::{self, SharedProject};
use cmdterm_remote::proto::{Msg, OpenSpec};
use cmdterm_remote::session::{END_PROGRAM, LIST_PROGRAM, SessionInfo};
use cmdterm_remote::transport::FrameRead;
use tokio::sync::mpsc;

use crate::now;
use crate::store::Store;

/// How long a machine may take to finish the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Most machines connected at once.
const MAX_LINKS: usize = 1024;
/// Messages queued for one channel before the machine's reader waits for
/// the browser, which pushes back on the program.
const CHANNEL_DEPTH: usize = 64;
/// How long listing projects or sessions may take.
const ASK_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Hub {
    identity: Identity,
    store: Arc<Store>,
    runtime: tokio::runtime::Handle,
    links: Mutex<HashMap<String, Arc<Link>>>,
    connections: AtomicUsize,
    /// Pre-handshake attempt limit per address, so unauthenticated callers
    /// cannot keep the post-quantum handshake busy.
    attempts: cmdterm_remote::rate::Attempts,
}

/// The connection of one machine.
pub struct Link {
    machine: String,
    out: std_mpsc::Sender<Msg>,
    channels: Mutex<HashMap<u32, mpsc::Sender<Msg>>>,
    next: AtomicU32,
    stream: TcpStream,
}

/// A channel on a machine. Dropping it closes the channel; a session's
/// program keeps running.
pub struct Channel {
    id: u32,
    link: Arc<Link>,
    pub rx: mpsc::Receiver<Msg>,
}

impl Channel {
    pub fn send(&self, msg: Msg) {
        let _ = self.link.out.send(msg.with_channel(self.id));
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.link.channels.lock().unwrap().remove(&self.id);
        let _ = self.link.out.send(Msg::Close(self.id));
    }
}

impl Link {
    pub fn open(self: &Arc<Self>, spec: OpenSpec) -> Channel {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        self.channels.lock().unwrap().insert(id, tx);
        let _ = self.out.send(Msg::Open(id, spec));
        Channel {
            id,
            link: self.clone(),
            rx,
        }
    }

    /// Runs `spec` to its end and returns its output. Fails on an exit
    /// other than 0, with what it wrote to stderr.
    pub async fn run(self: &Arc<Self>, spec: OpenSpec) -> Result<Vec<u8>, String> {
        let mut channel = self.open(spec);
        let collect = async {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            while let Some(msg) = channel.rx.recv().await {
                match msg {
                    Msg::Data(_, data) => stdout.extend(data),
                    Msg::Stderr(_, data) => stderr.extend(data),
                    Msg::Exit(_, 0) => return Ok(stdout),
                    Msg::Exit(_, code) => {
                        let message = String::from_utf8_lossy(&stderr).trim().to_owned();
                        return Err(if message.is_empty() {
                            format!("exited with {code}")
                        } else {
                            message
                        });
                    }
                    Msg::OpenFailed(_, message) => return Err(message),
                    _ => {}
                }
            }
            Err("the machine went offline".to_owned())
        };
        tokio::time::timeout(ASK_TIMEOUT, collect)
            .await
            .unwrap_or_else(|_| Err("the machine did not answer".into()))
    }
}

fn program(name: &str) -> OpenSpec {
    OpenSpec {
        program: name.into(),
        ..OpenSpec::default()
    }
}

impl Hub {
    pub fn new(
        identity: Identity,
        store: Arc<Store>,
        runtime: tokio::runtime::Handle,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity,
            store,
            runtime,
            links: Mutex::default(),
            connections: AtomicUsize::new(0),
            attempts: cmdterm_remote::rate::Attempts::new(),
        })
    }

    /// The key line machines pin when they link.
    pub fn key_line(&self) -> String {
        self.identity.public().to_line("")
    }

    /// Accepts machines on `address` in a background thread.
    pub fn listen(self: &Arc<Self>, address: &str) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind(address)?;
        let local = listener.local_addr()?;
        let hub = self.clone();
        std::thread::Builder::new()
            .name("cmdterm-web-links".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    // Refuse addresses that hammer the port before any
                    // handshake work is spent on them.
                    if let Some(ip) = stream.peer_addr().ok().map(|a| a.ip())
                        && !hub.attempts.allow(ip)
                    {
                        continue;
                    }
                    if hub.connections.fetch_add(1, Ordering::SeqCst) >= MAX_LINKS {
                        hub.connections.fetch_sub(1, Ordering::SeqCst);
                        continue;
                    }
                    let hub = hub.clone();
                    std::thread::spawn(move || {
                        let peer = stream.peer_addr().ok();
                        if let Err(error) = hub.connection(stream) {
                            eprintln!(
                                "cmdterm-web: machine at {}: {error}",
                                peer.map(|p| p.to_string()).unwrap_or_default()
                            );
                        }
                        hub.connections.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            })?;
        Ok(local)
    }

    fn connection(self: &Arc<Self>, stream: TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        cmdterm_remote::transport::keep_alive(&stream);
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let control = stream.try_clone()?;
        // The read timeout restarts with every byte, so a peer that
        // trickles its hello would hold the connection; the deadline
        // covers the whole handshake.
        let expired = stream.try_clone()?;
        let deadline = cmdterm_remote::transport::deadline(HANDSHAKE_TIMEOUT, move || {
            let _ = expired.shutdown(std::net::Shutdown::Both);
        });
        let session = handshake::server(stream, &self.identity, |key| {
            Ok(self
                .store
                .machine_by_key(key)
                .map(|_| KeyOptions::default()))
        })?;
        drop(deadline);
        control.set_read_timeout(None)?;
        let Some(machine) = self.store.machine_by_key(&session.peer) else {
            return Ok(());
        };
        let (out, queue) = std_mpsc::channel();
        mux::spawn_writer(session.writer, queue);
        let link = Arc::new(Link {
            machine: machine.id.clone(),
            out,
            channels: Mutex::default(),
            next: AtomicU32::new(1),
            stream: control,
        });
        let old = self
            .links
            .lock()
            .unwrap()
            .insert(machine.id.clone(), link.clone());
        // A machine that reconnects before its old connection timed out
        // takes over; the old one's channels end.
        if let Some(old) = old {
            let _ = old.stream.shutdown(Shutdown::Both);
        }
        self.seen(&machine.id);
        let hub = self.clone();
        let id = machine.id.clone();
        self.runtime.spawn(async move {
            let _ = hub.projects(&id).await;
            let _ = hub.sessions(&id).await;
        });

        let mut reader = session.reader;
        let mut buf = Vec::new();
        let result = loop {
            match reader.read_frame(&mut buf) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(error) => break Err(error),
            }
            let msg = match Msg::decode(&buf) {
                Ok(msg) => msg,
                Err(error) => break Err(error),
            };
            let sender = link.channels.lock().unwrap().get(&msg.channel()).cloned();
            if let Some(sender) = sender {
                // Waits while the browser is slow; a gone browser drops
                // the receiver and the message with it.
                let _ = sender.blocking_send(msg);
            }
        };
        {
            let mut links = self.links.lock().unwrap();
            if links
                .get(&link.machine)
                .is_some_and(|l| Arc::ptr_eq(l, &link))
            {
                links.remove(&link.machine);
            }
        }
        link.channels.lock().unwrap().clear();
        self.seen(&link.machine);
        result
    }

    fn seen(&self, machine: &str) {
        let _ = self.store.write(|d| {
            if let Some(m) = d.machines.iter_mut().find(|m| m.id == machine) {
                m.last_seen = Some(now());
            }
        });
    }

    pub fn link(&self, machine: &str) -> Option<Arc<Link>> {
        self.links.lock().unwrap().get(machine).cloned()
    }

    pub fn online(&self, machine: &str) -> bool {
        self.links.lock().unwrap().contains_key(machine)
    }

    /// Ends the connection of `machine`, after it was removed.
    pub fn disconnect(&self, machine: &str) {
        if let Some(link) = self.links.lock().unwrap().remove(machine) {
            let _ = link.stream.shutdown(Shutdown::Both);
        }
    }

    fn connected(&self, machine: &str) -> Result<Arc<Link>, String> {
        self.link(machine)
            .ok_or_else(|| "the machine is offline".to_owned())
    }

    /// Asks `machine` for its projects and keeps the answer.
    pub async fn projects(&self, machine: &str) -> Result<Vec<SharedProject>, String> {
        let output = self
            .connected(machine)?
            .run(program(projects::PROGRAM))
            .await?;
        let list: Vec<SharedProject> =
            serde_json::from_slice(&output).map_err(|error| error.to_string())?;
        let _ = self.store.write(|d| {
            if let Some(m) = d.machines.iter_mut().find(|m| m.id == machine) {
                m.projects = list.clone();
            }
        });
        Ok(list)
    }

    /// Asks `machine` for the sessions of this server and keeps the answer.
    pub async fn sessions(&self, machine: &str) -> Result<Vec<SessionInfo>, String> {
        let output = self.connected(machine)?.run(program(LIST_PROGRAM)).await?;
        let list: Vec<SessionInfo> =
            serde_json::from_slice(&output).map_err(|error| error.to_string())?;
        let _ = self.store.write(|d| {
            if let Some(m) = d.machines.iter_mut().find(|m| m.id == machine) {
                m.sessions = list.clone();
            }
        });
        Ok(list)
    }

    /// Ends the session `name` on `machine`.
    pub async fn end_session(&self, machine: &str, name: &str) -> Result<(), String> {
        let spec = OpenSpec {
            program: END_PROGRAM.into(),
            session: name.into(),
            ..OpenSpec::default()
        };
        match self.connected(machine)?.run(spec).await {
            Ok(_) => Ok(()),
            Err(_) => Err(format!("no session {name}")),
        }
    }
}
