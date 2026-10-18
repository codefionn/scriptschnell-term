//! Sessions: PTY channels that outlive their connection.
//!
//! A client names a session in [`OpenSpec::session`]. The first open starts
//! the program; when the connection ends, or the client closes the channel,
//! the program keeps running and its output goes into a buffer. The next
//! open with the same name attaches to it again: the server sends
//! [`Msg::Attached`] with the offset the replay starts at, then the output
//! the client missed, and resizes the PTY so full-screen programs draw
//! again. A newer attach takes the session from an older one, so a client
//! that reconnects before the server noticed the old connection died does
//! not wait for it.
//!
//! Sessions belong to the client key that started them. One ends when its
//! program exits and the last output reached a client, when a client opens
//! [`END_PROGRAM`] with its name, or when it stayed detached longer than
//! the server allows. An open that resumes a session that is gone starts
//! nothing: it gets the session's exit when the server still knows it, else
//! it fails. A client that lost the connection before the exit reached it
//! then ends instead of running the program a second time.
//!
//! A client that lost the connection before [`Msg::Attached`] arrived cannot
//! tell whether its open reached the server. It opens again with
//! [`OpenSpec::exit_if_ended`]: a running session attaches, a kept exit is
//! sent and starts nothing, and otherwise the program starts. Only the exit
//! of the same run counts: the client sends one id with every open of a
//! run ([`OpenSpec::run`]), and the exit of an earlier program under the
//! same name has another id, or none.
//!
//! A client that does not show the terminal sends [`Msg::Batch`]. Until it
//! shows it again, the server holds output back for up to [`BATCH_DELAY`]
//! or until [`BATCH_BYTES`] wait, and sends it as one message. A phone or
//! laptop on a mobile network then wakes its radio every few seconds for a
//! busy terminal in the background, not for every line.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::exec::{Channel, OUTBOX_DEPTH, Outbox};
use crate::proto::{Msg, OpenSpec};

/// The program of a channel that ends the session it names instead of
/// running anything. It exits with 0 when the session ran, else 1.
pub const END_PROGRAM: &str = "cmdterm-remote:end-session";

/// The program of a channel that answers with the sessions of the client's
/// key as one JSON array ([`SessionInfo`]) instead of running anything.
pub const LIST_PROGRAM: &str = "cmdterm-remote:sessions";

/// One session, as [`LIST_PROGRAM`] lists it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub name: String,
    /// A client shows it right now.
    pub attached: bool,
    /// Its program ended; the session goes once a client saw the rest.
    pub exited: bool,
}

/// Output each session keeps for clients that attach later.
const BUFFER: usize = 1 << 20;
/// Output an attached client may lag behind before the program waits for
/// it, so a slow connection still pushes back on the program.
const HIGH_WATER: u64 = 256 * 1024;
/// Largest data message sent to a client.
const CHUNK: usize = 32 * 1024;
/// How long output waits for a client that does not show the terminal.
const BATCH_DELAY: Duration = Duration::from_secs(5);
/// Output that goes to such a client at once, in one message. Below
/// [`HIGH_WATER`], so a busy program does not wait for the delay.
const BATCH_BYTES: usize = 128 * 1024;
/// Sessions one client key may keep.
const MAX_SESSIONS: usize = 64;
/// Exits of ended sessions kept for clients that resume them later.
const ENDED_KEPT: usize = 256;
/// How often detached sessions are checked against the limit.
const REAP_EVERY: Duration = Duration::from_secs(60);
/// How long a detached session lives without a limit in `server.json`.
pub const DEFAULT_DETACHED: Duration = Duration::from_secs(7 * 24 * 60 * 60);

type Key = (String, String);

/// The sessions of one server or local VM daemon.
pub struct Sessions {
    map: Mutex<HashMap<Key, Arc<Session>>>,
    /// Names whose first open is starting the program right now. Other
    /// opens of the name wait on `started` instead of starting it again.
    /// Taken before `map` when both are held.
    starting: Mutex<HashSet<Key>>,
    started: Condvar,
    /// The exits of the newest ended sessions, `Exit` or `OpenFailed` on
    /// channel 0, with the run id of the open that started them.
    ended: Mutex<VecDeque<(Key, Msg, Option<u64>)>>,
    /// Detached sessions end after this long. `None` keeps them.
    detached_limit: Option<Duration>,
}

impl Sessions {
    /// `detached_limit` of `None` keeps detached sessions until their
    /// program exits.
    pub fn new(detached_limit: Option<Duration>) -> Arc<Self> {
        let sessions = Arc::new(Self {
            map: Mutex::default(),
            starting: Mutex::default(),
            started: Condvar::new(),
            ended: Mutex::default(),
            detached_limit,
        });
        if detached_limit.is_some() {
            let weak = Arc::downgrade(&sessions);
            thread::spawn(move || {
                loop {
                    thread::sleep(REAP_EVERY);
                    let Some(sessions) = weak.upgrade() else {
                        return;
                    };
                    sessions.reap();
                }
            });
        }
        sessions
    }

