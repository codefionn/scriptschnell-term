//! The keyring bus: a D-Bus proxy that gives bubblewrap sandboxes the
//! Secret Service of this machine's session bus and nothing else of it.
//!
//! The session bus reaches every desktop service: systemd starts any
//! command on request, portals open files, and a sandbox that can own a
//! name can pose as a service. So sandboxes never see the real bus. They
//! get a socket at `/tmp/.cmdterm-bus` that [`KeyringBus`] serves, and
//! `DBUS_SESSION_BUS_ADDRESS` points there. Without it, programs that look
//! for a keyring, such as `gh`, start a `dbus-daemon` of their own.
//!
//! Each sandbox connection gets its own connection to the real bus, and
//! messages pass whole, one by one. From the sandbox the proxy passes:
//!
//! - replies, but no signals and no file descriptors;
//! - calls to the bus itself that look things up or subscribe to signals
//!   from the bus or the Secret Service, but none that own a name, start
//!   another service or change the bus;
//! - calls to the Secret Service (`org.freedesktop.secrets`, or the unique
//!   name that owns it) on its own interfaces. Reading, changing, creating
//!   or deleting a secret goes through the [`KEYRING_GUARD`]: in
//!   `Ask` mode the user answers one question per secret through
//!   [`Approvals`], named by the secret's label. Coding agents' logins
//!   ([`KeyringBus::set_logins`]) pass without a question, as their login
//!   files are writable in sandboxes: agents such as Antigravity CLI give
//!   the keyring a few seconds and then act as if logged out.
//!
//! Everything else gets an `AccessDenied` error. From the bus the proxy
//! passes everything: the bus delivers only replies, the signals the
//! sandbox subscribed to, and calls to its unique name.

use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use crate::proxy::{Approvals, GuardMode, GuardRule, KEYRING_GUARD, builtin_guards};

mod wire;

use wire::{Builder, Cursor, Header};

const BUS: &str = "org.freedesktop.DBus";
const SECRETS: &str = "org.freedesktop.secrets";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";
const PEER: &str = "org.freedesktop.DBus.Peer";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const SESSION: &str = "org.freedesktop.Secret.Session";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";

/// How long a lookup the proxy makes on its own waits for the bus.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// What a question about a secret asks for. [`crate::Approval::method`]
/// holds its [`Action::code`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Read,
    Change,
    Delete,
    /// A new secret in a collection, or a new collection.
    Create,
}

impl Action {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Change => "change",
            Self::Delete => "delete",
            Self::Create => "create",
        }
    }
}

/// Where the real session bus listens.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Address {
    Path(PathBuf),
    Abstract(Vec<u8>),
}

impl Address {
    /// The first Unix socket address of a D-Bus address list, such as
    /// `unix:path=/run/user/1000/bus`.
    fn parse(addresses: &str) -> Option<Self> {
        for address in addresses.split(';') {
            let Some(pairs) = address.strip_prefix("unix:") else {
                continue;
            };
            for pair in pairs.split(',') {
                match pair.split_once('=') {
                    Some(("path", value)) => {
                        let bytes = unescape(value)?;
                        return Some(Self::Path(PathBuf::from(String::from_utf8(bytes).ok()?)));
                    }
                    Some(("abstract", value)) => return Some(Self::Abstract(unescape(value)?)),
                    _ => {}
                }
            }
        }
        None
    }

    fn connect(&self) -> io::Result<UnixStream> {
        match self {
            Self::Path(path) => UnixStream::connect(path),
            Self::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
                UnixStream::connect_addr(&address)
            }
        }
    }
}

/// Undoes the `%XX` escapes of a D-Bus address value.
fn unescape(value: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hex = [bytes.next()?, bytes.next()?];
            out.push(u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?);
        } else {
            out.push(byte);
        }
    }
    Some(out)
}

/// A connection of the proxy's own, for the owner of the Secret Service
/// and the labels of secrets.
struct Lookup {
    stream: UnixStream,
    serial: u32,
    buf: Vec<u8>,
}

impl Lookup {
    fn connect(address: &Address, uid: u32) -> io::Result<Self> {
        let mut stream = address.connect()?;
        stream.set_read_timeout(Some(LOOKUP_TIMEOUT))?;
        wire::authenticate(&mut stream, uid)?;
        let mut lookup = Self {
            stream,
            serial: 0,
            buf: Vec::new(),
        };
        lookup.call(BUS, "/org/freedesktop/DBus", BUS, "Hello", &[])?;
        Ok(lookup)
    }

    /// Calls a method with string arguments and returns the reply's body
    /// offset; the reply is in `self.buf`.
    fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[&str],
    ) -> io::Result<usize> {
        self.serial += 1;
        let message = Builder::new(wire::METHOD_CALL, 0, self.serial)
            .field(1, "o", path)
            .field(2, "s", interface)
            .field(3, "s", member)
            .field(6, "s", destination)
            .strings(args);
        self.stream.write_all(&message)?;
        loop {
            wire::read_message(&mut self.stream, &mut self.buf)?;
            let header = wire::parse(&self.buf)?;
            if header.reply_serial != Some(self.serial) {
                continue;
            }
            return match header.kind {
                wire::METHOD_RETURN => Ok(header.body),
                _ => Err(io::Error::other(format!("{member} failed"))),
            };
        }
    }

    fn owner(&mut self) -> io::Result<String> {
        let body = self.call(
            BUS,
            "/org/freedesktop/DBus",
            BUS,
            "GetNameOwner",
            &[SECRETS],
        )?;
        Ok(Cursor::new(&self.buf, body).string()?.to_owned())
    }

    fn label(&mut self, path: &str, interface: &str) -> io::Result<String> {
        let body = self.call(SECRETS, path, PROPERTIES, "Get", &[interface, "Label"])?;
        Ok(Cursor::new(&self.buf, body).string_variant()?.to_owned())
    }

    fn attributes(&mut self, path: &str) -> io::Result<Vec<(String, String)>> {
        let body = self.call(SECRETS, path, PROPERTIES, "Get", &[ITEM, "Attributes"])?;
        let pairs = Cursor::new(&self.buf, body).string_map_variant()?;
        Ok((pairs.into_iter())
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect())
    }
}