    /// Opens channel `id` of `owner` for `spec`, which names a session.
    /// Attaches to the session when it runs; otherwise `start` starts its
    /// program with an outbox of the session's own. A session attaches only
    /// to opens for the VM it runs in (`spec.vm`), so the caller decides
    /// whether the key may use it by checking that.
    pub fn open(
        self: &Arc<Self>,
        owner: &str,
        id: u32,
        spec: &OpenSpec,
        out: Outbox,
        start: impl FnOnce(&OpenSpec, Outbox) -> Result<Box<dyn Channel>, String>,
    ) -> Result<Box<dyn Channel>, String> {
        let key = (owner.to_owned(), spec.session.clone());
        if !spec.shell && spec.program == END_PROGRAM {
            let code = if self.end(&key) { 0 } else { 1 };
            let _ = out.send(Msg::Exit(id, code));
            return Ok(Box::new(Ended));
        }
        let session = match self.find_or_claim(&key) {
            Some(session) => session,
            None => {
                // Until this ends, other opens of the name wait.
                let _claim = Claim {
                    sessions: self,
                    key: &key,
                };
                if spec.resume.is_some() {
                    // The client saw this session, so it ended meanwhile.
                    return match self.exit_of(&key) {
                        Some((exit, _)) => {
                            let _ = out.send(exit.with_channel(id));
                            Ok(Box::new(Ended))
                        }
                        None => Err(format!("session {} no longer runs", spec.session)),
                    };
                }
                if spec.exit_if_ended
                    && let Some((exit, run)) = self.exit_of(&key)
                    && run == spec.run
                {
                    // An earlier open of this run started the program,
                    // which ended before the client heard of it.
                    let _ = out.send(exit.with_channel(id));
                    return Ok(Box::new(Ended));
                }
                self.make_room(owner)?;
                let mut inner_spec = spec.clone();
                inner_spec.session.clear();
                inner_spec.resume = None;
                inner_spec.exit_if_ended = false;
                inner_spec.run = None;
                let (inner_out, inner_rx) = mpsc::sync_channel(OUTBOX_DEPTH);
                let channel = start(&inner_spec, inner_out)?;
                let session = Arc::new(Session {
                    key: key.clone(),
                    vm: spec.vm.clone(),
                    run: spec.run,
                    sessions: Arc::downgrade(self),
                    state: Mutex::new(State {
                        output: Output::default(),
                        modes: Modes::default(),
                        client: None,
                        generation: 0,
                        exit: None,
                        detached_at: Some(Instant::now()),
                        size: spec.pty.unwrap_or((80, 24)),
                    }),
                    changed: Condvar::new(),
                    channel: Mutex::new(channel),
                });
                let pump = session.clone();
                thread::spawn(move || pump.pump(inner_rx));
                self.ended.lock().unwrap().retain(|(k, ..)| *k != key);
                self.map
                    .lock()
                    .unwrap()
                    .insert(key.clone(), session.clone());
                session
            }
        };
        if session.vm != spec.vm {
            return Err(format!(
                "session {} runs {}",
                spec.session,
                if session.vm.is_empty() {
                    "on the host".to_owned()
                } else {
                    format!("in the VM {}", session.vm)
                }
            ));
        }
        let generation = session.attach(id, spec, out);
        Ok(Box::new(Handle {
            session,
            generation,
        }))
    }

    /// The running session `key`, or `None` when the caller is to start
    /// it. The caller then holds the name's place in `starting` and must
    /// give it up with a [`Claim`], after it put the session into `map`.
    fn find_or_claim(&self, key: &Key) -> Option<Arc<Session>> {
        loop {
            if let Some(session) = self.map.lock().unwrap().get(key) {
                return Some(session.clone());
            }
            let mut starting = self.starting.lock().unwrap();
            // Again under `starting`: a start that finished since put its
            // session into the map before it gave up its place.
            if let Some(session) = self.map.lock().unwrap().get(key) {
                return Some(session.clone());
            }
            if starting.insert(key.clone()) {
                return None;
            }
            while starting.contains(key) {
                starting = self.started.wait(starting).unwrap();
            }
        }
    }

    /// The exit of the ended session `key` and the run id it started
    /// with, while it is kept.
    fn exit_of(&self, key: &Key) -> Option<(Msg, Option<u64>)> {
        let ended = self.ended.lock().unwrap();
        ended
            .iter()
            .find(|(k, ..)| k == key)
            .map(|(_, exit, run)| (exit.clone(), *run))
    }