struct Shared {
    address: Address,
    uid: u32,
    /// The answer to clients' `AUTH`, the same for every connection.
    guid: String,
    guard: GuardRule,
    mode: RwLock<GuardMode>,
    approvals: RwLock<Option<Arc<Approvals>>>,
    /// See [`KeyringBus::set_logins`].
    logins: RwLock<Vec<Vec<(String, String)>>>,
    lookup: Mutex<Option<Lookup>>,
    stop: AtomicBool,
}

impl Shared {
    /// Runs `f` on the lookup connection, connecting first when there is
    /// none and once more when it failed.
    fn lookup<T>(&self, mut f: impl FnMut(&mut Lookup) -> io::Result<T>) -> Option<T> {
        let mut lookup = self.lookup.lock().unwrap_or_else(|p| p.into_inner());
        for _ in 0..2 {
            if lookup.is_none() {
                *lookup = Lookup::connect(&self.address, self.uid).ok();
            }
            match lookup.as_mut().map(&mut f) {
                Some(Ok(value)) => return Some(value),
                Some(Err(_)) => *lookup = None,
                None => return None,
            }
        }
        None
    }

    fn mode(&self) -> GuardMode {
        *self.mode.read().unwrap_or_else(|p| p.into_inner())
    }

    /// Whether the user lets `question` pass, or why not.
    fn allows(&self, scope: Option<&str>, question: &Question) -> Result<(), &'static str> {
        match self.mode() {
            GuardMode::Allow => return Ok(()),
            GuardMode::Block => return Err("the cmdterm.keyring guard blocks secrets"),
            GuardMode::Ask => {}
        }
        if self.is_login(question) {
            return Ok(());
        }
        let approvals = self.approvals.read().ok().and_then(|a| a.clone());
        let Some(approvals) = approvals else {
            return Err("nobody can answer for secrets");
        };
        let Question {
            action,
            path,
            interface,
            ..
        } = *question;
        let label = (self.lookup(|lookup| lookup.label(path, interface)))
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| path.to_owned());
        if approvals.ask(&self.guard, scope, &label, action.code(), "") {
            Ok(())
        } else {
            Err("the user did not allow this keyring access")
        }
    }
}

impl Shared {
    /// Whether `question` is about an agent's login.
    fn is_login(&self, question: &Question) -> bool {
        let logins = || self.logins.read().unwrap_or_else(|p| p.into_inner());
        if logins().is_empty() {
            return false;
        }
        match &question.login {
            Login::No => false,
            Login::New(attributes) => login_matches(&logins(), attributes),
            Login::Item => {
                // The lookup may wait for the bus, so the logins stay
                // unlocked meanwhile.
                let attributes = self.lookup(|lookup| lookup.attributes(question.path));
                attributes.is_some_and(|attributes| {
                    let pairs: Vec<(&str, &str)> = (attributes.iter())
                        .map(|(key, value)| (key.as_str(), value.as_str()))
                        .collect();
                    login_matches(&logins(), &pairs)
                })
            }
        }
    }
}

/// Whether secret attributes `attributes` hold every attribute of one of
/// `logins`. Attributes that name a key twice never match: a service
/// keeps one of the values, and it may not be the one compared here.
fn login_matches(logins: &[Vec<(String, String)>], attributes: &[(&str, &str)]) -> bool {
    let once = (attributes.iter().enumerate())
        .all(|(at, (key, _))| attributes[..at].iter().all(|(other, _)| other != key));
    once && logins.iter().any(|login| {
        !login.is_empty()
            && (login.iter())
                .all(|(key, value)| attributes.contains(&(key.as_str(), value.as_str())))
    })
}

/// A running keyring bus. Dropping it stops it and removes its sockets.
pub struct KeyringBus {
    shared: Arc<Shared>,
    socket: PathBuf,
    /// The sockets of [`Self::socket_for`], by scope.
    scoped: Mutex<Vec<(String, PathBuf)>>,
}

/// How many connections one keyring socket serves at once. Each takes two
/// threads and a connection to the real bus, and a client that has not
/// authenticated holds its place for up to 30 seconds. Programs open one
/// or two, so a sandbox that opens more gets the next ones closed.
const MAX_CLIENTS: usize = 32;

/// Gives a place of [`MAX_CLIENTS`] back when dropped.
struct Place(Arc<AtomicUsize>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serves the connections of `listener` on threads of their own, with
/// `scope` for the questions they raise, at most [`MAX_CLIENTS`] at once.
fn listen(shared: &Arc<Shared>, listener: UnixListener, scope: Option<Arc<str>>) -> io::Result<()> {
    let accept = Arc::clone(shared);
    let live = Arc::new(AtomicUsize::new(0));
    thread::Builder::new()
        .name("cmdterm-keyring".into())
        .spawn(move || {
            for client in listener.incoming() {
                if accept.stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(client) = client else {
                    continue;
                };
                let place = Place(Arc::clone(&live));
                if live.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
                    // Dropping both closes the client and gives the place back.
                    continue;
                }
                let shared = Arc::clone(&accept);
                let scope = scope.clone();
                let _ = thread::Builder::new()
                    .name("cmdterm-keyring-client".into())
                    .spawn(move || {
                        let _place = place;
                        let _ = serve(&shared, client, scope);
                    });
            }
        })?;
    Ok(())
}