    /// Keeps the exit of `session`, which has left the map.
    fn remember_exit(&self, session: &Session) {
        let Some(exit) = session.state().exit.clone() else {
            return;
        };
        let mut ended = self.ended.lock().unwrap();
        ended.retain(|(k, ..)| *k != session.key);
        if ended.len() >= ENDED_KEPT {
            ended.pop_front();
        }
        ended.push_back((session.key.clone(), exit, session.run));
    }

    /// Answers a [`LIST_PROGRAM`] open of `owner` on channel `id`.
    pub fn open_list(&self, owner: &str, id: u32, out: Outbox) -> Box<dyn Channel> {
        let json = serde_json::to_vec(&self.list(owner)).unwrap_or_else(|_| b"[]".to_vec());
        let _ = out.send(Msg::Data(id, json));
        let _ = out.send(Msg::Exit(id, 0));
        Box::new(Ended)
    }

    /// The sessions of `owner`, by name.
    pub fn list(&self, owner: &str) -> Vec<SessionInfo> {
        let map = self.map.lock().unwrap();
        let mut list: Vec<SessionInfo> = map
            .iter()
            .filter(|((o, _), _)| o == owner)
            .map(|((_, name), session)| {
                let state = session.state();
                SessionInfo {
                    name: name.clone(),
                    attached: state.client.is_some(),
                    exited: state.exit.is_some(),
                }
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Ends the session `key`. False when there was none.
    fn end(&self, key: &Key) -> bool {
        let session = self.map.lock().unwrap().remove(key);
        match session {
            Some(session) => {
                session.stop();
                self.remember_exit(&session);
                true
            }
            None => false,
        }
    }

    /// Ends the longest detached session of `owner` when it has too many.
    fn make_room(&self, owner: &str) -> Result<(), String> {
        let victim = {
            let map = self.map.lock().unwrap();
            let mine = map.iter().filter(|((o, _), _)| o == owner);
            if mine.clone().count() < MAX_SESSIONS {
                return Ok(());
            }
            mine.filter_map(|(key, session)| Some((session.state().detached_at?, key.clone())))
                .min()
                .map(|(_, key)| key)
        };
        match victim {
            Some(key) => {
                self.end(&key);
                Ok(())
            }
            None => Err(format!(
                "this key already runs {MAX_SESSIONS} attached sessions"
            )),
        }
    }

    /// Ends sessions detached for longer than the limit.
    fn reap(&self) {
        let Some(limit) = self.detached_limit else {
            return;
        };
        let expired: Vec<Key> = {
            let map = self.map.lock().unwrap();
            map.iter()
                .filter(|(_, session)| {
                    session
                        .state()
                        .detached_at
                        .is_some_and(|at| at.elapsed() >= limit)
                })
                .map(|(key, _)| key.clone())
                .collect()
        };
        for key in expired {
            self.end(&key);
        }
    }

    /// Ends every session. Call before the process exits.
    pub fn stop_all(&self) {
        let sessions: Vec<_> = self.map.lock().unwrap().drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.stop();
        }
    }

    /// Number of sessions, attached or not.
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A name's place in [`Sessions::starting`], given up when dropped.
struct Claim<'a> {
    sessions: &'a Sessions,
    key: &'a Key,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut starting = self
            .sessions
            .starting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        starting.remove(self.key);
        drop(starting);
        self.sessions.started.notify_all();
    }
}

struct Session {
    key: Key,
    /// The VM it runs in, empty on the host.
    vm: String,
    /// The run id of the open that started it, [`OpenSpec::run`].
    run: Option<u64>,
    sessions: std::sync::Weak<Sessions>,
    state: Mutex<State>,
    /// Signals new output, a new or gone client, the exit, and progress of
    /// the client the program waits for.
    changed: Condvar,
    channel: Mutex<Box<dyn Channel>>,
}

struct State {
    output: Output,
    modes: Modes,
    client: Option<Client>,
    /// Bumped by every attach and detach. Handles and forwarders of an
    /// older generation do nothing.
    generation: u64,
    /// `Exit` or `OpenFailed` on channel 0, once the program ended.
    exit: Option<Msg>,
    /// Since when no client is attached.
    detached_at: Option<Instant>,
    size: (u16, u16),
}

struct Client {
    id: u32,
    out: Outbox,
    /// Offset of the output sent to it so far.
    sent: u64,
    /// It does not show the output right now ([`Msg::Batch`]).
    batch: bool,
}

impl Session {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Makes channel `id` on `out` the session's client and starts sending
    /// it output. Returns its generation.
    fn attach(self: &Arc<Self>, id: u32, spec: &OpenSpec, out: Outbox) -> u64 {
        let mut state = self.state();
        state.generation += 1;
        let generation = state.generation;
        let (start, end) = (state.output.start, state.output.end());
        let from = match spec.resume {
            Some(offset) if (start..=end).contains(&offset) => offset,
            _ => start,
        };
        // The client misses output it cannot get back: what it shows is
        // stale, or the replay starts after the modes were set.
        let mut prefix = Vec::new();
        if spec.resume.is_some_and(|offset| offset != from) {
            prefix.extend_from_slice(b"\x1bc");
        }
        if from > 0 && spec.resume != Some(from) {
            state.modes.restore(&mut prefix);
        }
        state.client = Some(Client {
            id,
            out: out.clone(),
            sent: from,
            batch: false,
        });
        state.detached_at = None;
        let resize = match spec.pty {
            Some(size) if state.exit.is_none() => {
                let same = state.size == size;
                state.size = size;
                Some((size, same))
            }
            _ => None,
        };
        drop(state);
        self.changed.notify_all();
        // Sent without the state lock: a client that stops reading blocks
        // here, and must not hold up the session's program or the other
        // sessions. Only this attach's forwarder sends on the channel, and
        // it starts below.
        if !prefix.is_empty() {
            let _ = out.send(Msg::Data(id, prefix));
        }
        let _ = out.send(Msg::Attached(id, from, true));
        if let Some(((cols, rows), same)) = resize {
            let mut channel = self.channel.lock().unwrap();
            // The same size again would not reach the program, so step
            // through another one: it gets SIGWINCH and draws again.
            if same {
                channel.resize(cols, if rows > 1 { rows - 1 } else { rows + 1 });
            }
            channel.resize(cols, rows);
        }
        let forwarder = self.clone();
        thread::spawn(move || forwarder.forward(generation));
        generation
    }

    /// Detaches the client of `generation`, if it still is the client.
    fn detach(&self, generation: u64) {
        let mut state = self.state();
        if state.generation != generation {
            return;
        }
        state.generation += 1;
        state.client = None;
        state.detached_at = Some(Instant::now());
        drop(state);
        self.changed.notify_all();
    }

    /// Sends output and the exit to the client of `generation` until another
    /// client takes over, it goes away, or the session ended.
    fn forward(self: Arc<Self>, generation: u64) {
        // When held back output goes to a batching client at the latest.
        let mut due: Option<Instant> = None;
        loop {
            let mut state = self.state();
            let (id, out, chunk) = loop {
                if state.generation != generation {
                    return;
                }
                let end = state.output.end();
                let start = state.output.start;
                let exited = state.exit.is_some();
                let Some(client) = state.client.as_mut() else {
                    return;
                };
                // Output that wrapped out of the buffer while the client
                // lagged without the program waiting is gone.
                client.sent = client.sent.max(start);
                let (id, sent, batch) = (client.id, client.sent, client.batch);
                let held = (end - sent) as usize;
                if held > 0 && batch && !exited && held < BATCH_BYTES {
                    let now = Instant::now();
                    let at = *due.get_or_insert(now + BATCH_DELAY);
                    if now < at {
                        state = self
                            .changed
                            .wait_timeout(state, at - now)
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0;
                        continue;
                    }
                }
                if held > 0 {
                    due = None;
                    let limit = if batch { BATCH_BYTES } else { CHUNK };
                    let chunk = state.output.copy(sent, limit);
                    let client = state.client.as_mut().expect("checked above");
                    client.sent += chunk.len() as u64;
                    break (id, client.out.clone(), chunk);
                }
                let out = client.out.clone();
                if let Some(exit) = state.exit.clone() {
                    drop(state);
                    let _ = out.send(exit.with_channel(id));
                    // Queued is not delivered: the connection may drop
                    // first. A client that resumes then gets the exit kept.
                    if let Some(sessions) = self.sessions.upgrade() {
                        let mut map = sessions.map.lock().unwrap();
                        if map.get(&self.key).is_some_and(|s| Arc::ptr_eq(s, &self)) {
                            // Kept before the name goes, so a resume never
                            // finds neither.
                            sessions.remember_exit(&self);
                            map.remove(&self.key);
                        }
                    }
                    return;
                }
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            };
            drop(state);
            self.changed.notify_all();
            if out.send(Msg::Data(id, chunk)).is_err() {
                self.detach(generation);
                return;
            }
        }
    }