impl KeyringBus {
    /// Starts a keyring bus on a new socket at `socket` (an old one is
    /// replaced) for the session bus at `address`, the value of
    /// `DBUS_SESSION_BUS_ADDRESS`. Fails when the address names no Unix
    /// socket.
    pub fn start(socket: &Path, address: &str) -> io::Result<Self> {
        let address = Address::parse(address)
            .ok_or_else(|| io::Error::other("the session bus has no Unix socket"))?;
        let uid = std::fs::metadata("/proc/self")?.uid();
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let guid = random.iter().map(|b| format!("{b:02x}")).collect();
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)?;
        let shared = Arc::new(Shared {
            address,
            uid,
            guid,
            guard: (builtin_guards().into_iter())
                .find(|guard| guard.id == KEYRING_GUARD)
                .ok_or_else(|| io::Error::other("no keyring guard"))?,
            mode: RwLock::new(GuardMode::Ask),
            approvals: RwLock::new(None),
            logins: RwLock::new(Vec::new()),
            lookup: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        listen(&shared, listener, None)?;
        Ok(Self {
            shared,
            socket: socket.to_path_buf(),
            scoped: Mutex::new(Vec::new()),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// A socket whose connections carry `scope`, such as a project, into
    /// the questions they raise, so the user can allow a secret for good
    /// there. The first call for a scope opens it at `socket`.
    pub fn socket_for(&self, scope: &str, socket: &Path) -> io::Result<PathBuf> {
        let mut scoped = self
            .scoped
            .lock()
            .map_err(|_| io::Error::other("keyring state poisoned"))?;
        if let Some((_, path)) = scoped.iter().find(|(s, _)| s == scope) {
            return Ok(path.clone());
        }
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket)?;
        listen(&self.shared, listener, Some(Arc::from(scope)))?;
        scoped.push((scope.to_owned(), socket.to_path_buf()));
        Ok(socket.to_path_buf())
    }

    /// The user's mode for [`KEYRING_GUARD`].
    pub fn set_mode(&self, mode: GuardMode) {
        *self.shared.mode.write().unwrap_or_else(|p| p.into_inner()) = mode;
    }

    /// The secrets coding agents keep their logins in, each by attributes
    /// it has, such as `service` and `username`. In `Ask` mode a sandbox
    /// reads and changes a secret that has every attribute of one of them,
    /// and creates one, without a question, as it may write the agents'
    /// login files. Deleting one, or changing its attributes or label,
    /// still asks.
    pub fn set_logins(&self, logins: Vec<Vec<(String, String)>>) {
        *self
            .shared
            .logins
            .write()
            .unwrap_or_else(|p| p.into_inner()) = logins;
    }

    /// Where questions in `Ask` mode go. Without it secrets stay closed.
    pub fn set_approvals(&self, approvals: Option<Arc<Approvals>>) {
        *self
            .shared
            .approvals
            .write()
            .unwrap_or_else(|p| p.into_inner()) = approvals;
    }
}

impl Drop for KeyringBus {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the accept loops so they see the flag.
        let scoped = self
            .scoped
            .get_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        for socket in std::iter::once(&self.socket).chain(scoped.iter().map(|(_, s)| s)) {
            let _ = UnixStream::connect(socket);
            let _ = std::fs::remove_file(socket);
        }
    }
}

/// What happens to a message from the sandbox.
#[derive(Debug, PartialEq, Eq)]
enum Verdict<'a> {
    Pass,
    /// Dropped; an error answers it when the caller waits for a reply.
    Refuse(&'static str),
    /// Passes when the user allows each question.
    Ask(Vec<Question<'a>>),
}

/// A question about one secret or collection.
#[derive(Debug, PartialEq, Eq)]
struct Question<'a> {
    action: Action,
    /// The secret or collection the call goes to.
    path: &'a str,
    /// The interface whose label names it.
    interface: &'static str,
    login: Login<'a>,
}

/// How a question can turn out to be about an agent's login, which passes
/// without one.
#[derive(Debug, PartialEq, Eq)]
enum Login<'a> {
    /// It cannot.
    No,
    /// By the attributes of the secret at the question's path.
    Item,
    /// By the attributes of the secret the call creates.
    New(Vec<(&'a str, &'a str)>),
}

/// One sandbox connection.
struct Session {
    /// The unique name that owned the Secret Service when last asked.
    owner: Option<String>,
    /// Serials of the errors the proxy writes itself.
    serial: u32,
}

impl Session {
    /// Whether `destination` is the Secret Service, by its well-known or
    /// its unique name.
    fn is_secrets(&mut self, shared: &Shared, destination: &str) -> bool {
        if destination == SECRETS {
            return true;
        }
        if !destination.starts_with(':') {
            return false;
        }
        if self.owner.as_deref() != Some(destination) {
            self.owner = shared.lookup(Lookup::owner);
        }
        self.owner.as_deref() == Some(destination)
    }