    /// Moves the program's output into the buffer until it exits.
    fn pump(self: Arc<Self>, rx: Receiver<Msg>) {
        let mut exit = Msg::Exit(0, 255);
        for msg in rx.iter() {
            match msg {
                Msg::Data(_, data) | Msg::Stderr(_, data) => {
                    let mut state = self.state();
                    // An attached client that lags far behind makes the
                    // program wait, as a plain channel would. A detached
                    // session only keeps the newest output.
                    while let Some(client) = &state.client
                        && state.output.end() - client.sent > HIGH_WATER
                    {
                        state = self
                            .changed
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    state.modes.scan(&data);
                    state.output.push(&data);
                    drop(state);
                    self.changed.notify_all();
                }
                Msg::Exit(_, code) => {
                    exit = Msg::Exit(0, code);
                    break;
                }
                Msg::OpenFailed(_, message) => {
                    exit = Msg::OpenFailed(0, message);
                    break;
                }
                _ => {}
            }
        }
        self.state().exit.get_or_insert(exit);
        self.changed.notify_all();
    }

    /// Kills the program. An attached client gets the exit after the
    /// output it has not seen yet.
    fn stop(&self) {
        {
            let mut state = self.state();
            if state.exit.is_none() {
                state.exit = Some(Msg::Exit(0, 128 + 9));
            }
        }
        self.changed.notify_all();
        self.channel.lock().unwrap().kill();
    }
}

/// A connection's view of a session. Closing it detaches.
struct Handle {
    session: Arc<Session>,
    generation: u64,
}

impl Handle {
    fn current(&self) -> bool {
        self.session.state().generation == self.generation
    }
}

impl Channel for Handle {
    fn input(&mut self, data: Vec<u8>) {
        if self.current() {
            self.session.channel.lock().unwrap().input(data);
        }
    }

    fn close_input(&mut self) {}

    fn resize(&mut self, cols: u16, rows: u16) {
        let mut state = self.session.state();
        if state.generation == self.generation && state.exit.is_none() {
            state.size = (cols, rows);
            self.session.channel.lock().unwrap().resize(cols, rows);
        }
    }

    fn kill(&mut self) {
        self.session.detach(self.generation);
    }

    fn batch(&mut self, on: bool) {
        let mut state = self.session.state();
        if state.generation != self.generation {
            return;
        }
        if let Some(client) = state.client.as_mut() {
            client.batch = on;
        }
        drop(state);
        // Output held back goes out now that the client shows it.
        self.session.changed.notify_all();
    }
}

/// The channel of an [`END_PROGRAM`] or [`LIST_PROGRAM`] open, which has
/// already exited.
struct Ended;

impl Channel for Ended {
    fn input(&mut self, _data: Vec<u8>) {}
    fn close_input(&mut self) {}
    fn resize(&mut self, _cols: u16, _rows: u16) {}
    fn kill(&mut self) {}
}

/// The newest [`BUFFER`] bytes of a session's output.
#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    /// Offset of the first byte in `bytes`.
    start: u64,
}

impl Output {
    fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        if self.bytes.len() <= BUFFER {
            return;
        }
        let mut drop = self.bytes.len() - BUFFER;
        // Start the kept output on a new line, so a replay does not begin
        // inside an escape sequence or a character.
        if let Some(newline) = self
            .bytes
            .range(drop..)
            .take(4096)
            .position(|&b| b == b'\n')
        {
            drop += newline + 1;
        }
        self.bytes.drain(..drop);
        self.start += drop as u64;
    }

    /// Up to `limit` bytes from offset `from`.
    fn copy(&self, from: u64, limit: usize) -> Vec<u8> {
        let skip = (from - self.start) as usize;
        self.bytes.range(skip..).take(limit).copied().collect()
    }
}

/// DEC private modes that change how the terminal shows or reads, and
/// that a replay of the newest output may no longer contain.
const MODES: [u16; 13] = [
    1, 25, 47, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1047, 1049, 2004,
];

/// The state of [`MODES`], read from the output as it passes.
#[derive(Default)]
struct Modes {
    /// Bit `i` set: `MODES[i]` was last set. Cursor visibility (25) is
    /// stored inverted, so the default of every mode is a cleared bit.
    set: u16,
    parser: ModeParser,
}

#[derive(Default)]
enum ModeParser {
    #[default]
    Ground,
    Escape,
    Csi,
    /// Inside `CSI ?` with the parameters read so far.
    Private {
        params: [u16; 8],
        count: usize,
    },
}

impl Modes {
    fn scan(&mut self, data: &[u8]) {
        for &byte in data {
            self.parser = match std::mem::take(&mut self.parser) {
                ModeParser::Ground | ModeParser::Escape | ModeParser::Csi if byte == 0x1b => {
                    ModeParser::Escape
                }
                ModeParser::Ground => ModeParser::Ground,
                ModeParser::Escape if byte == b'[' => ModeParser::Csi,
                ModeParser::Escape => ModeParser::Ground,
                ModeParser::Csi if byte == b'?' => ModeParser::Private {
                    params: [0; 8],
                    count: 1,
                },
                ModeParser::Csi => ModeParser::Ground,
                ModeParser::Private {
                    mut params,
                    mut count,
                } => match byte {
                    b'0'..=b'9' => {
                        let slot = &mut params[count - 1];
                        *slot = slot
                            .saturating_mul(10)
                            .saturating_add(u16::from(byte - b'0'));
                        ModeParser::Private { params, count }
                    }
                    b';' if count < params.len() => {
                        count += 1;
                        ModeParser::Private { params, count }
                    }
                    b'h' | b'l' => {
                        for param in &params[..count] {
                            self.apply(*param, byte == b'h');
                        }
                        ModeParser::Ground
                    }
                    0x1b => ModeParser::Escape,
                    _ => ModeParser::Ground,
                },
            };
        }
    }