    fn judge<'a>(
        &mut self,
        shared: &Shared,
        header: &Header<'a>,
        message: &'a [u8],
    ) -> io::Result<Verdict<'a>> {
        if header.fds {
            return Ok(Verdict::Refuse("file descriptors are not passed"));
        }
        match header.kind {
            wire::METHOD_RETURN | wire::ERROR => return Ok(Verdict::Pass),
            wire::METHOD_CALL => {}
            _ => return Ok(Verdict::Refuse("sandboxes send no signals")),
        }
        let (Some(destination), Some(member), Some(path)) =
            (header.destination, header.member, header.path)
        else {
            return Ok(Verdict::Refuse("calls need a destination"));
        };
        let mut body = Cursor::new(message, header.body);
        if destination == BUS {
            return Ok(match (header.interface.unwrap_or(BUS), member) {
                (
                    BUS,
                    "Hello"
                    | "GetNameOwner"
                    | "NameHasOwner"
                    | "ListNames"
                    | "ListActivatableNames"
                    | "GetId"
                    | "RemoveMatch",
                )
                | (PEER, _)
                | (INTROSPECTABLE, "Introspect")
                | (PROPERTIES, "Get" | "GetAll") => Verdict::Pass,
                (BUS, "AddMatch") if header.signature == "s" => {
                    let rule = body.string()?;
                    if self.subscribes_to_keyring(shared, rule) {
                        Verdict::Pass
                    } else {
                        Verdict::Refuse("signals only from the bus or the keyring")
                    }
                }
                (BUS, "StartServiceByName") if header.signature == "su" => {
                    if body.string()? == SECRETS {
                        Verdict::Pass
                    } else {
                        Verdict::Refuse("only the keyring may be started")
                    }
                }
                _ => Verdict::Refuse("this call to the bus is not allowed in a sandbox"),
            });
        }
        if !self.is_secrets(shared, destination) {
            return Ok(Verdict::Refuse(
                "sandboxes reach only the keyring on this bus",
            ));
        }
        let Some(interface) = header.interface else {
            return Ok(Verdict::Refuse("calls to the keyring need an interface"));
        };
        let ask = |action, interface, login| {
            Ok(Verdict::Ask(vec![Question {
                action,
                path,
                interface,
                login,
            }]))
        };
        match (interface, member) {
            (SERVICE, "OpenSession" | "SearchItems" | "Unlock" | "Lock" | "ReadAlias")
            | (COLLECTION, "SearchItems")
            | (SESSION, "Close")
            | (PROMPT, "Prompt" | "Dismiss")
            | (PROPERTIES, "Get" | "GetAll")
            | (INTROSPECTABLE, "Introspect")
            | (PEER, _) => Ok(Verdict::Pass),
            (SERVICE, "GetSecrets") if header.signature == "aoo" => {
                let items = body.strings()?;
                Ok(Verdict::Ask(
                    (items.into_iter())
                        .map(|item| Question {
                            action: Action::Read,
                            path: item,
                            interface: ITEM,
                            login: Login::Item,
                        })
                        .collect(),
                ))
            }
            (ITEM, "GetSecret") => ask(Action::Read, ITEM, Login::Item),
            (ITEM, "SetSecret") => ask(Action::Change, ITEM, Login::Item),
            (ITEM, "Delete") => ask(Action::Delete, ITEM, Login::No),
            (COLLECTION, "CreateItem") => {
                let attributes = if header.signature.starts_with("a{sv}") {
                    body.item_attributes()?
                } else {
                    None
                };
                ask(
                    Action::Create,
                    COLLECTION,
                    attributes.map_or(Login::No, Login::New),
                )
            }
            (COLLECTION, "Delete") => ask(Action::Delete, COLLECTION, Login::No),
            (SERVICE, "CreateCollection" | "SetAlias") => ask(Action::Create, SERVICE, Login::No),
            (PROPERTIES, "Set") if header.signature.starts_with("ss") => match body.string()? {
                ITEM => ask(Action::Change, ITEM, Login::No),
                COLLECTION => ask(Action::Change, COLLECTION, Login::No),
                _ => Ok(Verdict::Refuse("unknown keyring property")),
            },
            _ => Ok(Verdict::Refuse(
                "this keyring call is not allowed in a sandbox",
            )),
        }
    }

    /// Whether match `rule` only covers signals of the bus or the Secret
    /// Service, so a sandbox cannot listen to other programs.
    /// The rule is read the way dbus-daemon reads it. One that sets
    /// `eavesdrop` at all, names a key twice or a key dbus does not know,
    /// or does not parse, is refused.
    fn subscribes_to_keyring(&mut self, shared: &Shared, rule: &str) -> bool {
        let Some(keys) = match_rule_keys(rule) else {
            return false;
        };
        let mut sender = None;
        for (key, value) in &keys {
            if key == "eavesdrop" || !known_match_key(key) {
                return false;
            }
            if keys.iter().filter(|(other, _)| other == key).count() > 1 {
                return false;
            }
            if key == "sender" {
                sender = Some(value.as_str());
            }
        }
        sender.is_some_and(|sender| sender == BUS || self.is_secrets(shared, sender))
    }

    /// The error that answers a refused call. It comes from the bus, as
    /// dbus-daemon's own refusals do: a sender field must be a unique name
    /// or the bus, and zbus clients such as Codex panic on a well-known
    /// name there.
    fn refusal(&mut self, header: &Header, reason: &str) -> Vec<u8> {
        self.serial = self.serial.wrapping_add(1).max(1);
        let message = format!("cmdterm sandbox: {reason}");
        Builder::new(
            wire::ERROR,
            wire::NO_REPLY_EXPECTED,
            0x8000_0000 | self.serial,
        )
        .field(4, "s", ACCESS_DENIED)
        .field_u32(5, header.serial)
        .field(7, "s", BUS)
        .strings(&[&message])
    }
}

/// The keys and values of D-Bus match rule `rule`, read as dbus-daemon's
/// `bus_match_rule_parse` reads them: whitespace before a key and around
/// its `=` is skipped, a value runs to the next `,` outside quotes, `'`
/// quotes, and a backslash outside quotes escapes only a quote. `None`
/// for a key without `=` or unbalanced quotes, which the bus refuses too.
fn match_rule_keys(rule: &str) -> Option<Vec<(String, String)>> {
    let white = |c: char| c.is_ascii_whitespace();
    let mut keys = Vec::new();
    let mut rest = rule;
    loop {
        rest = rest.trim_start_matches(white);
        let end = rest
            .find(|c: char| c == '=' || white(c))
            .unwrap_or(rest.len());
        let key = &rest[..end];
        rest = rest[end..].trim_start_matches(white);
        if key.is_empty() {
            // An empty rule, or whitespace at its end.
            return rest.is_empty().then_some(keys);
        }
        rest = rest.strip_prefix('=')?;
        let mut value = String::new();
        let mut quote = None;
        let mut next = rest.len();
        for (at, c) in rest.char_indices() {
            match quote {
                None if c == ',' => {
                    next = at + 1;
                    break;
                }
                None if c == '\'' || c == '\\' => quote = Some(c),
                None => value.push(c),
                Some('\\') => {
                    if c != '\'' {
                        value.push('\\');
                    }
                    value.push(c);
                    quote = None;
                }
                Some(_) if c == '\'' => quote = None,
                Some(_) => value.push(c),
            }
        }
        match quote {
            Some('\\') => value.push('\\'),
            Some(_) => return None,
            None => {}
        }
        keys.push((key.to_owned(), value));
        rest = &rest[next..];
        if rest.is_empty() {
            return Some(keys);
        }
    }
}