    fn apply(&mut self, mode: u16, on: bool) {
        let Some(index) = MODES.iter().position(|&m| m == mode) else {
            return;
        };
        let bit = 1 << index;
        if on != (mode == 25) {
            self.set |= bit;
        } else {
            self.set &= !bit;
        }
    }

    /// Escapes that set every mode that differs from its default.
    fn restore(&self, out: &mut Vec<u8>) {
        for (index, &mode) in MODES.iter().enumerate() {
            if self.set & (1 << index) != 0 {
                let action = if mode == 25 { 'l' } else { 'h' };
                out.extend_from_slice(format!("\x1b[?{mode}{action}").as_bytes());
            }
        }
    }
}

// They drive `sh`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::exec;

    fn shell_spec(session: &str, script: &str) -> OpenSpec {
        OpenSpec {
            pty: Some((40, 10)),
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            session: session.into(),
            ..OpenSpec::default()
        }
    }

    fn open(
        sessions: &Arc<Sessions>,
        id: u32,
        spec: &OpenSpec,
    ) -> (Box<dyn Channel>, Receiver<Msg>) {
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let channel = sessions
            .open("me", id, spec, tx, |spec, out| {
                exec::open_local(9, spec, out)
            })
            .unwrap();
        (channel, rx)
    }

    /// Reads data until `needle` shows. Returns the offset of the first
    /// counted byte and the counted data.
    fn read_until(rx: &Receiver<Msg>, needle: &str) -> (Option<u64>, Vec<u8>) {
        let (mut offset, mut data) = (None, Vec::new());
        while !String::from_utf8_lossy(&data).contains(needle) {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Msg::Attached(_, from, _) => offset = Some(from),
                Msg::Data(_, bytes) if offset.is_some() => data.extend(bytes),
                Msg::Data(..) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        (offset, data)
    }

    #[test]
    fn a_session_outlives_its_channel_and_replays_what_was_missed() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("s1", "echo one; read l; echo two:$l; read l; echo three");
        let (mut first, rx) = open(&sessions, 1, &spec);
        let (from, data) = read_until(&rx, "one");
        assert_eq!(from, Some(0));
        let seen = data.len() as u64;
        first.kill();
        drop(rx);

        // Typed while detached would go nowhere, so attach, type, detach.
        let resume = OpenSpec {
            resume: Some(seen),
            ..spec.clone()
        };
        let (mut second, rx) = open(&sessions, 2, &resume);
        second.input(b"x\r".to_vec());
        let (from, data) = read_until(&rx, "two:x");
        assert_eq!(from, Some(seen));
        assert!(!String::from_utf8_lossy(&data).contains("one"));
        second.kill();
        drop(rx);
        assert_eq!(sessions.len(), 1);

        // A client without an offset gets everything still buffered.
        let fresh = OpenSpec {
            resume: None,
            ..spec.clone()
        };
        let (mut third, rx) = open(&sessions, 3, &fresh);
        let (from, data) = read_until(&rx, "two:x");
        assert_eq!(from, Some(0));
        assert!(String::from_utf8_lossy(&data).contains("one"));
        third.input(b"y\r".to_vec());
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
                Msg::Exit(id, code) => {
                    assert_eq!((id, code), (3, 0));
                    break;
                }
                Msg::Data(..) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        // The exit reached a client, so the session is gone.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sessions.is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_newer_attach_takes_the_session_over() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("s2", "read l; echo got:$l; sleep 30");
        let (mut old, _old_rx) = open(&sessions, 1, &spec);
        let (mut new, rx) = open(&sessions, 2, &spec);
        // The old connection's input no longer reaches the program, and its
        // close does not detach the new one.
        old.input(b"old\r".to_vec());
        old.kill();
        new.input(b"new\r".to_vec());
        let (_, data) = read_until(&rx, "got:new");
        let text = String::from_utf8_lossy(&data);
        assert!(!text.contains("old"), "{text}");
        sessions.stop_all();
    }

    #[test]
    fn ending_a_session_kills_it() {
        let sessions = Sessions::new(None);
        let (_channel, _rx) = open(&sessions, 1, &shell_spec("s3", "sleep 30"));
        let end = OpenSpec {
            program: END_PROGRAM.into(),
            session: "s3".into(),
            ..OpenSpec::default()
        };
        let (_, rx) = open(&sessions, 2, &end);
        assert!(matches!(rx.recv().unwrap(), Msg::Exit(2, 0)));
        assert!(sessions.is_empty());
        let (_, rx) = open(&sessions, 3, &end);
        assert!(matches!(rx.recv().unwrap(), Msg::Exit(3, 1)));
    }

    /// A client that hides the terminal gets output only after the delay,
    /// and at once when it shows the terminal again.
    #[test]
    fn a_hidden_client_gets_output_in_batches() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("b1", "echo ready; read l; echo got:$l; sleep 30");
        let (mut channel, rx) = open(&sessions, 1, &spec);
        read_until(&rx, "ready");
        channel.batch(true);
        channel.input(b"x\r".to_vec());
        let held = rx.recv_timeout(BATCH_DELAY / 2);
        assert!(held.is_err(), "sent before the delay: {held:?}");
        channel.batch(false);
        let mut data = Vec::new();
        while !String::from_utf8_lossy(&data).contains("got:x") {
            match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                Msg::Data(_, bytes) => data.extend(bytes),
                other => panic!("unexpected {other:?}"),
            }
        }
        sessions.stop_all();
    }

    #[test]
    fn sessions_belong_to_their_owner() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("same", "sleep 30");
        let (tx, _rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let mut started = 0;
        for owner in ["a", "b"] {
            sessions
                .open(owner, 1, &spec, tx.clone(), |spec, out| {
                    started += 1;
                    exec::open_local(9, spec, out)
                })
                .unwrap();
        }
        assert_eq!(started, 2);
        let listed = |owner| sessions.list(owner);
        assert_eq!(
            listed("a"),
            [SessionInfo {
                name: "same".into(),
                attached: true,
                exited: false,
            }]
        );
        assert!(listed("c").is_empty());
        let (out, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let _ = sessions.open_list("b", 4, out);
        let Msg::Data(4, json) = rx.recv().unwrap() else {
            panic!("no list");
        };
        let list: Vec<SessionInfo> = serde_json::from_slice(&json).unwrap();
        assert_eq!(list[0].name, "same");
        assert!(matches!(rx.recv().unwrap(), Msg::Exit(4, 0)));
        sessions.stop_all();
    }

    /// A client that stops reading blocks its own attach, not the other
    /// users of the session or of the server's sessions.
    #[test]
    fn an_attach_that_cannot_send_blocks_nobody_else() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("full", "sleep 30");
        let (_first, _rx) = open(&sessions, 1, &spec);
        // Nobody reads this outbox, so the attach blocks on its first
        // message.
        let (stuck, _never_read) = mpsc::sync_channel(0);
        let (attaching, stuck_spec) = (sessions.clone(), spec.clone());
        thread::spawn(move || {
            let _ = attaching.open("me", 2, &stuck_spec, stuck, |_, _| unreachable!());
        });
        thread::sleep(Duration::from_millis(200));
        let (done, listed) = mpsc::channel();
        let lister = sessions.clone();
        thread::spawn(move || {
            let _ = done.send(lister.list("me"));
        });
        let list = listed
            .recv_timeout(Duration::from_secs(5))
            .expect("listing waited for a stuck attach");
        assert_eq!(list.len(), 1);
        sessions.stop_all();
    }

    /// An exit that was queued but never reached the client is not a
    /// reason to run the program again when the client resumes.
    #[test]
    fn resuming_an_ended_session_gets_its_exit_and_starts_nothing() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("gone", "exit 3");
        let (_channel, rx) = open(&sessions, 1, &spec);
        while !matches!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Msg::Exit(1, 3)
        ) {}
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sessions.is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }

        let resume = OpenSpec {
            resume: Some(5),
            ..spec.clone()
        };
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let mut started = false;
        sessions
            .open("me", 2, &resume, tx, |_, _| {
                started = true;
                Err("started".into())
            })
            .unwrap();
        assert!(!started);
        assert!(matches!(rx.recv().unwrap(), Msg::Exit(2, 3)));

        // Nothing kept about the name: the open fails and says why.
        let unknown = OpenSpec {
            session: "never".into(),
            ..resume
        };
        let (tx, _rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let error = sessions
            .open("me", 3, &unknown, tx, |_, _| unreachable!())
            .err()
            .unwrap();
        assert!(error.contains("no longer runs"), "{error}");
        assert!(sessions.is_empty());
    }

    /// An open that may have reached the server before gets the exit of a
    /// session that ended since, when the same run started it, and starts
    /// the program otherwise.
    #[test]
    fn an_open_that_may_have_started_the_session_reports_its_exit() {
        let sessions = Sessions::new(None);
        // The first open reached the server, but its connection dropped
        // before `Attached` did, and the program exited meanwhile.
        let spec = OpenSpec {
            run: Some(1),
            ..shell_spec("lost", "exit 4")
        };
        let (_channel, rx) = open(&sessions, 1, &spec);
        while !matches!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            Msg::Exit(1, 4)
        ) {}
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sessions.is_empty() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }

        let again = OpenSpec {
            exit_if_ended: true,
            ..spec.clone()
        };
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let mut started = false;
        sessions
            .open("me", 2, &again, tx, |_, _| {
                started = true;
                Err("started".into())
            })
            .unwrap();
        assert!(!started, "the program ran a second time");
        assert!(matches!(rx.recv().unwrap(), Msg::Exit(2, 4)));

        // A new run under the same name whose first open never arrived:
        // the exit of the earlier run does not end it.
        let reused = OpenSpec {
            exit_if_ended: true,
            run: Some(2),
            ..shell_spec("lost", "echo second; sleep 30")
        };
        let (_second, rx) = open(&sessions, 3, &reused);
        read_until(&rx, "second");
        sessions.stop_all();

        // The first open never arrived: the program starts.
        let (_fresh, rx) = open(
            &sessions,
            5,
            &OpenSpec {
                exit_if_ended: true,
                ..shell_spec("new", "echo started; sleep 30")
            },
        );
        read_until(&rx, "started");

        // It runs: the open attaches to it.
        let again = OpenSpec {
            exit_if_ended: true,
            ..shell_spec("new", "echo started; sleep 30")
        };
        let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let _attached = sessions
            .open("me", 6, &again, tx, |_, _| unreachable!())
            .unwrap();
        read_until(&rx, "started");
        assert_eq!(sessions.len(), 1);
        sessions.stop_all();
    }

    /// Opens of a new name that race start its program once; the others
    /// attach to it.
    #[test]
    fn concurrent_first_opens_start_the_program_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let sessions = Sessions::new(None);
        let starts = Arc::new(AtomicUsize::new(0));
        let spec = shell_spec("race", "sleep 30");
        let threads: Vec<_> = (0..4)
            .map(|id| {
                let (sessions, starts, spec) = (sessions.clone(), starts.clone(), spec.clone());
                thread::spawn(move || {
                    let (tx, rx) = mpsc::sync_channel(OUTBOX_DEPTH);
                    let channel = sessions
                        .open("me", id, &spec, tx, |spec, out| {
                            starts.fetch_add(1, Ordering::SeqCst);
                            // A slow start, so the others arrive meanwhile.
                            thread::sleep(Duration::from_millis(200));
                            exec::open_local(9, spec, out)
                        })
                        .unwrap();
                    (channel, rx)
                })
            })
            .collect();
        let opened: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(sessions.len(), 1);
        drop(opened);
        sessions.stop_all();
    }

    /// A session runs where it started, so an open for another place does
    /// not reach it, whatever that open was allowed.
    #[test]
    fn a_session_attaches_only_to_opens_for_its_vm() {
        let sessions = Sessions::new(None);
        let spec = shell_spec("host", "sleep 30");
        let (_channel, _rx) = open(&sessions, 1, &spec);
        let in_vm = OpenSpec {
            vm: "ci".into(),
            ..spec.clone()
        };
        let (tx, _rx) = mpsc::sync_channel(OUTBOX_DEPTH);
        let error = sessions
            .open("me", 2, &in_vm, tx, |_, _| unreachable!())
            .err()
            .unwrap();
        assert!(error.contains("on the host"), "{error}");
        sessions.stop_all();
    }

    #[test]
    fn a_trimmed_replay_restores_modes_and_a_gap_resets() {
        let mut output = Output::default();
        let mut modes = Modes::default();
        let head = b"\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[?2004l\x1b[?1;1000h\n";
        modes.scan(head);
        output.push(head);
        let line = [b'x'; 1023];
        for _ in 0..BUFFER / 1024 + 4 {
            output.push(&line);
            output.push(b"\n");
        }
        assert!(output.start > 0);
        assert_eq!(output.bytes.len() % 1024, 0, "starts on a line");
        let mut prefix = Vec::new();
        modes.restore(&mut prefix);
        assert_eq!(prefix, b"\x1b[?1h\x1b[?25l\x1b[?1000h\x1b[?1049h");
        // Split across chunks, and switched back.
        let mut modes = Modes::default();
        modes.scan(b"\x1b[?10");
        modes.scan(b"49h");
        modes.scan(b"\x1b[?1049l");
        let mut prefix = Vec::new();
        modes.restore(&mut prefix);
        assert!(prefix.is_empty());
    }
}