/// Whether dbus-daemon knows match rule key `key`.
fn known_match_key(key: &str) -> bool {
    if let Some(arg) = key.strip_prefix("arg") {
        let digits = match arg {
            "0namespace" => "0",
            _ => arg.strip_suffix("path").unwrap_or(arg),
        };
        return (1..=2).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit())
            && digits.parse::<u8>().is_ok_and(|n| n < 64);
    }
    matches!(
        key,
        "type"
            | "sender"
            | "interface"
            | "member"
            | "path"
            | "path_namespace"
            | "destination"
            | "eavesdrop"
    )
}

fn serve(shared: &Arc<Shared>, mut client: UnixStream, scope: Option<Arc<str>>) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    wire::accept(&mut client, &shared.guid)?;
    client.set_read_timeout(None)?;
    let mut bus = shared.address.connect()?;
    wire::authenticate(&mut bus, shared.uid)?;
    // Both directions write whole messages to the client, one at a time.
    let to_client = Arc::new(Mutex::new(client.try_clone()?));
    let replies = {
        let to_client = Arc::clone(&to_client);
        let mut from_bus = bus.try_clone()?;
        let client = client.try_clone()?;
        thread::spawn(move || {
            let mut buf = Vec::new();
            while wire::read_message(&mut from_bus, &mut buf).is_ok() {
                let mut out = to_client.lock().unwrap_or_else(|p| p.into_inner());
                if out.write_all(&buf).is_err() {
                    break;
                }
            }
            let _ = client.shutdown(std::net::Shutdown::Both);
        })
    };
    let mut session = Session {
        owner: None,
        serial: 0,
    };
    let mut buf = Vec::new();
    while wire::read_message(&mut client, &mut buf).is_ok() {
        let Ok(header) = wire::parse(&buf) else {
            break;
        };
        let verdict = match session.judge(shared, &header, &buf) {
            Ok(Verdict::Ask(questions)) => (questions.iter())
                .try_for_each(|question| shared.allows(scope.as_deref(), question))
                .map_or_else(Verdict::Refuse, |()| Verdict::Pass),
            Ok(verdict) => verdict,
            Err(_) => Verdict::Refuse("malformed arguments"),
        };
        match verdict {
            Verdict::Pass => {
                if bus.write_all(&buf).is_err() {
                    break;
                }
            }
            Verdict::Refuse(reason) if header.wants_reply() => {
                let error = session.refusal(&header, reason);
                let mut out = to_client.lock().unwrap_or_else(|p| p.into_inner());
                if out.write_all(&error).is_err() {
                    break;
                }
            }
            _ => {}
        }
    }
    let _ = bus.shutdown(std::net::Shutdown::Both);
    let _ = replies.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    use super::*;

    #[test]
    fn bus_addresses_name_their_socket() {
        assert_eq!(
            Address::parse("unix:path=/run/user/1000/bus"),
            Some(Address::Path("/run/user/1000/bus".into()))
        );
        assert_eq!(
            Address::parse("tcp:host=x;unix:abstract=/tmp/dbus-a%2cb,guid=00"),
            Some(Address::Abstract(b"/tmp/dbus-a,b".to_vec()))
        );
        assert_eq!(Address::parse("tcp:host=localhost,port=1"), None);
    }

    /// A private session bus, killed on drop.
    struct TestBus {
        child: Child,
        address: String,
    }

    impl Drop for TestBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn test_bus(dir: &Path) -> Option<TestBus> {
        let config = dir.join("bus.conf");
        let socket = dir.join("bus.sock");
        std::fs::write(
            &config,
            format!(
                "<busconfig><type>session</type><listen>unix:path={}</listen>\
                 <auth>EXTERNAL</auth><policy context=\"default\">\
                 <allow send_destination=\"*\" eavesdrop=\"true\"/>\
                 <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
                socket.display()
            ),
        )
        .ok()?;
        let mut child = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut line)
            .ok()?;
        Some(TestBus {
            child,
            address: line.trim().to_owned(),
        })
    }

    /// A client of `socket`, authenticated and said hello to.
    struct Client {
        stream: UnixStream,
        serial: u32,
        buf: Vec<u8>,
    }

    impl Client {
        fn connect(socket: &Path) -> Self {
            let mut stream = UnixStream::connect(socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let uid = std::fs::metadata("/proc/self").unwrap().uid();
            wire::authenticate(&mut stream, uid).unwrap();
            let mut client = Self {
                stream,
                serial: 0,
                buf: Vec::new(),
            };
            let (kind, _) = client.call(BUS, "/org/freedesktop/DBus", BUS, "Hello", &[]);
            assert_eq!(kind, wire::METHOD_RETURN);
            client
        }

        /// Calls and returns the reply's kind and first string, if any.
        fn call(
            &mut self,
            destination: &str,
            path: &str,
            interface: &str,
            member: &str,
            args: &[&str],
        ) -> (u8, String) {
            self.serial += 1;
            let message = Builder::new(wire::METHOD_CALL, 0, self.serial)
                .field(1, "o", path)
                .field(2, "s", interface)
                .field(3, "s", member)
                .field(6, "s", destination)
                .strings(args);
            self.send(&message)
        }

        /// [`Self::call`] with a body of signature `signature`.
        fn call_with(
            &mut self,
            path: &str,
            interface: &str,
            member: &str,
            signature: &str,
            body: &[u8],
        ) -> (u8, String) {
            self.serial += 1;
            let message = Builder::new(wire::METHOD_CALL, 0, self.serial)
                .field(1, "o", path)
                .field(2, "s", interface)
                .field(3, "s", member)
                .field(6, "s", SECRETS)
                .finish(signature, body);
            self.send(&message)
        }

        fn send(&mut self, message: &[u8]) -> (u8, String) {
            self.stream.write_all(message).unwrap();
            loop {
                wire::read_message(&mut self.stream, &mut self.buf).unwrap();
                let header = wire::parse(&self.buf).unwrap();
                if header.reply_serial == Some(self.serial) {
                    let first = if header.signature.starts_with('s') {
                        Cursor::new(&self.buf, header.body)
                            .string()
                            .unwrap()
                            .to_owned()
                    } else {
                        String::new()
                    };
                    return (header.kind, first);
                }
            }
        }
    }

    /// A Secret Service that owns its name on `address`, gives every item
    /// the label `GitHub token` and answers `GetSecret` with `hunter2`.
    /// Items under `/agent/` have the attributes `service=agent` and
    /// `user=login`, others `service=github`.
    fn fake_keyring(address: &str) {
        let address = Address::parse(address).unwrap();
        let mut client =
            Lookup::connect(&address, std::fs::metadata("/proc/self").unwrap().uid()).unwrap();
        // RequestName(s name, u flags).
        let mut body = (SECRETS.len() as u32).to_le_bytes().to_vec();
        body.extend_from_slice(SECRETS.as_bytes());
        body.extend_from_slice(&[0; 5]);
        let request = Builder::new(wire::METHOD_CALL, 0, 999)
            .field(1, "o", "/org/freedesktop/DBus")
            .field(2, "s", BUS)
            .field(3, "s", "RequestName")
            .field(6, "s", BUS)
            .finish("su", &body);
        client.stream.write_all(&request).unwrap();
        loop {
            wire::read_message(&mut client.stream, &mut client.buf).unwrap();
            if wire::parse(&client.buf).unwrap().reply_serial == Some(999) {
                break;
            }
        }
        client.stream.set_read_timeout(None).unwrap();
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut serial = 1000;
            while wire::read_message(&mut client.stream, &mut buf).is_ok() {
                let header = wire::parse(&buf).unwrap();
                if header.kind != wire::METHOD_CALL {
                    continue;
                }
                serial += 1;
                let (kind, value) = match header.member {
                    Some("GetSecret") => (wire::METHOD_RETURN, "hunter2"),
                    Some("CreateItem" | "Delete") => (wire::METHOD_RETURN, "/"),
                    Some("Get") => (wire::METHOD_RETURN, "GitHub token"),
                    _ => (wire::ERROR, "unknown"),
                };
                let mut reply = Builder::new(kind, wire::NO_REPLY_EXPECTED, serial)
                    .field_u32(5, header.serial)
                    .field(6, "s", header.sender.unwrap());
                if kind == wire::ERROR {
                    reply = reply.field(4, "s", "org.example.Unknown");
                }
                let property = (header.member == Some("Get")).then(|| {
                    let mut body = Cursor::new(&buf, header.body);
                    body.string().unwrap();
                    body.string().unwrap()
                });
                let message = if property == Some("Attributes") {
                    let mut body = wire::Body::default();
                    body.signature("a{ss}");
                    if header.path.unwrap().contains("/agent/") {
                        body.string_map(&[("service", "agent"), ("user", "login")]);
                    } else {
                        body.string_map(&[("service", "github")]);
                    }
                    reply.finish("v", &body.0)
                } else if property.is_some() {
                    // A variant holding a string.
                    let mut body = vec![1, b's', 0, 0];
                    body.extend_from_slice(&(value.len() as u32).to_le_bytes());
                    body.extend_from_slice(value.as_bytes());
                    body.push(0);
                    reply.finish("v", &body)
                } else {
                    reply.strings(&[value])
                };
                client.stream.write_all(&message).unwrap();
            }
        });
    }

    #[test]
    fn sandboxes_reach_the_keyring_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bus) = test_bus(dir.path()) else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        fake_keyring(&bus.address);
        let keyring = KeyringBus::start(&dir.path().join("keyring.sock"), &bus.address).unwrap();
        let approvals = Arc::new(Approvals::new(|| {}));
        keyring.set_approvals(Some(Arc::clone(&approvals)));
        let mut client = Client::connect(keyring.socket());

        // Other services and owning names stay out of reach.
        let (kind, error) = client.call(
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StartTransientUnit",
            &[],
        );
        assert_eq!(kind, wire::ERROR);
        assert!(error.contains("only the keyring"), "{error}");
        let (kind, _) = client.call(BUS, "/org/freedesktop/DBus", BUS, "RequestName", &["a.b"]);
        assert_eq!(kind, wire::ERROR);
        let rule = "type='signal',interface='org.example'";
        let (kind, _) = client.call(BUS, "/org/freedesktop/DBus", BUS, "AddMatch", &[rule]);
        assert_eq!(kind, wire::ERROR);
        let rule = "type='signal',sender='org.freedesktop.secrets'";
        let (kind, _) = client.call(BUS, "/org/freedesktop/DBus", BUS, "AddMatch", &[rule]);
        assert_eq!(kind, wire::METHOD_RETURN);
        // The bus takes these and would pass the keyring's replies to
        // other clients.
        for rule in [
            "sender='org.freedesktop.secrets',eavesdrop=true",
            "sender='org.freedesktop.secrets',eavesdrop ='true'",
        ] {
            let (kind, _) = client.call(BUS, "/org/freedesktop/DBus", BUS, "AddMatch", &[rule]);
            assert_eq!(kind, wire::ERROR, "{rule}");
        }

        // Reading a secret asks, by the secret's label.
        let item = "/org/freedesktop/secrets/collection/login/1";
        let asking = {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || {
                loop {
                    if let Some(question) = approvals.pending().into_iter().next() {
                        approvals.answer(question.id, true);
                        return question;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let session = "/org/freedesktop/secrets/session/1";
        let (kind, secret) = client.call(SECRETS, item, ITEM, "GetSecret", &[session]);
        assert_eq!((kind, secret.as_str()), (wire::METHOD_RETURN, "hunter2"));
        let question = asking.join().unwrap();
        assert_eq!(question.guard, KEYRING_GUARD);
        assert_eq!(question.host, "GitHub token");
        assert_eq!(question.method, "read");

        // The unique name of the keyring counts as the keyring.
        let owner = shared_owner(&keyring);
        let (kind, _) = client.call(&owner, item, ITEM, "GetSecret", &[session]);
        assert_eq!(kind, wire::METHOD_RETURN, "remembered answer");

        // A project's socket names the project, and a standing answer there
        // needs no question.
        let scoped = keyring
            .socket_for("p1", &dir.path().join("keyring-p1.sock"))
            .unwrap();
        let mut project = Client::connect(&scoped);
        let always = vec![(KEYRING_GUARD.to_owned(), "GitHub token".to_owned())];
        approvals.set_always("p1", always);
        let (kind, secret) = project.call(SECRETS, item, ITEM, "GetSecret", &[session]);
        assert_eq!((kind, secret.as_str()), (wire::METHOD_RETURN, "hunter2"));
        assert!(approvals.pending().is_empty());
        approvals.set_always("p1", Vec::new());
        let asking = {
            let approvals = Arc::clone(&approvals);
            thread::spawn(move || {
                loop {
                    if let Some(question) = approvals.pending().into_iter().next() {
                        approvals.answer(question.id, false);
                        return question;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let (kind, _) = project.call(SECRETS, item, ITEM, "GetSecret", &[session]);
        assert_eq!(kind, wire::ERROR);
        assert_eq!(asking.join().unwrap().scope.as_deref(), Some("p1"));

        keyring.set_mode(GuardMode::Block);
        let (kind, error) = client.call(SECRETS, item, ITEM, "GetSecret", &[session]);
        assert_eq!(kind, wire::ERROR);
        assert!(error.contains("guard blocks"), "{error}");
        assert!(approvals.pending().is_empty());
    }

    /// Answers the next question with `allow` and returns it.
    fn answer_next(approvals: &Arc<Approvals>, allow: bool) -> thread::JoinHandle<crate::Approval> {
        let approvals = Arc::clone(approvals);
        thread::spawn(move || {
            loop {
                if let Some(question) = approvals.pending().into_iter().next() {
                    approvals.answer(question.id, allow);
                    return question;
                }
                thread::sleep(Duration::from_millis(5));
            }
        })
    }

    #[test]
    fn agents_reach_their_logins_without_a_question() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bus) = test_bus(dir.path()) else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        fake_keyring(&bus.address);
        let keyring = KeyringBus::start(&dir.path().join("keyring.sock"), &bus.address).unwrap();
        let approvals = Arc::new(Approvals::new(|| {}));
        keyring.set_approvals(Some(Arc::clone(&approvals)));
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        keyring.set_logins(vec![vec![pair("service", "agent"), pair("user", "login")]]);
        let mut client = Client::connect(keyring.socket());
        let session = "/org/freedesktop/secrets/session/1";
        let login = "/org/freedesktop/secrets/collection/agent/1";
        let other = "/org/freedesktop/secrets/collection/login/1";
        let collection = "/org/freedesktop/secrets/collection/login";

        let (kind, secret) = client.call(SECRETS, login, ITEM, "GetSecret", &[session]);
        assert_eq!((kind, secret.as_str()), (wire::METHOD_RETURN, "hunter2"));
        assert!(approvals.pending().is_empty());
        let mut body = wire::Body::default();
        body.new_item(&[("user", "login"), ("service", "agent")], true);
        // The secret itself and `replace` are left out; the fake keyring
        // does not read them.
        let (kind, _) = client.call_with(collection, COLLECTION, "CreateItem", "a{sv}", &body.0);
        assert_eq!(kind, wire::METHOD_RETURN);

        // Other secrets, a new one with other attributes and deleting the
        // login still ask. The fake keyring labels everything alike and an
        // answer holds for its label, so each asks from a project of its own.
        let project = |scope: &str| {
            let socket = dir.path().join(format!("keyring-{scope}.sock"));
            Client::connect(&keyring.socket_for(scope, &socket).unwrap())
        };
        let asking = answer_next(&approvals, false);
        let (kind, _) = project("p1").call(SECRETS, other, ITEM, "GetSecret", &[session]);
        assert_eq!(kind, wire::ERROR);
        assert_eq!(asking.join().unwrap().method, "read");
        let asking = answer_next(&approvals, false);
        let mut body = wire::Body::default();
        body.new_item(&[("service", "agent")], false);
        let (kind, _) =
            project("p2").call_with(collection, COLLECTION, "CreateItem", "a{sv}", &body.0);
        assert_eq!(kind, wire::ERROR);
        assert_eq!(asking.join().unwrap().method, "create");
        let asking = answer_next(&approvals, false);
        let (kind, _) = project("p3").call(SECRETS, login, ITEM, "Delete", &[]);
        assert_eq!(kind, wire::ERROR);
        assert_eq!(asking.join().unwrap().method, "delete");

        keyring.set_mode(GuardMode::Block);
        let (kind, error) = client.call(SECRETS, login, ITEM, "GetSecret", &[session]);
        assert_eq!(kind, wire::ERROR);
        assert!(error.contains("guard blocks"), "{error}");
    }

    #[test]
    fn logins_match_by_every_attribute_once() {
        let logins = vec![vec![
            ("service".to_owned(), "gemini".to_owned()),
            ("username".to_owned(), "antigravity".to_owned()),
        ]];
        let matches = |pairs: &[(&str, &str)]| login_matches(&logins, pairs);
        assert!(matches(&[
            ("username", "antigravity"),
            ("service", "gemini")
        ]));
        assert!(matches(&[
            ("service", "gemini"),
            ("username", "antigravity"),
            ("xdg:schema", "org.freedesktop.Secret.Generic"),
        ]));
        assert!(!matches(&[("service", "gemini")]));
        assert!(!matches(&[("service", "gemini"), ("username", "other")]));
        assert!(!matches(&[
            ("service", "gemini"),
            ("username", "antigravity"),
            ("service", "github"),
        ]));
        assert!(!login_matches(&[Vec::new()], &[("service", "gemini")]));
    }

    #[test]
    fn match_rules_are_read_the_way_the_bus_reads_them() {
        let keys = |rule: &str| match_rule_keys(rule);
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            keys(" type ='signal',sender='a,b',member=x\\'y,"),
            Some(vec![
                pair("type", "signal"),
                pair("sender", "a,b"),
                pair("member", "x'y")
            ])
        );
        assert_eq!(keys("path='/a\\b'"), Some(vec![pair("path", "/a\\b")]));
        assert_eq!(keys("arg0=a\\b"), Some(vec![pair("arg0", "a\\b")]));
        assert_eq!(keys(""), Some(vec![]));
        // Like the bus, nothing skips the blanks after `=`.
        assert_eq!(keys("type= 'a' "), Some(vec![pair("type", " a ")]));
        assert_eq!(keys("sender='a"), None);
        assert_eq!(keys("sender"), None);
        assert!(known_match_key("arg63path") && known_match_key("arg0namespace"));
        assert!(!known_match_key("arg64") && !known_match_key("argpath"));
        assert!(!known_match_key("arg1namespace") && !known_match_key("evesdrop"));

        let mut session = Session {
            owner: None,
            serial: 0,
        };
        let shared = Shared {
            address: Address::Path("/nonexistent".into()),
            uid: 0,
            guid: String::new(),
            guard: builtin_guards().remove(0),
            mode: RwLock::new(GuardMode::Ask),
            approvals: RwLock::new(None),
            logins: RwLock::new(Vec::new()),
            lookup: Mutex::new(None),
            stop: AtomicBool::new(false),
        };
        let secrets = "type='signal',sender='org.freedesktop.secrets'";
        assert!(session.subscribes_to_keyring(&shared, secrets));
        assert!(session.subscribes_to_keyring(&shared, "sender=org.freedesktop.DBus"));
        for rule in [
            "sender='org.freedesktop.secrets',eavesdrop=true",
            "sender='org.freedesktop.secrets',eavesdrop ='true'",
            "sender='org.freedesktop.secrets', eavesdrop='false'",
            "sender='org.freedesktop.secrets',sender=':1.9'",
            "sender=':1.9',arg0='sender=\\'org.freedesktop.secrets\\''",
            "member='sender=\\'org.freedesktop.secrets\\''",
            "sender='org.freedesktop.secrets',unknown='x'",
            "type='signal'",
            "sender='org.freedesktop.secrets",
        ] {
            assert!(!session.subscribes_to_keyring(&shared, rule), "{rule}");
        }
    }

    #[test]
    fn a_socket_serves_a_bounded_number_of_clients() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", dir.path().join("none").display());
        let keyring = KeyringBus::start(&dir.path().join("keyring.sock"), &address).unwrap();
        let wait = Some(Duration::from_millis(200));
        // Served clients wait for authentication; the server says nothing.
        let served = |client: &mut UnixStream| {
            client.set_read_timeout(wait).unwrap();
            client.read(&mut [0; 1]).is_err()
        };
        let mut clients: Vec<UnixStream> = (0..MAX_CLIENTS)
            .map(|_| UnixStream::connect(keyring.socket()).unwrap())
            .collect();
        assert!(served(&mut clients[MAX_CLIENTS - 1]));
        let mut extra = UnixStream::connect(keyring.socket()).unwrap();
        assert!(!served(&mut extra), "one too many is closed");
        clients.pop();
        // The place comes back once the server's thread for the closed
        // client has noticed, which can take a while on a busy machine.
        let mut again = None;
        for _ in 0..250 {
            let mut client = UnixStream::connect(keyring.socket()).unwrap();
            if served(&mut client) {
                again = Some(client);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(again.is_some(), "a closed client gives its place back");
    }

    #[test]
    fn refusals_come_from_the_bus() {
        let call = Builder::new(wire::METHOD_CALL, 0, 7)
            .field(1, "o", "/org/freedesktop/portal/desktop")
            .field(2, "s", "org.freedesktop.portal.Settings")
            .field(3, "s", "Read")
            .field(6, "s", "org.freedesktop.portal.Desktop")
            .strings(&[]);
        let mut session = Session {
            owner: None,
            serial: 0,
        };
        let error = session.refusal(&wire::parse(&call).unwrap(), "no");
        let header = wire::parse(&error).unwrap();
        assert_eq!(header.kind, wire::ERROR);
        assert_eq!(header.reply_serial, Some(7));
        assert_eq!(header.sender, Some(BUS));
    }

    fn shared_owner(keyring: &KeyringBus) -> String {
        keyring.shared.lookup(Lookup::owner).unwrap()
    }
}
