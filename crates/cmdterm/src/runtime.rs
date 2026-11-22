//! Where a worktree's terminals run: the project's environment, the
//! worktree's devcontainer, or a sandbox (`Runtime` in
//! `state`). VCS work always stays in the project's environment.
//!
//! Starting a devcontainer can take minutes, so it runs on a worker thread
//! and writes its output to a log under `<config-dir>/logs/`. Until it is
//! ready, the worktree's terminals show that log. When it is ready they are
//! restarted inside the container, and extensions get `environment_ready`.
//!
//! The same worker threads carry out extensions' `term.copy`, `term.run`
//! and `term.trust` requests, and hand the outcome to the extension's `done`
//! function when it passed one. Everything here is cold: nothing runs per frame,
//! and [`Workspace::poll_runtimes`] costs one `try_recv` and one load of the
//! approvals' revision while idle.
//!
//! A sandboxed worktree whose settings ask for an engine sidecar waits for
//! it the same way: the sidecar starts on a worker thread, its log stands in
//! for the terminals, and they restart in the sandbox once it listens. A
//! project has one sidecar, started by its first sandboxed terminal, which
//! shares every worktree the project has then. A worktree it does not share
//! yet restarts it with that worktree added; the socket stays where it was,
//! so the other worktrees' terminals keep it. Sidecars stop when the app
//! quits ([`Workspace::stop_sidecars`]).
//!
//! Sandboxes reach the network through the proxy when the settings limit
//! it to allowed hosts, and also when extensions gave the proxy work:
//! secrets to reveal or guards that do not allow. Questions of guards in
//! `Ask` mode wait in [`Runtimes::approvals`] until the approval overlay
//! (`ui/approval.rs`) answers them. Blocked hosts and the blocklists of
//! extensions give the proxy work too; [`Runtimes::blocklists`] downloads
//! and caches the lists.

use std::cell::{Cell, OnceCell};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::thread;

use crate::container::{Sidecar, SidecarKind};
use crate::ext::{Callback, Event, JobResult, SandboxRules};
use crate::state::{EngineAccess, Project, Runtime, SandboxSettings, SidecarEngine};
use cmdterm_env::{
    Approval, Approvals, Environment, GuardMode, GuardRule, ProxyRules, SandboxKind, SandboxPolicy,
    SandboxProxy, SandboxTarget,
};
use schnellui::RedrawSignal;

use crate::workspace::{Inner, Workspace};

/// The environment a new terminal starts in.
pub enum TermEnv {
    Ready(Environment),
    /// The environment is still starting; show this log instead.
    Waiting(PathBuf),
}

enum Phase {
    Preparing { log: PathBuf },
    Ready(Environment),
    Failed { log: PathBuf },
}

/// A project's engine sidecar, see `crate::container::start_sidecar`.
enum SidecarPhase {
    /// `shares` are the worktrees it starts with; `waiting` the worktrees
    /// whose terminals show its log, some of which may not be in `shares`.
    Starting {
        log: PathBuf,
        shares: Vec<PathBuf>,
        waiting: Vec<PathBuf>,
    },
    Ready {
        sidecar: Sidecar,
        shares: Vec<PathBuf>,
    },
    Failed {
        log: PathBuf,
    },
}

enum Done {
    Prepared {
        project: u64,
        worktree: PathBuf,
        result: Result<(Environment, bool), String>,
    },
    Sidecar {
        project: u64,
        result: Result<Sidecar, String>,
    },
    Job(Result<Option<String>, String>),
    /// A job an extension passed a `done` function for.
    Callback(Callback, JobResult),
}

/// Devcontainer phases and background jobs.
pub struct Runtimes {
    phases: HashMap<(u64, PathBuf), Phase>,
    /// Engine sidecars per project id.
    sidecars: HashMap<u64, SidecarPhase>,
    /// Project environments already announced with `environment_ready`.
    announced: HashSet<u64>,
    done: (Sender<Done>, Receiver<Done>),
    log_dir: PathBuf,
    /// One directory per project id for what sandboxed shells write, such
    /// as their history.
    sandbox_dir: PathBuf,
    /// Where the app keeps its files.
    pub dirs: cmdterm_env::AppDirs,
    /// The filtering proxy of sandboxes, started on first use. `None`
    /// inside when it could not start.
    proxy: OnceCell<Option<cmdterm_env::NetworkProxy>>,
    /// The keyring bus of bubblewrap sandboxes, started on first use.
    /// `None` inside when this session has no bus or it could not start.
    #[cfg(target_os = "linux")]
    keyring: OnceCell<Option<cmdterm_env::KeyringBus>>,
    /// The port hubs of local sandboxes, one per worktree.
    pub ports: Arc<PortHubs>,
    /// The hubs' revision the palette last listed.
    seen_ports: Cell<u64>,
    /// The extensions' sandbox revision the proxy's hosts were taken from.
    synced_rules: Cell<u64>,
    /// Questions of guards in `Ask` mode.
    pub approvals: Arc<Approvals>,
    /// The approvals' revision the overlay last showed.
    seen_approvals: Cell<u64>,
    /// The copies of agents' settings in sandboxes, see `agent_sync.rs`.
    pub agent_sync: crate::agent_sync::AgentSync,
    /// The extensions' blocklists, downloaded once the proxy runs.
    pub blocklists: crate::blocklists::Blocklists,
    /// The blocklists' revision the proxy was last given.
    synced_blocklists: Cell<u64>,
}

/// Whether a local bubblewrap sandbox with `settings` has a network
/// namespace of its own: it has unless it keeps the host's network. That
/// needs the whole network, so `restrict_network` isolates whatever
/// `host_network` says.
pub fn isolates(settings: &SandboxSettings) -> bool {
    !(settings.network && !settings.restrict_network && settings.host_network)
}

/// The port hubs of the local sandboxes, one per worktree, shared with the
/// control socket.
#[derive(Default)]
pub struct PortHubs {
    hubs: std::sync::Mutex<HashMap<(u64, PathBuf), Arc<cmdterm_env::PortHub>>>,
}

impl PortHubs {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<(u64, PathBuf), Arc<cmdterm_env::PortHub>>> {
        self.hubs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The hub of a worktree, if its sandboxes have started one.
    pub fn get(&self, project: u64, worktree: &Path) -> Option<Arc<cmdterm_env::PortHub>> {
        self.lock().get(&(project, worktree.to_path_buf())).cloned()
    }

    /// Keeps `hub` as the hub of `worktree`.
    pub(crate) fn insert(&self, project: u64, worktree: &Path, hub: Arc<cmdterm_env::PortHub>) {
        self.lock().insert((project, worktree.to_path_buf()), hub);
    }

    /// A number that changes when a port of any hub opens, closes or is
    /// exposed. Allocates nothing.
    pub fn revision(&self) -> u64 {
        let hubs = self.lock();
        hubs.values()
            .fold(0u64, |sum, hub| sum.wrapping_add(hub.revision()))
    }
}

impl crate::control::PortAccess for PortHubs {
    fn http_proxy(&self, project: u64, worktree: &Path) -> Option<String> {
        let (port, token) = self.get(project, worktree)?.http_proxy().ok()?;
        Some(token.url(port))
    }

    fn list(&self, project: u64, worktree: &Path) -> Option<Vec<(u16, Option<u16>)>> {
        let hub = self.get(project, worktree)?;
        Some(
            hub.ports()
                .into_iter()
                .map(|p| (p.port, p.exposed))
                .collect(),
        )
    }

    fn expose(
        &self,
        project: u64,
        worktree: &Path,
        port: u16,
        host_port: Option<u16>,
    ) -> Result<u16, String> {
        let hub = self
            .get(project, worktree)
            .ok_or("no port forwarding here")?;
        let listening = hub.ports().iter().any(|p| p.port == port);
        if !listening {
            return Err(format!(
                "nothing listens on port {port} in this worktree's sandbox"
            ));
        }
        hub.expose(port, host_port).map_err(|e| e.to_string())
    }

    fn unexpose(&self, project: u64, worktree: &Path, port: u16) -> Result<(), String> {
        let hub = self
            .get(project, worktree)
            .ok_or("no port forwarding here")?;
        hub.unexpose(port);
        Ok(())
    }
}

impl Runtimes {
    pub fn new(dirs: &cmdterm_env::AppDirs, wake: Arc<OnceLock<RedrawSignal>>) -> Self {
        let blocklists =
            crate::blocklists::Blocklists::new(dirs.cache.join("blocklists"), Arc::clone(&wake));
        let log_dir = dirs.logs.clone();
        let sandbox_dir = dirs.state.join("sandbox");
        let approvals = Arc::new(Approvals::new(move || wake_up(&wake)));
        let agent_sync =
            crate::agent_sync::AgentSync::new(sandbox_dir.join("agents"), Arc::clone(&approvals));
        Self {
            phases: HashMap::new(),
            sidecars: HashMap::new(),
            announced: HashSet::new(),
            done: mpsc::channel(),
            log_dir,
            sandbox_dir,
            dirs: dirs.clone(),
            proxy: OnceCell::new(),
            #[cfg(target_os = "linux")]
            keyring: OnceCell::new(),
            ports: Arc::new(PortHubs::default()),
            seen_ports: Cell::new(0),
            synced_rules: Cell::new(u64::MAX),
            blocklists,
            synced_blocklists: Cell::new(0),
            approvals,
            seen_approvals: Cell::new(0),
            agent_sync,
        }
    }

    /// Where sandboxes reach the proxy, when they need it: the settings
    /// limit the network to allowed hosts, or the extensions' rules give it
    /// work (see [`proxy_rules`]), or they block hosts. Starts the proxy on first use, with its
    /// socket in the sandbox directory.
    ///
    /// For `intercept_tls` or the rules the proxy writes its CA bundle next
    /// to its socket. When interception cannot start, a sandbox with a
    /// restricted network gets no network, and one with the whole network
    /// gets no proxy.
    ///
    /// A local project gets an endpoint of its own (`scope`, its id), so
    /// its questions can be allowed for good in it.
    fn proxy(
        &self,
        settings: &SandboxSettings,
        rules: &SandboxRules,
        scope: Option<u64>,
    ) -> Option<SandboxProxy> {
        let restricted = settings.network && settings.restrict_network;
        let blocks = !rules.blocked.is_empty() || !rules.blocklists.is_empty();
        // A bubblewrap sandbox with a namespace of its own reaches the
        // network only through the proxy, also when it may reach any host.
        // So does an AppContainer on Windows, which has no other network.
        let isolated = scope.is_some()
            && (cfg!(target_os = "linux") || (cfg!(windows) && settings.windows_appcontainer))
            && isolates(settings);
        if !settings.network
            || (!isolated && !restricted && !blocks && proxy_rules(settings, rules).is_empty())
        {
            return None;
        }
        let proxy = self.proxy.get_or_init(|| {
            let socket = self.socket_path("proxy.sock")?;
            let proxy = cmdterm_env::NetworkProxy::start(&socket, Vec::new()).ok()?;
            proxy.set_approvals(Some(Arc::clone(&self.approvals)));
            Some(proxy)
        });
        let proxy = proxy.as_ref()?;
        self.configure(proxy, settings, rules).ok()?;
        let endpoint = match scope {
            Some(id) => {
                let socket = self.scoped_socket("proxy", id)?;
                proxy.endpoint_for(&id.to_string(), &socket).ok()?
            }
            None => proxy.endpoint(),
        };
        // An isolated bubblewrap sandbox reaches the proxy through the
        // bridge and the socket, which take no token. Keeping it out keeps
        // it off bwrap's command line, which every user can read.
        if cfg!(target_os = "linux") && isolated {
            return Some(endpoint.without_token());
        }
        Some(endpoint)
    }

    /// Where the `kind` socket of project `id` goes, see
    /// [`Self::socket_path`].
    fn scoped_socket(&self, kind: &str, id: u64) -> Option<PathBuf> {
        self.socket_path(&format!("{kind}-{id}.sock"))
    }

    /// Where the socket `name` goes: the sandbox directory, or the
    /// temporary directory when that path would be too long for a Unix
    /// socket (about 100 bytes). There the name also carries a hash of the
    /// sandbox directory: runtimes of one process, such as tests side by
    /// side, would otherwise replace each other's sockets.
    fn socket_path(&self, name: &str) -> Option<PathBuf> {
        use std::hash::{Hash, Hasher};
        std::fs::create_dir_all(&self.sandbox_dir).ok()?;
        let socket = self.sandbox_dir.join(name);
        if socket.as_os_str().len() <= 100 {
            return Some(socket);
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.sandbox_dir.hash(&mut hasher);
        let name = format!(
            "cmdterm-{}-{:016x}-{name}",
            std::process::id(),
            hasher.finish()
        );
        Some(std::env::temp_dir().join(name))
    }

    /// The socket of the port hub of `worktree` in project `id`. Starts the
    /// hub on first use. On Windows the hub has no socket, only its TCP
    /// port ([`cmdterm_env::PortHub::tcp_port`]), and the path is unused. The hub sends what the sandbox's own network may
    /// not reach through the plain port of the project's filtering proxy
    /// (no TLS interception), when `proxy` is set.
    #[cfg(any(target_os = "linux", windows))]
    fn port_hub(&self, id: u64, worktree: &Path, proxy: Option<&SandboxProxy>) -> Option<PathBuf> {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (id, worktree).hash(&mut hasher);
        let socket = self.scoped_socket("ports", hasher.finish())?;
        let hub = match self.ports.get(id, worktree) {
            Some(hub) => hub,
            None => {
                let hub = Arc::new(cmdterm_env::PortHub::start(&socket).ok()?);
                self.ports.insert(id, worktree, Arc::clone(&hub));
                hub
            }
        };
        // The hub's upstream serves the debug browser, which does not trust
        // the proxy's interception CA, so it gets the proxy's plain port.
        let upstream = proxy
            .and(self.proxy.get())
            .and_then(|proxy| proxy.as_ref())
            .and_then(|proxy| proxy.plain_port().ok());
        let upstream = upstream
            .map(|(port, token)| (std::net::SocketAddr::from(([127, 0, 0, 1], port)), token));
        hub.set_upstream(upstream);
        Some(socket)
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    fn port_hub(&self, _: u64, _: &Path, _: Option<&SandboxProxy>) -> Option<PathBuf> {
        None
    }

    /// The socket of the keyring bus that stands in for the session bus in
    /// bubblewrap sandboxes. Starts it on first use, with its socket in the
    /// sandbox directory. `None` when the app has no session bus.
    /// Like the proxy, a local project (`scope`) gets a socket of its own.
    #[cfg(target_os = "linux")]
    fn keyring(&self, settings: &SandboxSettings, scope: Option<u64>) -> Option<PathBuf> {
        let keyring = self.keyring.get_or_init(|| {
            let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()?;
            let socket = self.socket_path("keyring.sock")?;
            let keyring = cmdterm_env::KeyringBus::start(&socket, &address).ok()?;
            keyring.set_approvals(Some(Arc::clone(&self.approvals)));
            Some(keyring)
        });
        let keyring = keyring.as_ref()?;
        keyring.set_mode(settings.guard_mode(cmdterm_env::KEYRING_GUARD, None));
        keyring.set_logins(keyring_logins());
        match scope {
            Some(id) => {
                let socket = self.scoped_socket("keyring", id)?;
                keyring.socket_for(&id.to_string(), &socket).ok()
            }
            None => Some(keyring.socket().to_path_buf()),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn keyring(&self, _: &SandboxSettings, _: Option<u64>) -> Option<PathBuf> {
        None
    }

    /// Gives `proxy` the allowed hosts and rules of the settings and the
    /// extensions, and turns interception on as far as they need it.
    fn configure(
        &self,
        proxy: &cmdterm_env::NetworkProxy,
        settings: &SandboxSettings,
        rules: &SandboxRules,
    ) -> std::io::Result<()> {
        let restricted = settings.network && settings.restrict_network;
        let proxy_rules = proxy_rules(settings, rules);
        // On Windows, programs on Windows' own TLS stack (.NET, PowerShell,
        // winget) trust the Windows store only, which never holds the
        // proxy's CA, so they fail on every intercepted tunnel. Only the
        // hosts the rules read are intercepted there.
        let all = restricted && settings.intercept_tls && !cfg!(windows);
        let bundle =
            (all || !proxy_rules.is_empty()).then(|| self.sandbox_dir.join("proxy-ca.pem"));
        proxy.set_allowed(allowed_hosts(settings, rules));
        proxy.allow_any_host(!restricted);
        proxy.set_rules(proxy_rules);
        self.blocklists.want(&rules.blocklists);
        proxy.set_blocking(cmdterm_env::Blocking {
            lists: self.blocklists.lists(&rules.blocklists),
            hosts: rules.blocked.clone(),
            exceptions: rules.unblocked.clone(),
        });
        proxy.intercept_tls(bundle.as_deref(), all)
    }

    /// The hosts the sandbox proxy refused lately, once it runs, as the
    /// control socket reports them.
    pub fn refusals(&self) -> Option<crate::control::Refusals> {
        let log = self.proxy.get()?.as_ref()?.refusals();
        Some(std::sync::Arc::new(move || {
            (log.list().into_iter())
                .map(|refusal| {
                    let ago = refusal.at.elapsed().unwrap_or_default().as_secs();
                    serde_json::json!({
                        "host": refusal.host,
                        "reason": refusal.reason,
                        "seconds_ago": ago,
                    })
                })
                .collect()
        }))
    }

    /// Hands the allowed hosts and rules of the settings and the extensions
    /// to a running proxy. Open connections keep their upstream.
    pub fn update_proxy(&self, settings: &SandboxSettings, rules: &SandboxRules) {
        #[cfg(target_os = "linux")]
        if let Some(Some(keyring)) = self.keyring.get() {
            keyring.set_mode(settings.guard_mode(cmdterm_env::KEYRING_GUARD, None));
        }
        if let Some(Some(proxy)) = self.proxy.get() {
            let _ = self.configure(proxy, settings, rules);
        }
        (self.agent_sync).set_mode(settings.guard_mode(cmdterm_env::AGENT_SETTINGS_GUARD, None));
    }

    /// Hands the questions `project` allowed for good to the approvals,
    /// scoped to its id.
    pub fn share_always_allowed(&self, project: &Project) {
        let always = (project.always_allowed.iter())
            .map(|a| (a.guard.clone(), a.host.clone()))
            .collect();
        self.approvals.set_always(&project.id.to_string(), always);
    }

    /// Where sandboxed shells of project `id` keep their history.
    fn shell_dir(&self, id: u64) -> PathBuf {
        self.sandbox_dir.join(id.to_string())
    }

    /// Where the sidecar of the project at `root` puts its socket: a private
    /// directory, in the runtime directory when there is one, because socket
    /// paths are limited to about 100 bytes.
    fn sidecar_dir(&self, root: &Path) -> PathBuf {
        let name = crate::container::sidecar_name(root);
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("scriptschnell-term").join(name),
            None => self.sandbox_dir.join("sidecars").join(name),
        }
    }

    /// Deletes what sandboxed shells of a removed project wrote.
    pub fn forget_project(&self, id: u64) {
        self.approvals.set_always(&id.to_string(), Vec::new());
        self.agent_sync.forget(id);
        let _ = std::fs::remove_dir_all(self.shell_dir(id));
    }
}

/// Every guard: the proxy's own ([`cmdterm_env::builtin_guards`]), then
/// the extensions'. Modes are not applied yet.
pub fn all_guards(rules: &SandboxRules) -> Vec<GuardRule> {
    let mut guards = cmdterm_env::builtin_guards();
    for guard in &rules.guards {
        if !guards.iter().any(|g| g.id == guard.id) {
            guards.push(guard.clone());
        }
    }
    guards
}

/// What the proxy does besides filtering hosts: the extensions' secrets,
/// and every guard with the user's mode, leaving out the ones that allow.
pub fn proxy_rules(settings: &SandboxSettings, rules: &SandboxRules) -> ProxyRules {
    ProxyRules {
        secrets: rules.secrets.clone(),
        guards: all_guards(rules)
            .into_iter()
            .map(|guard| GuardRule {
                mode: settings.guard_mode(&guard.id, guard.kind),
                ..guard
            })
            .filter(|guard| guard.mode != GuardMode::Allow)
            .collect(),
    }
}

/// The user's allowed hosts, then the ones extensions add, then the hosts
/// of the OTLP endpoints the app's environment names, so agents in a
/// sandbox send telemetry where the user told them to.
pub(crate) fn allowed_hosts(settings: &SandboxSettings, rules: &SandboxRules) -> Vec<String> {
    let mut hosts = settings.allowed_hosts.clone();
    let telemetry = cmdterm_env::host_env::telemetry_hosts();
    for host in rules.hosts.iter().chain(&telemetry) {
        if !hosts.contains(host) {
            hosts.push(host.clone());
        }
    }
    hosts
}

impl Inner {
    /// Gives the proxy the extensions' hosts and rules after they changed,
    /// and marks the approval overlay dirty when a question came or went.
    /// Runs every frame and costs three comparisons while nothing changes.
    pub(crate) fn sync_sandbox_rules(&mut self) {
        let revision = self.ext.sandbox_revision();
        if self.runtimes.synced_rules.get() != revision {
            self.runtimes.synced_rules.set(revision);
            self.runtimes
                .update_proxy(&self.settings.sandbox, self.ext.sandbox_rules());
            self.publish_environment();
        }
        let lists = self.runtimes.blocklists.revision();
        if self.runtimes.synced_blocklists.get() != lists {
            self.runtimes.synced_blocklists.set(lists);
            self.runtimes
                .update_proxy(&self.settings.sandbox, self.ext.sandbox_rules());
            for (id, error) in self.runtimes.blocklists.take_failures() {
                self.push_notice((self.tr.blocklist_failed)(&id, &error));
            }
        }
        let listed = self.runtimes.ports.revision();
        if self.runtimes.seen_ports.get() != listed {
            self.runtimes.seen_ports.set(listed);
            self.dirty.palette_list |= self.palette.open && self.palette.input.is_none();
        }
        let asked = self.runtimes.approvals.revision();
        if self.runtimes.seen_approvals.get() != asked {
            self.runtimes.seen_approvals.set(asked);
            self.dirty.approval = true;
        }
    }

    /// The oldest question of a guard, if one waits.
    pub fn pending_approval(&self) -> Option<Approval> {
        self.runtimes.approvals.pending().into_iter().next()
    }
}

/// The kind name extensions see for a project environment.
fn env_kind(env: &Environment) -> &'static str {
    match env.base() {
        Environment::Local => "local",
        Environment::Ssh(_) => "ssh",
        Environment::Remote(_) => "remote",
        Environment::Vm(_) => "vm",
        Environment::Custom(_) => "custom",
        Environment::Container(_) => "devcontainer",
        Environment::Sandbox(_) => "sandbox",
    }
}

/// The name extensions and the status bar use for a runtime.
pub fn runtime_name(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::Host => "host",
        Runtime::Devcontainer => "devcontainer",
        Runtime::Sandbox => "sandbox",
    }
}

/// The app's directories a sandbox uses, see [`sandbox_env`].
#[derive(Clone, Copy)]
pub struct SandboxDirs<'a> {
    /// Shell history and editor state, one per project.
    pub shell: &'a Path,
    /// Extension files that stand in for sandbox files.
    pub files: &'a Path,
    /// Where the app keeps its files.
    pub app: &'a cmdterm_env::AppDirs,
    /// The copies of agents' settings, for sandboxes that get them.
    /// Without them the agents' missing settings and hooks are not
    /// created either, see [`sandbox_env`].
    pub agents: Option<&'a crate::agent_sync::AgentSync>,
}

/// A sandbox for `worktree`: it may write the worktree, the repository's
/// metadata (git worktrees and hg shares write there) and the configured
/// paths, and it cannot read the configured secrets or the session's
/// runtime directory, which holds agent and keyring sockets. Paths from the
/// settings are this machine's, so a remote project's sandbox only gets the
/// worktree.
///
/// The repository's configuration and hooks stay read-only, since the app
/// runs git and hg on them outside the sandbox, see [`VcsMetadata`].
///
/// Coding agents may write their state (see [`crate::agents::Agent`]),
/// except their settings, hooks and skills. On Linux they write copies of
/// their settings instead, which come to the host after asking, see
/// `agent_sync.rs`. Where only existing paths can be kept read-only
/// (bubblewrap, Windows), missing ones are created first, empty: a
/// directory, or a file that means no settings, such as `{}`.
///
/// The host's shell history and editor state stay read-only. A local
/// project keeps its own in `shell_dir` instead: where the sandbox can
/// remount paths (Linux), fish's data directory is replaced by
/// `shell_dir/fish` and neovim's state directory by `shell_dir/nvim`, and
/// `HISTFILE` (bash, zsh) points to `shell_dir/history`.
///
/// The app's directories are hidden, except the references and shipped
/// extensions in [`crate::bundle::READABLE`] of the data directory, the
/// user's own extensions and the agents' skill, which show through
/// read-only. One that lies in a writable path, such as the cache in
/// `~/.cache`, stays hidden there, and read-only on Windows, which cannot
/// hide it.
///
/// When the settings restrict the network, a local project's sandbox
/// reaches it through `proxy` only. Without a proxy (it failed to start, or
/// the project is remote and cannot reach this machine's socket) the
/// sandbox gets no network at all.
///
/// Extensions add writable, hidden and redirected paths, variables and
/// files through `rules`, for local projects only. The user's hidden paths
/// win: an extension's writable path or redirect that overlaps one is
/// dropped. A file may stand in for a hidden path, because its content
/// comes from the extension, not from the host. Files are written to
/// `files_dir` and bound read-only where the sandbox can remount paths
/// (Linux); elsewhere the sandbox path is hidden instead.
///
/// The project's own folders ([`crate::state::ProjectSandbox`]) are writable or
/// read-only. They may lie in or be one of the user's hidden paths, which
/// they then show through, but a folder that holds a hidden path or one of
/// the app's directories, or lies in one of those, is dropped. A remote
/// project gets only its absolute ones.
///
/// With `engines` set to passthrough, a local project's sandbox reaches
/// this machine's container engines and WSL interop, see
/// [`cmdterm_env::pass_engines`]; otherwise they are hidden from it. A
/// running `sidecar` takes their place.
///
/// On Windows the proxy variables reach the launcher, which sets them for
/// the command. With `windows_appcontainer` the command runs in an
/// AppContainer (see [`use_appcontainer`]) and `ports` is a hub whose TCP
/// port the launcher's bridge uses.
///
/// A local project's bubblewrap sandbox gets `keyring` as its session bus:
/// the keyring bus that passes only the Secret Service, see
/// `cmdterm_env::KeyringBus`.
///
/// A local project's sandbox does not inherit the app's variables that look
/// like credentials, except those of [`passed_credentials`]. A remote one
/// has a shell on its machine remove them there, see
/// `SandboxPolicy::strip_credentials`.
#[allow(clippy::too_many_arguments)]
pub fn sandbox_env(
    settings: &SandboxSettings,
    rules: &SandboxRules,
    project: &Project,
    worktree: &Path,
    dirs: &SandboxDirs,
    proxy: Option<&SandboxProxy>,
    keyring: Option<&Path>,
    ports: Option<&Path>,
    sidecar: Option<&Sidecar>,
    git_dir: Option<&Path>,
) -> Environment {
    let SandboxDirs {
        shell: shell_dir,
        files: files_dir,
        app: app_dirs,
        agents,
    } = *dirs;
    let restricted = settings.network && settings.restrict_network;
    let network = settings.network && !restricted;
    let mut writable = vec![worktree.to_path_buf()];
    if project.root != worktree {
        writable.push(project.root.join(".git"));
        writable.push(project.root.join(".hg"));
    }
    let local = project.env.is_local();
    let recorded = project.git_dir.as_deref();
    let vcs = VcsMetadata::of(worktree, &project.root, local, recorded, git_dir);
    writable.extend(vcs.writable.iter().cloned());
    let policy = if project.env.is_local() {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let expand = |path: &String| match (path.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => home.join(rest),
            _ => PathBuf::from(path),
        };
        writable.extend(settings.writable.iter().map(expand));
        let mut hidden: Vec<PathBuf> = settings.hidden.iter().map(expand).collect();
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            hidden.push(PathBuf::from(dir));
        }
        let user_hidden = hidden.clone();
        let exposes = |path: &Path| {
            user_hidden
                .iter()
                .any(|h| path.starts_with(h) || h.starts_with(path))
        };
        writable.extend(rules.writable.iter().map(expand).filter(|p| !exposes(p)));
        // bubblewrap and the Windows launcher protect only paths that
        // exist, so a missing one inside a writable directory is created
        // first. Otherwise the sandbox could create it, and the host's git
        // or agent would run what it names.
        let placeholders = SandboxKind::native() != SandboxKind::Seatbelt;
        let mut read_only = Vec::new();
        if placeholders {
            for (path, dir) in &vcs.missing {
                create_placeholder(path, path.parent().unwrap_or(path), *dir);
            }
        }
        read_only.extend(vcs.read_only.iter().cloned());
        // Coding agents write their login, sessions and state in the home
        // directory, but their hooks and settings stay as the host has them.
        if let Some(home) = &home {
            for agent in crate::agents::Agent::all() {
                let dirs: Vec<PathBuf> = (agent.sandbox_writable().iter())
                    .map(|p| home.join(p))
                    .filter(|p| !exposes(p))
                    .collect();
                for file in agent.sandbox_read_only() {
                    let path = home.join(file);
                    // Tests pass no agent copies and leave the real home alone.
                    let create = placeholders && agents.is_some();
                    let dir = dirs.iter().find(|dir| path.starts_with(dir));
                    if let Some(dir) = dir.filter(|dir| create && dir.is_dir()) {
                        let is_dir = Path::new(file).extension().is_none();
                        create_placeholder(&path, dir, is_dir);
                    }
                    read_only.push(path);
                }
                writable.extend(dirs);
                for (skill, _) in agent.skills() {
                    read_only.extend(home.join(skill).parent().map(Path::to_path_buf));
                }
            }
        }
        hidden.extend(rules.hidden.iter().map(expand));
        // The app's directories hold keys, logins and every project's shell
        // history, whatever the settings say. The references and extensions,
        // and the agents' skill that points at them, show through read-only,
        // unless the user hid them on purpose.
        let mut app: Vec<PathBuf> = Vec::new();
        for dir in [
            &app_dirs.config,
            &app_dirs.data,
            &app_dirs.state,
            &app_dirs.cache,
            &app_dirs.logs,
        ] {
            if !app.iter().any(|known| dir.starts_with(known)) {
                app.retain(|known| !known.starts_with(dir));
                app.push(dir.clone());
            }
        }
        let hidden_inside = |path: &Path| {
            user_hidden.iter().any(|h| {
                app.iter().any(|dir| h != dir && h.starts_with(dir))
                    && (path.starts_with(h) || h.starts_with(path))
            })
        };
        let mut readable: Vec<PathBuf> = (crate::bundle::READABLE.iter())
            .map(|path| app_dirs.data.join(path))
            .chain([app_dirs.config.join("extensions")])
            .filter(|path| !hidden_inside(path))
            .collect();
        readable.dedup();
        if let Some(home) = &home {
            readable.extend(
                (crate::agents::Agent::all().into_iter())
                    .flat_map(|agent| agent.skills())
                    .filter_map(|(skill, _)| Some(home.join(skill).parent()?.to_path_buf()))
                    .filter(|dir| !exposes(dir)),
            );
        }
        // The project's own folders were picked for it, so they show
        // through the user's hidden paths. They never uncover a hidden path
        // or reach into the app's directories.
        let real =
            |path: &Path| cmdterm_env::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let both = |paths: &[PathBuf]| -> Vec<PathBuf> {
            (paths.iter())
                .flat_map(|path| [path.clone(), real(path)])
                .collect()
        };
        let (user_hidden_real, app_real) = (both(&user_hidden), both(&app));
        let picked = |path: &PathBuf| {
            [path.clone(), real(path)].iter().all(|path| {
                !user_hidden_real
                    .iter()
                    .any(|h| h != path && h.starts_with(path))
                    && !app_real
                        .iter()
                        .any(|dir| dir.starts_with(path) || path.starts_with(dir))
            })
        };
        let own = &project.sandbox;
        writable.extend(own.writable.iter().map(expand).filter(picked));
        let own_readable: Vec<PathBuf> = (own.readable.iter())
            .map(expand)
            .filter(picked)
            .filter_map(|path| cmdterm_env::canonicalize(path).ok())
            .collect();
        read_only.extend(
            (app.iter())
                .filter(|dir| writable.iter().any(|w| dir.starts_with(w)))
                .cloned(),
        );
        // The CA files and directories the app's variables name, which the
        // sandbox inherits, unless they lie in the app's directories.
        let ca_paths: Vec<PathBuf> = (cmdterm_env::host_env::ca_paths().into_iter())
            .filter_map(|path| cmdterm_env::canonicalize(path).ok())
            .filter(|path| !app_real.iter().any(|dir| path.starts_with(dir)))
            .collect();
        hidden.extend(app);
        // bwrap mounts a file only over one that exists.
        let files: Vec<(PathBuf, &String)> = (rules.files.iter())
            .map(|(inside, content)| (expand(inside), content))
            .filter(|(inside, _)| inside.is_file())
            .collect();
        let bind_files = SandboxKind::native().redirects();
        if !bind_files {
            hidden.extend(files.iter().map(|(inside, _)| inside.clone()));
        }
        let mut policy = SandboxPolicy::local(writable, &hidden, network);
        // Mounts land on real paths, like the hidden ones they show through.
        policy.readable = (readable.iter())
            .filter_map(|path| cmdterm_env::canonicalize(path).ok())
            .collect();
        // Seatbelt denies writes to a missing path too, such as a git
        // directory's `commondir`, so a missing one keeps its place under
        // its real parent. bwrap and the launcher skip missing ones.
        policy.read_only = (read_only.iter())
            .filter_map(|path| match cmdterm_env::canonicalize(path) {
                Ok(path) => Some(path),
                Err(_) if !placeholders => {
                    let parent = cmdterm_env::canonicalize(path.parent()?).ok()?;
                    Some(parent.join(path.file_name()?))
                }
                Err(_) => None,
            })
            .collect();
        // Shown through hidden paths, and kept read-only inside writable ones.
        policy.readable.extend(own_readable.iter().cloned());
        policy.readable.extend(ca_paths);
        policy.read_only.extend(own_readable);
        // Pasted images, and for Ctrl+V the shims that hand them to
        // programs asking xclip or wl-paste, see `clipboard.rs`.
        let clipboard = crate::clipboard::staging_dir(&app_dirs.cache);
        if let Ok(staged) = cmdterm_env::canonicalize(&clipboard) {
            if bind_files {
                let shims = crate::clipboard::sandbox_shims(&staged);
                policy.files.extend(shims);
            }
            policy.readable.push(staged);
        }
        policy.unset.push("DBUS_SESSION_BUS_ADDRESS".into());
        // The sandbox program inherits the app's environment. Its tokens
        // would reach every command inside, around the hidden paths and the
        // proxy's fake credentials, so the ones that look like credentials
        // go. Variables the policy or the command sets stay.
        let inherited = std::env::vars_os().map(|(name, _)| name);
        policy.unset_credentials(inherited, &passed_credentials(settings));
        let xdg_home = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(default)))
        };
        // fish keeps its history in its data directory; neovim its log,
        // swap, undo and shada files in its state directory. Its state also
        // holds Lua that plugin managers load, so the host's stays
        // read-only and each project gets its own.
        let own_dirs = [
            (xdg_home("XDG_DATA_HOME", ".local/share"), "fish"),
            (xdg_home("XDG_STATE_HOME", ".local/state"), "nvim"),
        ];
        for (base, name) in own_dirs {
            // bwrap mounts only over a path that exists, so a host without
            // the directory keeps the read-only one.
            let Some(inside) = base
                .map(|base| base.join(name))
                .filter(|p| policy.kind.redirects() && p.is_dir())
            else {
                continue;
            };
            let own = shell_dir.join(name);
            if std::fs::create_dir_all(&own).is_ok() {
                policy.redirects.push((own, inside));
            }
        }
        if std::fs::create_dir_all(shell_dir).is_ok() {
            policy.writable.push(shell_dir.to_path_buf());
            policy.vars.push((
                "HISTFILE".into(),
                shell_dir.join("history").to_string_lossy().into_owned(),
            ));
        }
        // bwrap needs both ends of a redirect; the host directory is
        // created, the sandbox path must already exist.
        if policy.kind.redirects() {
            for (host, inside) in &rules.redirects {
                let (host, inside) = (expand(host), expand(inside));
                if !exposes(&host)
                    && !exposes(&inside)
                    && inside.exists()
                    && std::fs::create_dir_all(&host).is_ok()
                {
                    policy.redirects.push((host, inside));
                }
            }
        }
        if bind_files {
            for (inside, content) in files {
                match write_sandbox_file(files_dir, &inside, content) {
                    Some(host) => policy.files.push((host, inside)),
                    // Without its stand-in the real file stays hidden.
                    None => policy.hidden_files.push(inside),
                }
            }
        }
        // Agents write copies of their settings, which come back to the
        // host after the user allows it. Their lock and temporary files
        // next to a state file in the home directory go to a scratch home.
        if let (true, Some(sync), Some(home)) = (bind_files, agents, &home) {
            let prepared = sync.prepare(project.id, home, |path| {
                exposes(path) || hidden.iter().any(|h| path.starts_with(h))
            });
            let real_home = cmdterm_env::canonicalize(home).unwrap_or_else(|_| home.clone());
            if (prepared.binds.iter()).any(|(_, inside)| inside.parent() == Some(&real_home)) {
                policy.scratch_home = Some(real_home);
            }
            policy.synced = prepared.binds;
            policy.writable.extend(prepared.dirs);
            policy.vars.extend(prepared.vars);
        }
        match settings.engines {
            EngineAccess::Passthrough => cmdterm_env::pass_engines(&mut policy),
            EngineAccess::Sidecar | EngineAccess::Off => cmdterm_env::hide_engines(&mut policy),
        }
        if let Some(sidecar) = sidecar {
            policy.sockets.push(sidecar.socket.clone());
            policy.vars.extend(sidecar.vars.iter().cloned());
        }
        policy.vars.extend(rules.vars.iter().cloned());
        // The proxy comes only when the sandbox needs it, see `Runtimes::proxy`.
        policy.proxy = proxy.cloned();
        // Login shells read the host's startup files, which may set proxy
        // variables of their own. Each stand-in is named after its content,
        // since projects' proxies differ and a bound file must not change
        // under a running sandbox.
        for (inside, content) in policy.shell_startup_stand_ins() {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (&inside, &content).hash(&mut hasher);
            let profiles = files_dir
                .join("profiles")
                .join(format!("{:016x}", hasher.finish()));
            if let Some(host) = write_sandbox_file(&profiles, &inside, &content) {
                policy.files.push((host, inside));
            }
        }
        // ssh refuses an included configuration file that neither root nor
        // the user owns, and root's files show as nobody's in the sandbox,
        // so every ssh there failed before it read a host name. Copies the
        // user owns, named after their content like the stand-ins above,
        // take their place.
        if bind_files {
            let dir = files_dir.join("ssh");
            for (inside, content) in ssh_config_stand_ins(Path::new(SSH_CONFIG_DIR)) {
                if let Some(host) = write_content_file(&dir, &inside, &content) {
                    policy.files.push((host, inside));
                }
            }
        }
        use_profile(&mut policy, project.id);
        if let Some(home) = &home {
            use_appcontainer(&mut policy, settings, project.id, home);
        }
        // A bubblewrap sandbox has a namespace of its own unless the user
        // asked for the host's network (which needs `restrict_network` off,
        // see `isolates`); its servers then bind no host ports.
        // Seatbelt keeps the host's network, and so does the Windows
        // launcher.
        policy.host_network = (settings.host_network && !settings.restrict_network)
            || policy.kind != SandboxKind::Bwrap;
        if policy.kind == SandboxKind::Bwrap {
            policy.keyring = keyring.map(Path::to_path_buf);
            policy.ports = ports.map(Path::to_path_buf);
        }
        policy
    } else {
        // The project's folders are on its machine, where `~/` means a home
        // directory this one does not know. That machine runs bwrap, so its
        // absolute paths start with `/` even when this one is Windows.
        let absolute = |list: &[String]| -> Vec<PathBuf> {
            (list.iter())
                .filter(|path| path.starts_with('/'))
                .map(PathBuf::from)
                .collect()
        };
        writable.extend(absolute(&project.sandbox.writable));
        let mut read_only = absolute(&project.sandbox.readable);
        read_only.extend(vcs.read_only);
        // The app cannot create missing ones there, so that machine does,
        // before bwrap starts.
        let placeholders = (vcs.missing.into_iter())
            .map(|(path, dir)| {
                let content = (!dir).then(|| placeholder_content(&path).to_owned());
                (path, content)
            })
            .collect();
        // Seatbelt and the Windows launcher only run on this machine, so a
        // remote host gets bwrap.
        SandboxPolicy {
            kind: SandboxKind::Bwrap,
            writable,
            read_only,
            placeholders,
            hidden_dirs: Vec::new(),
            hidden_files: Vec::new(),
            readable: Vec::new(),
            redirects: Vec::new(),
            files: Vec::new(),
            synced: Vec::new(),
            scratch_home: None,
            scratch_dirs: Vec::new(),
            sockets: Vec::new(),
            pipes: Vec::new(),
            unset: vec!["SSH_AUTH_SOCK".into()],
            vars: Vec::new(),
            network,
            // A remote host keeps the whole network of its host.
            host_network: true,
            proxy: None,
            ports: None,
            keyring: None,
            appcontainer: None,
            ports_tcp: None,
            profile: None,
            // The app cannot see that machine's environment, so a shell
            // there removes its credentials before bwrap starts.
            strip_credentials: Some(
                (passed_credentials(settings).into_iter())
                    .map(str::to_owned)
                    .collect(),
            ),
        }
    };
    Environment::Sandbox(Box::new(SandboxTarget::new(project.env.clone(), policy)))
}

/// The keyring secrets of every agent's login, see
/// [`crate::agents::Agent::keyring_logins`]. Like [`passed_credentials`],
/// they serve every sandbox, whichever agent its terminals run.
#[cfg(target_os = "linux")]
fn keyring_logins() -> Vec<Vec<(String, String)>> {
    (crate::agents::Agent::all().into_iter())
        .flat_map(|agent| agent.keyring_logins().iter())
        .map(|login| {
            (login.iter())
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        })
        .collect()
}

/// The credential-shaped variables a local sandbox keeps from the app's
/// environment: those in `sandbox.pass_env` and every agent's
/// [`crate::agents::Agent::login_vars`]. The sandbox is built for a
/// worktree before anyone knows which agent its terminals run, so it keeps
/// the logins of all of them.
fn passed_credentials(settings: &SandboxSettings) -> Vec<&str> {
    let agents = crate::agents::Agent::all().into_iter();
    (settings.pass_env.iter().map(String::as_str))
        .chain(agents.flat_map(|agent| agent.login_vars().iter().copied()))
        .collect()
}

/// What a sandbox must not change in a worktree's repository metadata. The
/// app runs git and hg on the host against the same repository, unsandboxed,
/// so a sandbox that set `core.fsmonitor`, a filter, a hook or an hg hook
/// would have the next poll run it outside. The configuration, the hooks,
/// and the files that lead git or hg to other metadata stay read-only:
/// a linked worktree's `.git` file and its `commondir` and `gitdir`, the
/// `commondir` of every other git directory, the list of worktrees, hg's
/// `sharedpath` and `requires`, and the app's list of hg shares. The
/// metadata directories themselves are bound onto themselves, so the
/// sandbox cannot rename them away and put others in their place. A `.git`
/// symlink is followed, but the link itself cannot be pinned: a sandbox may
/// replace it, and the `.git` of a worktree inside its own. The app's git
/// commands never read a worktree's `.git` for that reason. They read the
/// common git directory recorded for the project (`git_dir` in
/// `state.json`), which is protected too, see `vcs/git.rs`.
///
/// On this machine the paths come from the worktree's `.git` file. On
/// another machine the app cannot look without blocking the UI, so they
/// come from the recorded common directory, else the main worktree's
/// `.git`, and a linked worktree's administrative directory comes from the
/// last poll's list of worktrees. Before the first poll it is taken to be
/// named after the worktree, as `git worktree add` names it; when that
/// guess is wrong, git in the sandbox cannot write it.
#[derive(Debug, Default, PartialEq)]
struct VcsMetadata {
    /// Metadata directories, and a linked worktree's own administrative
    /// directory inside the read-only list of worktrees.
    writable: Vec<PathBuf>,
    read_only: Vec<PathBuf>,
    /// Read-only paths that do not exist yet, and whether each is a
    /// directory. On another machine, every read-only path that may be
    /// missing, which that machine creates before the sandbox starts.
    missing: Vec<(PathBuf, bool)>,
    local: bool,
    /// The common git directory recorded for the project.
    recorded: Option<PathBuf>,
    /// On another machine, the sandbox's worktree's git directory as the
    /// last poll found it.
    admin: Option<PathBuf>,
}

impl VcsMetadata {
    /// The metadata of `worktree` of the repository whose main worktree is
    /// `root` and whose common git directory the app recorded as `recorded`.
    /// `admin` is the worktree's git directory from the last poll, which
    /// only another machine's sandbox reads.
    fn of(
        worktree: &Path,
        root: &Path,
        local: bool,
        recorded: Option<&Path>,
        admin: Option<&Path>,
    ) -> Self {
        let real = |path: &Path| {
            (local
                .then(|| cmdterm_env::canonicalize(path).ok())
                .flatten())
            .unwrap_or_else(|| path.to_path_buf())
        };
        let (worktree, root) = (real(worktree), real(root));
        let mut meta = Self {
            local,
            recorded: recorded.map(real),
            admin: admin.map(Path::to_path_buf),
            ..Self::default()
        };
        let mut tops = vec![worktree.as_path()];
        if root != worktree {
            tops.push(root.as_path());
        }
        for top in tops {
            meta.git(top, &root);
            meta.hg(top, top == root);
        }
        // The `.git` link that led there may have been replaced since the
        // project was added.
        if let Some(common) = meta.recorded.clone()
            && (!local || common.is_dir())
            && !meta.writable.iter().any(|dir| real(dir) == common)
        {
            meta.gitdir(common);
        }
        if local {
            for path in &mut meta.writable {
                *path = real(path);
            }
        }
        for list in [&mut meta.writable, &mut meta.read_only] {
            let mut seen = Vec::new();
            list.retain(|path| {
                let new = !seen.contains(path);
                seen.push(path.clone());
                new
            });
        }
        meta
    }

    fn protect(&mut self, path: PathBuf, dir: bool) {
        if !self.local || path.symlink_metadata().is_err() {
            self.missing.push((path.clone(), dir));
        }
        self.read_only.push(path);
    }

    /// The git metadata of worktree `top` of the repository whose main
    /// worktree is `root`.
    fn git(&mut self, top: &Path, root: &Path) {
        let dot = top.join(".git");
        if !self.local {
            let common = self.recorded.clone().unwrap_or_else(|| dot.clone());
            if top == root {
                // A `.git` file or link that leads elsewhere is not bound:
                // bwrap cannot mount on a link, and the app's git does not
                // read it.
                self.gitdir(common);
                return;
            }
            self.read_only.push(dot);
            let common = self.recorded.clone().unwrap_or_else(|| root.join(".git"));
            let guess = top
                .file_name()
                .map(|name| common.join("worktrees").join(name));
            if let Some(admin) = self.admin.clone().or(guess) {
                // It stays writable inside the read-only list of worktrees.
                self.writable.push(admin.clone());
                self.read_only.push(admin.join("commondir"));
                self.read_only.push(admin.join("gitdir"));
                self.protect(admin.join("config.worktree"), false);
            }
            // The main worktree's pass covers the common directory.
            return;
        }
        let Ok(meta) = std::fs::symlink_metadata(&dot) else {
            return;
        };
        // bwrap cannot mount over a symlink, so a linked `.git` is
        // followed: what it leads to is protected, the link itself is not.
        let (dot, meta) = if meta.is_symlink() {
            let Ok(real) = cmdterm_env::canonicalize(&dot) else {
                return;
            };
            let Ok(meta) = std::fs::metadata(&real) else {
                return;
            };
            (real, meta)
        } else {
            (dot, meta)
        };
        if meta.is_dir() {
            self.gitdir(dot);
            return;
        }
        if !meta.is_file() {
            return;
        }
        self.read_only.push(dot.clone());
        // Relative to the worktree, also when `.git` is a link.
        let Some(admin) = gitdir_of(top, &dot) else {
            return;
        };
        let commondir = admin.join("commondir");
        if commondir.symlink_metadata().is_err() {
            // A git directory of its own (`git init --separate-git-dir`,
            // a submodule's checkout), not a linked worktree's.
            self.gitdir(admin);
            return;
        }
        // It stays writable inside the read-only list of worktrees.
        self.writable.push(admin.clone());
        self.read_only.push(commondir.clone());
        self.read_only.push(admin.join("gitdir"));
        self.protect(admin.join("config.worktree"), false);
        let common = std::fs::read_to_string(&commondir)
            .ok()
            .and_then(|dir| cmdterm_env::canonicalize(admin.join(dir.trim_end())).ok());
        if let Some(common) = common {
            self.common(&common);
        }
    }

    /// A git directory that is not a linked worktree's: the sandbox may
    /// write it, but not what [`Self::common`] protects.
    fn gitdir(&mut self, dir: PathBuf) {
        // git never writes `commondir` into such a directory; one that
        // names another directory came from a sandbox before it was
        // protected, so it is set back to `.` instead of locked as it is.
        let commondir = dir.join("commondir");
        if self.local
            && std::fs::read_to_string(&commondir).is_ok_and(|text| text.trim_end() != ".")
            && let Err(error) = std::fs::write(&commondir, ".\n")
        {
            eprintln!("scriptschnell-term: {}: {error}", commondir.display());
        }
        self.writable.push(dir.clone());
        self.common(&dir);
    }

    /// What git reads in a common directory that makes it run commands or
    /// look elsewhere. git reads `commondir` in any git directory, also a
    /// main one, where it is missing, and would take the common directory
    /// from it. So a missing one is created as `.`, which names the
    /// directory itself: an empty one would break git.
    fn common(&mut self, dir: &Path) {
        self.protect(dir.join("config"), false);
        self.protect(dir.join("config.worktree"), false);
        self.protect(dir.join("hooks"), true);
        self.protect(dir.join("commondir"), false);
        self.protect(dir.join("worktrees"), true);
    }

    /// hg's metadata of worktree `top`, the share source when `main` is set.
    fn hg(&mut self, top: &Path, main: bool) {
        let dot = top.join(".hg");
        if self.local && !dot.is_dir() {
            return;
        }
        self.writable.push(dot.clone());
        self.protect(dot.join("hgrc"), false);
        self.protect(dot.join("hgrc-not-shared"), false);
        self.read_only.push(dot.join("requires"));
        self.read_only.push(dot.join("sharedpath"));
        if main {
            self.protect(dot.join("cmdterm-shares"), false);
        }
    }
}

/// The administrative directory that the `.git` file `dot` of worktree
/// `top` names.
fn gitdir_of(top: &Path, dot: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dot).ok()?;
    let dir = text.strip_prefix("gitdir:")?.trim();
    cmdterm_env::canonicalize(top.join(dir)).ok()
}

/// Creates the missing read-only `path` inside `inside`, a directory the
/// sandbox may write: an empty directory, or a file that the program
/// reading it takes for no settings, such as `{}` for JSON. Parents inside
/// `inside` are created too.
fn create_placeholder(path: &Path, inside: &Path, dir: bool) {
    if path.symlink_metadata().is_ok() || !path.starts_with(inside) {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if !parent.starts_with(inside) || std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if dir {
        let _ = std::fs::create_dir(path);
        return;
    }
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        let _ = file.write_all(placeholder_content(path).as_bytes());
    }
}

/// What a placeholder file at `path` holds: no settings for the program
/// that reads it. A git directory's `commondir` names the directory itself.
fn placeholder_content(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match name {
        "mcp-config.json" | "mcp.json" | "mcp_config.json" => "{\"mcpServers\": {}}\n",
        "models.json" => "{\"providers\": {}}\n",
        _ if name.ends_with(".json") => "{}\n",
        "commondir" => ".\n",
        _ => "",
    }
}

/// Directories of the user profile that hold toolchains an AppContainer
/// may read and run from, which it cannot reach otherwise.
const APPCONTAINER_TOOLCHAINS: [&str; 9] = [
    ".cargo",
    ".rustup",
    ".dotnet",
    ".nuget",
    ".nvm",
    ".volta",
    ".deno",
    "scoop",
    "AppData/Local/Programs",
];

/// With `windows_appcontainer` set, makes the Windows launcher run the
/// command of project `id` in an AppContainer (experimental): it gets a
/// profile of its own and read access to the toolchains of the user
/// profile `home` and to the proxy's CA bundle. Other kinds are left alone.
/// The launcher falls back to the restricted token when it cannot set the
/// AppContainer up.
fn use_appcontainer(policy: &mut SandboxPolicy, settings: &SandboxSettings, id: u64, home: &Path) {
    if policy.kind != SandboxKind::Windows || !settings.windows_appcontainer {
        return;
    }
    // An AppContainer has no network of its own. Without a proxy to reach
    // (host network, or the proxy failed to start) and with the network
    // on, the restricted token keeps the host's network instead.
    if policy.proxy.is_none() && settings.network {
        return;
    }
    policy.appcontainer = Some(format!("cmdterm-sandbox-{id}"));
    let toolchains = APPCONTAINER_TOOLCHAINS.iter().map(|dir| home.join(dir));
    let bundle = policy.proxy.iter().filter_map(|p| p.ca_bundle.clone());
    for path in toolchains.chain(bundle) {
        if path.exists() && !policy.readable.contains(&path) {
            policy.readable.push(path);
        }
    }
}

/// Gives the Windows sandbox of project `id` a profile of its own, in both
/// modes. The launcher derives the restricted token's capability SID and the
/// temp dir from it, so one project's sandbox cannot write another's files.
/// Other kinds ignore profiles and are left alone.
fn use_profile(policy: &mut SandboxPolicy, id: u64) {
    if policy.kind == SandboxKind::Windows {
        policy.profile = Some(format!("cmdterm-sandbox-{id}"));
    }
}

/// Writes `content` to a file in `dir` for the sandbox path `inside` and
/// returns its path. The name comes from `inside`, so each sandbox path
/// keeps one file. An unchanged file is left alone, so sandboxes that
/// bound it keep reading it.
/// The directory whose files the system's `ssh_config` includes.
const SSH_CONFIG_DIR: &str = "/etc/ssh/ssh_config.d";

/// The readable text files in `dir` as `(path, content)`, with symbolic
/// links resolved, since bwrap mounts a file only over one that is not a
/// link. Sorted by path. Empty when `dir` is missing.
fn ssh_config_stand_ins(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = cmdterm_env::canonicalize(entry.path()).ok()?;
            if !path.is_file() {
                return None;
            }
            let content = std::fs::read_to_string(&path).ok()?;
            Some((path, content))
        })
        .collect();
    found.sort();
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

/// [`write_sandbox_file`] at a name taken from `inside` and `content`
/// both, so a file a running sandbox has bound never changes.
fn write_content_file(dir: &Path, inside: &Path, content: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    write_sandbox_file(
        &dir.join(format!("{:016x}", hasher.finish())),
        inside,
        content,
    )
}

fn write_sandbox_file(dir: &Path, inside: &Path, content: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    inside.hash(&mut hasher);
    let name = inside.file_name()?.to_string_lossy();
    let path = dir.join(format!("{:016x}-{name}", hasher.finish()));
    if std::fs::read(&path).is_ok_and(|old| old == content.as_bytes()) {
        private_file(&path).ok()?;
        return Some(path);
    }
    private_dir(dir).ok()?;
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    let mut file = file.open(&path).ok()?;
    private_file(&path).ok()?;
    std::io::Write::write_all(&mut file, content.as_bytes()).ok()?;
    Some(path)
}

/// Makes `path` readable by this user only. A stand-in for a shell startup
/// file holds the proxy URL with its password, and files made before this
/// rule may be readable by everyone.
fn private_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Creates `dir` and its missing parents for this user only, and makes
/// `dir` itself so if it was there.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    builder.create(dir)
}

impl Inner {
    /// Where sandboxes keep the files extensions stand in for theirs.
    fn sandbox_files_dir(&self) -> PathBuf {
        self.runtimes.sandbox_dir.join("files")
    }

    fn runtime_of(&self, id: u64, path: &Path) -> Runtime {
        self.state
            .project(id)
            .and_then(|project| project.worktree(path))
            .map(|config| config.runtime)
            .unwrap_or_default()
    }

    /// Where terminals of worktree `path` start. Starts the devcontainer
    /// when it is neither ready nor starting.
    pub(crate) fn terminal_env(&mut self, id: u64, path: &Path) -> TermEnv {
        let Some(project) = self.state.project(id) else {
            return TermEnv::Ready(Environment::Local);
        };
        match self.runtime_of(id, path) {
            Runtime::Host => TermEnv::Ready(project.env.clone()),
            Runtime::Sandbox if self.wants_sidecar(project) => {
                if let Some(SidecarPhase::Ready { sidecar, shares }) =
                    self.runtimes.sidecars.get(&id)
                    && has_path(shares, path)
                {
                    return TermEnv::Ready(self.sandbox(project, path, Some(sidecar)));
                }
                let shares = match self.runtimes.sidecars.get_mut(&id) {
                    Some(SidecarPhase::Starting { log, waiting, .. }) => {
                        if !has_path(waiting, path) {
                            waiting.push(path.to_path_buf());
                        }
                        return TermEnv::Waiting(log.clone());
                    }
                    // A worktree added after it started: start it again
                    // with that worktree.
                    Some(SidecarPhase::Ready { shares, .. }) => std::mem::take(shares),
                    Some(SidecarPhase::Failed { .. }) | None => self.worktree_paths(id),
                };
                let mut shares = shares;
                if !has_path(&shares, path) {
                    shares.push(path.to_path_buf());
                }
                TermEnv::Waiting(self.start_sidecar(id, shares, vec![path.to_path_buf()]))
            }
            Runtime::Sandbox => TermEnv::Ready(self.sandbox(project, path, None)),
            Runtime::Devcontainer => match self.runtimes.phases.get(&(id, path.to_path_buf())) {
                Some(Phase::Ready(env)) => TermEnv::Ready(env.clone()),
                Some(Phase::Preparing { log }) => TermEnv::Waiting(log.clone()),
                Some(Phase::Failed { .. }) | None => {
                    TermEnv::Waiting(self.start_devcontainer(id, path, false))
                }
            },
        }
    }

    /// The terminal environment of a worktree if it is ready, without
    /// starting anything.
    fn ready_env(&self, id: u64, path: &Path) -> Option<Environment> {
        let project = self.state.project(id)?;
        match self.runtime_of(id, path) {
            Runtime::Host => Some(project.env.clone()),
            Runtime::Sandbox if self.wants_sidecar(project) => {
                match self.runtimes.sidecars.get(&id)? {
                    SidecarPhase::Ready { sidecar, shares } if has_path(shares, path) => {
                        Some(self.sandbox(project, path, Some(sidecar)))
                    }
                    _ => None,
                }
            }
            Runtime::Sandbox => Some(self.sandbox(project, path, None)),
            Runtime::Devcontainer => match self.runtimes.phases.get(&(id, path.to_path_buf()))? {
                Phase::Ready(env) => Some(env.clone()),
                _ => None,
            },
        }
    }

    /// The sandbox for worktree `path` of `project`, see [`sandbox_env`].
    fn sandbox(&self, project: &Project, path: &Path, sidecar: Option<&Sidecar>) -> Environment {
        // Only a local project's sandbox reaches its own sockets.
        let scope = project.env.is_local().then_some(project.id);
        if scope.is_some() {
            self.runtimes.share_always_allowed(project);
        }
        // An SSH host's sandboxes keep its network, so they run a copy of
        // the bridge to scope themselves away from its abstract sockets.
        // The copy is made once, off this thread; sandboxes that start
        // before it is there start without it.
        if cmdterm_env::host_bridge_wanted(&project.env) {
            let host = project.env.clone();
            self.spawn_job(move || {
                let Some(local) = cmdterm_env::local_bridge_program() else {
                    return Ok(None);
                };
                if let Err(error) = cmdterm_env::install_host_bridge(&host, &local) {
                    eprintln!(
                        "scriptschnell-term: {}: no sandbox bridge: {error}",
                        host.label()
                    );
                }
                Ok(None)
            });
        }
        let mode = (self.settings.sandbox).guard_mode(cmdterm_env::AGENT_SETTINGS_GUARD, None);
        self.runtimes.agent_sync.set_mode(mode);
        let proxy = self
            .runtimes
            .proxy(&self.settings.sandbox, self.ext.sandbox_rules(), scope);
        // On Windows only an AppContainer is isolated.
        let isolated = isolates(&self.settings.sandbox)
            && (cfg!(not(windows)) || self.settings.sandbox.windows_appcontainer);
        let ports = scope
            .filter(|_| isolated)
            .and_then(|id| self.runtimes.port_hub(id, path, proxy.as_ref()));
        // The worktree's git directory as the last poll listed it.
        let git_dir = (self.snapshots.get(&project.id))
            .and_then(|snapshot| snapshot.repo.git.as_ref())
            .and_then(|git| git.git_dir(&project.root, path, project.env.is_local()));
        let mut env = sandbox_env(
            &self.settings.sandbox,
            self.ext.sandbox_rules(),
            project,
            path,
            &SandboxDirs {
                shell: &self.runtimes.shell_dir(project.id),
                files: &self.sandbox_files_dir(),
                app: &self.runtimes.dirs,
                agents: Some(&self.runtimes.agent_sync),
            },
            proxy.as_ref(),
            self.runtimes
                .keyring(&self.settings.sandbox, scope)
                .as_deref(),
            ports.as_deref(),
            sidecar,
            git_dir,
        );
        // The Windows launcher's bridge reaches the hub over loopback TCP.
        if let (Some(id), Some(_), Environment::Sandbox(target)) = (scope, &ports, &mut env)
            && cfg!(windows)
            && let Some(hub) = self.runtimes.ports.get(id, path)
        {
            target.policy.ports_tcp = hub.start_tcp().ok();
        }
        env
    }

    /// True when sandboxes of `project` run with an engine sidecar. Only
    /// local projects get one; a remote one's sandbox has no engine.
    fn wants_sidecar(&self, project: &Project) -> bool {
        self.settings.sandbox.engines == EngineAccess::Sidecar && project.env.is_local()
    }

    /// The worktrees of project `id` the last VCS snapshot lists, else the
    /// ones with a saved layout, plus the root.
    fn worktree_paths(&self, id: u64) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match self.snapshots.get(&id) {
            Some(snapshot) => (snapshot.worktrees.iter())
                .map(|worktree| worktree.path.clone())
                .collect(),
            None => (self.state.project(id).into_iter())
                .flat_map(|project| project.worktrees.iter().map(|w| w.path.clone()))
                .collect(),
        };
        if let Some(project) = self.state.project(id)
            && !has_path(&paths, &project.root)
        {
            paths.push(project.root.clone());
        }
        paths
    }

    /// Starts the engine sidecar of project `id`, sharing the worktrees
    /// `shares`, on a worker thread and returns its log file. A sidecar of
    /// the project that runs already is replaced. The terminals of `waiting`
    /// restart once it listens.
    fn start_sidecar(&mut self, id: u64, shares: Vec<PathBuf>, waiting: Vec<PathBuf>) -> PathBuf {
        let root = match self.state.project(id) {
            Some(project) => project.root.clone(),
            None => shares.first().cloned().unwrap_or_default(),
        };
        let name = crate::container::sidecar_name(&root);
        let log = self.runtimes.log_dir.join(format!("{name}.log"));
        // Linked worktrees need the repository's metadata in the root.
        let mut shared = shares.clone();
        shared.push(root.join(".git"));
        shared.push(root.join(".hg"));
        self.runtimes.sidecars.insert(
            id,
            SidecarPhase::Starting {
                log: log.clone(),
                shares,
                waiting,
            },
        );
        let _ = std::fs::create_dir_all(&self.runtimes.log_dir);
        let file = std::fs::File::create(&log);
        let sidecar = &self.settings.sandbox.sidecar;
        let kind = match sidecar.engine {
            SidecarEngine::Podman => SidecarKind::Podman,
            SidecarEngine::Docker => SidecarKind::Docker,
        };
        let image = sidecar.image.clone();
        let engine = self.settings.container_engine.clone();
        let socket_dir = self.runtimes.sidecar_dir(&root);
        let sender = self.runtimes.done.0.clone();
        let wake = self.wake_handle();
        thread::spawn(move || {
            let result = file.map_err(|e| e.to_string()).and_then(|mut file| {
                let result = crate::container::start_sidecar(crate::container::SidecarOptions {
                    host: Environment::Local,
                    project: root,
                    shared,
                    kind,
                    image,
                    engine,
                    socket_dir,
                    log: &mut file,
                })
                .map_err(|error| error.to_string());
                if let Err(error) = &result {
                    let _ = writeln!(file, "cmdterm: the sidecar failed: {error}");
                }
                result
            });
            let _ = sender.send(Done::Sidecar {
                project: id,
                result,
            });
            wake_up(&wake);
        });
        log
    }

    /// True while the active worktree's devcontainer starts.
    pub fn active_runtime_preparing(&self) -> bool {
        self.active_key().is_some_and(|key| {
            matches!(
                self.runtimes.phases.get(&key),
                Some(Phase::Preparing { .. })
            )
        })
    }

    /// The active worktree's runtime.
    pub fn active_runtime(&self) -> Runtime {
        self.active_key()
            .map(|(id, path)| self.runtime_of(id, &path))
            .unwrap_or_default()
    }

    /// Starts the devcontainer of a worktree on a worker thread and returns
    /// its log file.
    fn start_devcontainer(&mut self, id: u64, path: &Path, rebuild: bool) -> PathBuf {
        let log = self.runtimes.log_dir.join(format!(
            "devcontainer-{}.log",
            crate::container::container_name(path)
        ));
        let host = self
            .state
            .project(id)
            .map(|project| project.env.clone())
            .unwrap_or_default();
        self.runtimes.phases.insert(
            (id, path.to_path_buf()),
            Phase::Preparing { log: log.clone() },
        );
        let _ = std::fs::create_dir_all(&self.runtimes.log_dir);
        // Create the file now, so the log terminal has something to follow.
        let file = std::fs::File::create(&log);
        let sender = self.runtimes.done.0.clone();
        let wake = self.wake_handle();
        let worktree = path.to_path_buf();
        let engine = self.settings.container_engine.clone();
        let mounts = self.control_mounts(&host);
        thread::spawn(move || {
            let result = match file {
                Ok(mut file) => {
                    let _ = writeln!(
                        file,
                        "cmdterm: starting the devcontainer of {}",
                        worktree.display()
                    );
                    let result = crate::container::up(crate::container::UpOptions {
                        host,
                        worktree: worktree.clone(),
                        engine,
                        rebuild,
                        mounts,
                        log: &mut file,
                    })
                    .map(|mut up| {
                        crate::container::pass_host_env(&mut up.target, &mut file);
                        (Environment::Container(Box::new(up.target)), up.created)
                    })
                    .map_err(|error| error.to_string());
                    match &result {
                        Ok(_) => {
                            let _ = writeln!(file, "cmdterm: the devcontainer is ready");
                        }
                        Err(error) => {
                            let _ = writeln!(file, "cmdterm: the devcontainer failed: {error}");
                        }
                    }
                    result
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = sender.send(Done::Prepared {
                project: id,
                worktree,
                result,
            });
            wake_up(&wake);
        });
        log
    }

    fn spawn_job(&self, job: impl FnOnce() -> Result<Option<String>, String> + Send + 'static) {
        let sender = self.runtimes.done.0.clone();
        let wake = self.wake_handle();
        thread::spawn(move || {
            let _ = sender.send(Done::Job(job()));
            wake_up(&wake);
        });
    }

    /// Runs an extension's job on a worker thread. With `done` its result
    /// goes to that function; without, a failure becomes the notice
    /// `notice` makes of the result.
    fn spawn_ext_job(
        &self,
        done: Option<Callback>,
        notice: impl FnOnce(&JobResult) -> String + Send + 'static,
        job: impl FnOnce() -> JobResult + Send + 'static,
    ) {
        let sender = self.runtimes.done.0.clone();
        let wake = self.wake_handle();
        thread::spawn(move || {
            let result = job();
            let message = match done {
                Some(done) => Done::Callback(done, result),
                None if result.ok => Done::Job(Ok(None)),
                None => Done::Job(Err(notice(&result))),
            };
            let _ = sender.send(message);
            wake_up(&wake);
        });
    }

    /// Ends an extension's request that could not start: its `done`
    /// function gets `error`, else `error` becomes a notice.
    fn refuse_ext_job(&self, done: Option<Callback>, error: String) {
        let message = match done {
            Some(done) => Done::Callback(done, JobResult::failed(error)),
            None => Done::Job(Err(error)),
        };
        let _ = self.runtimes.done.0.send(message);
    }
}

/// True when copying `from` into `to` would land on `from` itself: a local
/// or locally sandboxed worktree shares this machine's files, so an
/// extension that copies `~/.claude` into `~` must not unpack it over the
/// original while reading it.
fn copies_onto_itself(env: &Environment, from: &Path, to: &str) -> bool {
    let local = match env {
        Environment::Local => true,
        Environment::Sandbox(target) => target.host.is_local(),
        _ => false,
    };
    if !local {
        return false;
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let dir = match to.strip_prefix('~') {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(to),
    };
    let Some(name) = from.file_name() else {
        return false;
    };
    match (
        cmdterm_env::canonicalize(dir.join(name)),
        cmdterm_env::canonicalize(from),
    ) {
        (Ok(dest), Ok(source)) => dest == source,
        _ => false,
    }
}

/// What a failed job's notice says about it: its error, else the last line
/// of its stderr, else its exit code.
fn job_error(result: &JobResult) -> String {
    if let Some(error) = &result.error {
        return error.clone();
    }
    let last = result
        .stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty());
    match (last, result.code) {
        (Some(line), Some(code)) => format!("command exited with {code}: {}", line.trim()),
        (Some(line), None) => line.trim().to_owned(),
        (None, Some(code)) => format!("command exited with {code}"),
        (None, None) => "command killed by a signal".to_owned(),
    }
}

/// True when `paths` holds `path`.
fn has_path(paths: &[PathBuf], path: &Path) -> bool {
    paths.iter().any(|p| p == path)
}

fn wake_up(wake: &Arc<OnceLock<RedrawSignal>>) {
    if let Some(signal) = wake.get() {
        signal.request_redraw();
    }
}

impl Workspace {
    /// Applies finished devcontainer starts and jobs. One `try_recv` while
    /// nothing finished.
    pub fn poll_runtimes(&self) {
        loop {
            let done = self.borrow().runtimes.done.1.try_recv();
            let Ok(done) = done else { return };
            match done {
                Done::Prepared {
                    project,
                    worktree,
                    result,
                } => self.devcontainer_done(project, &worktree, result),
                Done::Sidecar { project, result } => self.sidecar_done(project, result),
                Done::Job(Ok(Some(message))) => self.notify(message),
                Done::Job(Ok(None)) => {}
                Done::Job(Err(message)) => self.notify(message),
                Done::Callback(done, result) => {
                    let outcome = self.borrow_mut().ext.complete(done, &result);
                    if let Err(error) = outcome {
                        self.notify(error.to_string());
                    }
                }
            }
        }
    }

    fn devcontainer_done(&self, id: u64, path: &Path, result: Result<(Environment, bool), String>) {
        let tr = self.tr();
        let key = (id, path.to_path_buf());
        match result {
            Ok((env, fresh)) => {
                let name = env.label().to_owned();
                let (project, host) = {
                    let mut inner = self.borrow_mut();
                    inner.runtimes.phases.insert(key, Phase::Ready(env));
                    inner.dirty.status = true;
                    inner
                        .state
                        .project(id)
                        .map(|p| (p.name.clone(), p.env.label().to_owned()))
                        .unwrap_or_default()
                };
                self.notify((tr.devcontainer_ready)(&name));
                self.restart_terminals(id, path, false);
                self.emit(&Event::EnvironmentReady {
                    project,
                    worktree: path.to_path_buf(),
                    kind: "devcontainer".into(),
                    env: name,
                    host,
                    fresh,
                });
            }
            Err(error) => {
                {
                    let mut inner = self.borrow_mut();
                    let log = match inner.runtimes.phases.remove(&key) {
                        Some(Phase::Preparing { log } | Phase::Failed { log }) => log,
                        _ => PathBuf::new(),
                    };
                    inner.runtimes.phases.insert(key, Phase::Failed { log });
                    inner.dirty.status = true;
                }
                self.notify((tr.devcontainer_failed)(&error));
            }
        }
    }

    fn sidecar_done(&self, id: u64, result: Result<Sidecar, String>) {
        let tr = self.tr();
        match result {
            Ok(sidecar) => {
                let name = sidecar.name.clone();
                let waiting = {
                    let mut inner = self.borrow_mut();
                    inner.dirty.status = true;
                    let Some(SidecarPhase::Starting {
                        mut shares,
                        waiting,
                        ..
                    }) = inner.runtimes.sidecars.remove(&id)
                    else {
                        return;
                    };
                    // The settings may have left sidecars while it started.
                    if inner.settings.sandbox.engines != EngineAccess::Sidecar {
                        None
                    } else if waiting.iter().all(|path| has_path(&shares, path)) {
                        let ready = SidecarPhase::Ready {
                            sidecar: sidecar.clone(),
                            shares,
                        };
                        inner.runtimes.sidecars.insert(id, ready);
                        Some(waiting)
                    } else {
                        // Worktrees asked for it while it started. Starting
                        // again replaces this one.
                        for path in &waiting {
                            if !has_path(&shares, path) {
                                shares.push(path.clone());
                            }
                        }
                        inner.start_sidecar(id, shares, waiting);
                        return;
                    }
                };
                let Some(waiting) = waiting else {
                    self.borrow().spawn_job(move || {
                        crate::container::stop_sidecar(&Environment::Local, &sidecar)
                            .map(|()| None)
                            .map_err(|error| error.to_string())
                    });
                    return;
                };
                self.notify((tr.sidecar_ready)(&name));
                for path in waiting {
                    self.restart_terminals(id, &path, false);
                }
            }
            Err(error) => {
                {
                    let mut inner = self.borrow_mut();
                    let log = match inner.runtimes.sidecars.remove(&id) {
                        Some(SidecarPhase::Starting { log, .. } | SidecarPhase::Failed { log }) => {
                            log
                        }
                        _ => PathBuf::new(),
                    };
                    inner
                        .runtimes
                        .sidecars
                        .insert(id, SidecarPhase::Failed { log });
                    inner.dirty.status = true;
                }
                self.notify((tr.sidecar_failed)(&error));
            }
        }
    }

    /// Stops every running engine sidecar, on a worker thread unless
    /// `wait`, which the app uses as it quits. Their storage volumes stay.
    /// Sidecars that are still starting stop when they are ready.
    pub fn stop_sidecars(&self, wait: bool) {
        let running: Vec<Sidecar> = {
            let mut inner = self.borrow_mut();
            let sidecars = std::mem::take(&mut inner.runtimes.sidecars);
            let (running, starting): (Vec<_>, Vec<_>) = (sidecars.into_iter())
                .partition(|(_, phase)| matches!(phase, SidecarPhase::Ready { .. }));
            inner.runtimes.sidecars.extend(
                starting
                    .into_iter()
                    .filter(|(_, phase)| matches!(phase, SidecarPhase::Starting { .. })),
            );
            (running.into_iter())
                .filter_map(|(_, phase)| match phase {
                    SidecarPhase::Ready { sidecar, .. } => Some(sidecar),
                    _ => None,
                })
                .collect()
        };
        let stop = move || {
            for sidecar in &running {
                let _ = crate::container::stop_sidecar(&Environment::Local, sidecar);
            }
            Ok(None)
        };
        if wait {
            let _ = stop();
        } else {
            self.borrow().spawn_job(stop);
        }
    }

    /// Stops the terminals of a worktree and starts them again from its saved
    /// layout, so they pick up a changed runtime. With `new_sessions` the
    /// remote sessions of the terminals end and the new terminals start
    /// their own; without, terminals that waited for the runtime attach to
    /// the sessions they saved.
    pub fn restart_terminals(&self, id: u64, path: &Path, new_sessions: bool) {
        self.save();
        let active = {
            let mut inner = self.borrow_mut();
            let live = inner.live.get_mut(&id).and_then(|w| w.remove(path));
            if let Some(live) = live.filter(|_| new_sessions) {
                live.end_sessions();
            }
            // Forget the names, so the restarted terminals do not attach to
            // the sessions that are ending, and the sockets and tokens their
            // shells had, so new sessions do not share them with old relays.
            if let Some(config) = (inner.state.project_mut(id))
                .and_then(|project| project.worktrees.iter_mut().find(|w| w.path == path))
                .filter(|_| new_sessions)
            {
                for spec in &mut config.panels {
                    if let crate::state::PanelSpec::Terminal {
                        session, control, ..
                    } = spec
                    {
                        *session = None;
                        *control = None;
                    }
                }
            }
            inner.dirty.main = true;
            inner.active_key() == Some((id, path.to_path_buf()))
        };
        if active {
            self.activate_worktree(id, path);
        }
    }

    /// Sets where the active worktree's terminals run and restarts them.
    pub fn set_runtime(&self, runtime: Runtime) {
        let Some((id, path)) = self.borrow().active_key() else {
            return;
        };
        {
            let mut inner = self.borrow_mut();
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            project.worktree_mut(&path).runtime = runtime;
            inner.dirty.status = true;
        }
        if runtime == Runtime::Devcontainer {
            let host = self.borrow().state.project(id).map(|p| p.env.clone());
            if let Some(host) = host
                && let Ok(None) = crate::container::find(&host, &path)
            {
                self.notify(self.tr().no_devcontainer);
                if let Some(project) = self.borrow_mut().state.project_mut(id) {
                    project.worktree_mut(&path).runtime = Runtime::Host;
                }
                return;
            }
        }
        self.restart_terminals(id, &path, true);
        if runtime == Runtime::Sandbox {
            self.announce(id, &path, "sandbox");
        }
    }

    /// Removes the active worktree's devcontainer and builds it again.
    pub fn rebuild_devcontainer(&self) {
        let Some((id, path)) = self.borrow().active_key() else {
            return;
        };
        if self.borrow().active_runtime() != Runtime::Devcontainer {
            return;
        }
        self.borrow_mut().start_devcontainer(id, &path, true);
        self.restart_terminals(id, &path, true);
    }

    /// Sends `environment_ready` for a project environment that is not
    /// local, once per run, the first time one of its worktrees shows.
    pub fn announce_project_env(&self, id: u64, path: &Path) {
        let kind = {
            let mut inner = self.borrow_mut();
            let Some(project) = inner.state.project(id) else {
                return;
            };
            let kind = env_kind(&project.env);
            if kind == "local" || !inner.runtimes.announced.insert(id) {
                return;
            }
            kind
        };
        self.announce(id, path, kind);
    }

    fn announce(&self, id: u64, path: &Path, kind: &str) {
        let event = {
            let inner = self.borrow();
            let Some(project) = inner.state.project(id) else {
                return;
            };
            Event::EnvironmentReady {
                project: project.name.clone(),
                worktree: path.to_path_buf(),
                kind: kind.to_owned(),
                env: project.env.label().to_owned(),
                host: project.env.label().to_owned(),
                fresh: false,
            }
        };
        self.emit(&event);
    }

    /// The worktree an extension request names, else the active one.
    fn request_target(&self, worktree: Option<PathBuf>) -> Option<(u64, PathBuf)> {
        let inner = self.borrow();
        match worktree {
            Some(path) => Some((inner.find_worktree(&path)?, path)),
            None => inner.active_key(),
        }
    }

    /// `cmdterm.copy`: copies a local path into a worktree's terminal
    /// environment in the background.
    pub fn copy_into_env(
        &self,
        from: PathBuf,
        to: String,
        worktree: Option<PathBuf>,
        exclude: Vec<String>,
        done: Option<Callback>,
    ) {
        let tr = self.tr();
        let Some((env, _)) = self.request_env(worktree, false, done) else {
            return;
        };
        if copies_onto_itself(&env, &from, &to) {
            if let Some(done) = done {
                let ok = JobResult {
                    ok: true,
                    ..JobResult::default()
                };
                let _ = self.borrow().runtimes.done.0.send(Done::Callback(done, ok));
            }
            return;
        }
        let shown = from.display().to_string();
        self.borrow().spawn_ext_job(
            done,
            move |result| (tr.copy_failed)(&format!("{shown}: {}", job_error(result))),
            move || {
                let exclude: Vec<&str> = exclude.iter().map(String::as_str).collect();
                match env.copy_in(&from, &to, &exclude) {
                    Ok(()) => JobResult {
                        ok: true,
                        ..JobResult::default()
                    },
                    Err(error) => JobResult::failed(error.to_string()),
                }
            },
        );
    }

    /// The environment an extension request runs in: the worktree's
    /// terminal environment, or with `on_host` its project's. `None` when
    /// it is not there or not ready; the request then ends. Also returns
    /// the worktree's path.
    fn request_env(
        &self,
        worktree: Option<PathBuf>,
        on_host: bool,
        done: Option<Callback>,
    ) -> Option<(Environment, PathBuf)> {
        let tr = self.tr();
        let Some((id, path)) = self.request_target(worktree.clone()) else {
            if done.is_some() {
                let shown = worktree.unwrap_or_default().display().to_string();
                self.borrow()
                    .refuse_ext_job(done, (tr.no_worktree_at)(&shown));
            }
            return None;
        };
        let inner = self.borrow();
        let env = if on_host {
            inner.state.project(id).map(|project| project.env.clone())
        } else {
            inner.ready_env(id, &path)
        };
        if env.is_none() {
            inner.refuse_ext_job(done, tr.env_not_ready.to_owned());
        }
        Some((env?, path))
    }

    /// `term.run`: runs a command in a worktree's terminal environment in
    /// the background, from the worktree unless `cwd` says otherwise. With
    /// `on_host` it runs in the project's environment instead, the machine
    /// under a devcontainer or sandbox.
    pub fn run_in_env(
        &self,
        command: Vec<String>,
        worktree: Option<PathBuf>,
        cwd: Option<String>,
        on_host: bool,
        done: Option<Callback>,
    ) {
        let tr = self.tr();
        let Some((env, path)) = self.request_env(worktree, on_host, done) else {
            return;
        };
        let shown = command.join(" ");
        self.borrow().spawn_ext_job(
            done,
            move |result| (tr.run_failed)(&format!("{shown}: {}", job_error(result))),
            move || {
                let cwd = cwd.map_or(path, PathBuf::from);
                match env
                    .command(&command[0])
                    .args(&command[1..])
                    .cwd(cwd)
                    .output()
                {
                    Ok(output) => {
                        JobResult::from_output(output.status.code(), &output.stdout, &output.stderr)
                    }
                    Err(error) => JobResult::failed(format!("could not start command: {error}")),
                }
            },
        );
    }

    /// `term.trust`: adds the PEM certificate at `path` on this machine to
    /// the trust store of a worktree's terminal environment.
    pub fn trust_in_env(
        &self,
        name: String,
        path: PathBuf,
        worktree: Option<PathBuf>,
        done: Option<Callback>,
    ) {
        let tr = self.tr();
        let Some((env, _)) = self.request_env(worktree, false, done) else {
            return;
        };
        let shown = path.display().to_string();
        self.borrow().spawn_ext_job(
            done,
            move |result| (tr.trust_failed)(&format!("{shown}: {}", job_error(result))),
            move || {
                let pem = match std::fs::read(&path) {
                    Ok(pem) => pem,
                    Err(error) => return JobResult::failed(format!("cannot read it: {error}")),
                };
                match env.trust_certificate(&name, &pem) {
                    Ok(bundle) => JobResult {
                        ok: true,
                        code: Some(0),
                        stdout: bundle,
                        ..JobResult::default()
                    },
                    Err(error) => JobResult::failed(error.to_string()),
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmdterm_env::GuardKind;

    #[cfg(unix)]
    #[test]
    fn sandbox_files_are_for_this_user_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("files/profiles/0123");
        let inside = Path::new("/etc/profile");
        let content = "export HTTP_PROXY='http://cmdterm:secret@127.0.0.1:1'\n";
        let path = write_sandbox_file(&dir, inside, content).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert_eq!((mode(&path), mode(&dir)), (0o600, 0o700));
        // Files and directories an older version made readable by everyone
        // are fixed, also when their content stays.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            write_sandbox_file(&dir, inside, content),
            Some(path.clone())
        );
        assert_eq!(mode(&path), 0o600);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_sandbox_file(&dir, inside, "changed").unwrap();
        assert_eq!((mode(&path), mode(&dir)), (0o600, 0o700));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed");
    }

    /// The ssh configuration files come out with links resolved, and each
    /// content gets a copy of its own, so a bound copy never changes.
    #[cfg(unix)]
    #[test]
    fn ssh_config_stand_ins_resolve_links_and_keep_old_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let base = cmdterm_env::canonicalize(tmp.path()).unwrap();
        let (dir, lib) = (base.join("ssh_config.d"), base.join("lib"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("20-proxy.conf"), "Host unix/*\n").unwrap();
        std::os::unix::fs::symlink(lib.join("20-proxy.conf"), dir.join("20-proxy.conf")).unwrap();
        std::fs::write(dir.join("99-vpn.conf"), "Host *.vpn\n").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let found = ssh_config_stand_ins(&dir);
        assert_eq!(
            found,
            [
                (lib.join("20-proxy.conf"), "Host unix/*\n".to_owned()),
                (dir.join("99-vpn.conf"), "Host *.vpn\n".to_owned()),
            ]
        );
        assert!(ssh_config_stand_ins(&base.join("missing")).is_empty());

        let files = base.join("files/ssh");
        let inside = &found[0].0;
        let old = write_content_file(&files, inside, "one").unwrap();
        let new = write_content_file(&files, inside, "two").unwrap();
        assert_ne!(old, new);
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "one");
        assert_eq!(write_content_file(&files, inside, "one"), Some(old));
    }

    /// ssh reads this machine's configuration in a sandbox, though root's
    /// files there belong to nobody.
    #[cfg(target_os = "linux")]
    #[test]
    fn ssh_reads_the_system_configuration_in_a_sandbox() {
        let works = |program: &str, args: &[&str]| {
            std::process::Command::new(program)
                .args(args)
                .output()
                .is_ok_and(|out| out.status.success())
        };
        if !works("bwrap", &["--ro-bind", "/", "/", "--unshare-all", "true"])
            || !works("ssh", &["-V"])
        {
            eprintln!("no bwrap or no ssh here; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let base = cmdterm_env::canonicalize(dir.path()).unwrap();
        let path = |name: &str| base.join(name);
        std::fs::create_dir_all(path("repo")).unwrap();
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": path("repo"), "vcs": "git",
        }))
        .unwrap();
        let env = sandbox_env(
            &SandboxSettings::default(),
            &SandboxRules::default(),
            &project,
            &path("repo"),
            &SandboxDirs {
                shell: &path("shell"),
                files: &path("files"),
                app: &cmdterm_env::AppDirs::under(&path("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            None,
        );
        let out = env
            .command("ssh")
            .args(["-G", "localhost"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn sandboxes_keep_the_agents_logins_and_the_names_the_user_passes() {
        let settings = SandboxSettings {
            pass_env: vec!["my_deploy_token".into()],
            ..SandboxSettings::default()
        };
        let kept = passed_credentials(&settings);
        let inherited = [
            "MY_DEPLOY_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "AWS_SESSION_TOKEN",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
        ];
        let mut policy = SandboxPolicy::local(vec![], &[], true);
        policy.unset.clear();
        policy.unset_credentials(inherited.map(std::ffi::OsString::from), &kept);
        assert_eq!(
            policy.unset,
            ["GITHUB_TOKEN", "GH_TOKEN", "AWS_SESSION_TOKEN"]
        );

        // Every login variable looks like a credential, or naming it would
        // be pointless.
        for agent in crate::agents::Agent::all() {
            let names = agent.login_vars().iter().map(std::ffi::OsString::from);
            let mut policy = SandboxPolicy::local(vec![], &[], true);
            policy.unset.clear();
            policy.unset_credentials(names, &[] as &[&str]);
            assert_eq!(policy.unset, agent.login_vars(), "{}", agent.name());
        }
    }

    #[test]
    fn appcontainer_mode_reads_toolchains_and_the_ca_bundle() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".cargo")).unwrap();
        let bundle = home.path().join("proxy-ca.pem");
        std::fs::write(&bundle, "x").unwrap();
        let mut policy = SandboxPolicy::local(vec![], &[], true);
        policy.kind = SandboxKind::Windows;
        policy.proxy = Some(SandboxProxy {
            socket: PathBuf::from("unused"),
            port: 1,
            ca_bundle: Some(bundle.clone()),
            token: None,
        });
        let mut settings = SandboxSettings::default();

        // Off by default: nothing changes.
        use_appcontainer(&mut policy, &settings, 7, home.path());
        assert_eq!(policy.appcontainer, None);
        assert!(policy.readable.is_empty());

        settings.windows_appcontainer = true;
        use_appcontainer(&mut policy, &settings, 7, home.path());
        assert_eq!(policy.appcontainer.as_deref(), Some("cmdterm-sandbox-7"));
        // Only toolchain directories that exist, and the bundle.
        assert_eq!(policy.readable, [home.path().join(".cargo"), bundle]);

        // Without a proxy and with the network on (host network), the
        // restricted token keeps the host's network.
        let mut host = SandboxPolicy::local(vec![], &[], true);
        host.kind = SandboxKind::Windows;
        use_appcontainer(&mut host, &settings, 7, home.path());
        assert_eq!(host.appcontainer, None);

        // Other kinds ignore the setting.
        let mut bwrap = SandboxPolicy::local(vec![], &[], true);
        bwrap.kind = SandboxKind::Bwrap;
        use_appcontainer(&mut bwrap, &settings, 7, home.path());
        assert_eq!(bwrap.appcontainer, None);
    }

    #[test]
    fn windows_sandboxes_get_a_profile_per_project() {
        let home = tempfile::tempdir().unwrap();
        for appcontainer in [false, true] {
            let mut policy = SandboxPolicy::local(vec![], &[], true);
            policy.kind = SandboxKind::Windows;
            policy.proxy = Some(SandboxProxy {
                socket: PathBuf::from("unused"),
                port: 1,
                ca_bundle: None,
                token: None,
            });
            let settings = SandboxSettings {
                windows_appcontainer: appcontainer,
                ..SandboxSettings::default()
            };
            use_profile(&mut policy, 7);
            use_appcontainer(&mut policy, &settings, 7, home.path());
            assert_eq!(policy.profile.as_deref(), Some("cmdterm-sandbox-7"));
            assert_eq!(
                policy.appcontainer.as_deref(),
                appcontainer.then_some("cmdterm-sandbox-7")
            );
        }
        for kind in [SandboxKind::Bwrap, SandboxKind::Seatbelt] {
            let mut policy = SandboxPolicy::local(vec![], &[], true);
            policy.kind = kind;
            use_profile(&mut policy, 7);
            assert_eq!(policy.profile, None);
        }
    }

    #[test]
    fn copying_a_local_path_onto_itself_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "{}").unwrap();
        let to = dir.path().to_str().unwrap();
        assert!(copies_onto_itself(&Environment::Local, &file, to));
        assert!(!copies_onto_itself(
            &Environment::Local,
            &file,
            "/elsewhere"
        ));
        let vm = Environment::Vm(cmdterm_env::VmTarget { name: "x".into() });
        assert!(!copies_onto_itself(&vm, &file, to));
    }

    #[test]
    fn extensions_add_paths_but_cannot_open_the_users_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        // Hidden paths are canonical, and the temporary directory has a
        // short name on Windows runners (RUNNER~1).
        let base = cmdterm_env::canonicalize(dir.path()).unwrap();
        let path = |name: &str| base.join(name);
        for name in ["secret", "target", "repo"] {
            std::fs::create_dir_all(path(name)).unwrap();
        }
        let text = |name: &str| path(name).to_string_lossy().into_owned();
        let settings = SandboxSettings {
            hidden: vec![text("secret")],
            writable: Vec::new(),
            ..SandboxSettings::default()
        };
        std::fs::write(path("secret/npmrc"), "real").unwrap();
        let rules = SandboxRules {
            writable: vec![text("cache"), text("secret/sub")],
            hidden: vec![text("target")],
            redirects: vec![
                (text("own"), text("target")),
                (text("secret/x"), text("target")),
                (text("own2"), text("missing")),
            ],
            vars: vec![("NPM_TOKEN".into(), "fake".into())],
            files: vec![
                (text("secret/npmrc"), "token=fake".into()),
                (text("missing-file"), "x".into()),
            ],
            ..SandboxRules::default()
        };
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": path("repo"), "vcs": "git",
        }))
        .unwrap();
        let files = path("files");
        let env = sandbox_env(
            &settings,
            &rules,
            &project,
            &path("repo"),
            &SandboxDirs {
                shell: &path("shell"),
                files: &files,
                app: &cmdterm_env::AppDirs::under(&path("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            None,
        );
        let Environment::Sandbox(target) = env else {
            panic!("not a sandbox");
        };
        let policy = &target.policy;
        assert!(policy.writable.contains(&path("cache")));
        assert!(!policy.writable.contains(&path("secret/sub")));
        assert!(policy.hidden_dirs.contains(&path("target")));
        assert!(policy.hidden_dirs.contains(&path("secret")));
        if policy.kind.redirects() {
            assert!(policy.redirects.contains(&(path("own"), path("target"))));
            assert!(path("own").is_dir());
            assert!(
                !policy
                    .redirects
                    .iter()
                    .any(|(host, _)| host == &path("secret/x"))
            );
            assert!(
                !policy
                    .redirects
                    .iter()
                    .any(|(_, inside)| inside == &path("missing"))
            );
            // A file stands in even under a hidden path, from outside the
            // sandbox's writable paths. The others are clipboard shims and
            // copies of the system's ssh configuration.
            let stand_ins: Vec<_> = (policy.files.iter())
                .filter(|(host, _)| !host.to_string_lossy().contains("clipboard"))
                .filter(|(host, _)| !host.starts_with(files.join("ssh")))
                .collect();
            assert_eq!(stand_ins.len(), 1);
            let (host, inside) = stand_ins[0];
            assert_eq!(inside, &path("secret/npmrc"));
            assert!(host.starts_with(&files));
            assert_eq!(std::fs::read_to_string(host).unwrap(), "token=fake");
        } else {
            assert!(
                policy.hidden_files.contains(&path("secret/npmrc"))
                    || policy.kind == SandboxKind::Windows
            );
        }
        assert!(policy.vars.contains(&("NPM_TOKEN".into(), "fake".into())));
        assert_eq!(policy.proxy, None);
    }

    #[test]
    fn a_projects_own_folders_show_through_hidden_paths_but_not_the_apps() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let text = |name: &str| path(name).to_string_lossy().into_owned();
        for name in [
            "repo",
            "shared",
            "secret/known",
            "home/inner",
            "config/keys",
        ] {
            std::fs::create_dir_all(path(name)).unwrap();
        }
        std::fs::create_dir_all(path("home/.ssh")).unwrap();
        let settings = SandboxSettings {
            hidden: vec![text("secret"), text("home/.ssh")],
            ..SandboxSettings::default()
        };
        let mut project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": path("repo"), "vcs": "git",
        }))
        .unwrap();
        project.sandbox.writable = vec![text("shared"), text("home"), text("config/keys")];
        project.sandbox.readable = vec![text("secret/known"), text("missing")];
        let env = sandbox_env(
            &settings,
            &SandboxRules::default(),
            &project,
            &path("repo"),
            &SandboxDirs {
                shell: &path("shell"),
                files: &path("files"),
                app: &cmdterm_env::AppDirs::under(&path("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            None,
        );
        let Environment::Sandbox(target) = env else {
            panic!("not a sandbox");
        };
        let policy = &target.policy;
        let real = |name: &str| cmdterm_env::canonicalize(path(name)).unwrap();
        assert!(policy.writable.contains(&path("shared")));
        // `home` would uncover `home/.ssh`, `config/keys` lies in the app's.
        assert!(!policy.writable.contains(&path("home")));
        assert!(!policy.writable.contains(&path("config/keys")));
        assert!(policy.readable.contains(&real("secret/known")));
        assert!(policy.read_only.contains(&real("secret/known")));
        assert!(!policy.readable.iter().any(|p| p.ends_with("missing")));

        // On another machine only absolute paths count, unexpanded.
        project.env = Environment::Ssh(cmdterm_env::SshTarget::new("box"));
        project.sandbox.writable = vec!["/srv/data".into(), "~/cache".into()];
        project.sandbox.readable = vec!["/etc/creds".into()];
        let env = sandbox_env(
            &settings,
            &SandboxRules::default(),
            &project,
            &path("repo"),
            &SandboxDirs {
                shell: &path("shell"),
                files: &path("files"),
                app: &cmdterm_env::AppDirs::under(&path("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            None,
        );
        let Environment::Sandbox(target) = env else {
            panic!("not a sandbox");
        };
        // The repository's metadata directories are pinned and its
        // configuration stays read-only, see `VcsMetadata`.
        assert_eq!(
            target.policy.writable,
            [
                path("repo"),
                path("repo").join(".git"),
                path("repo").join(".hg"),
                "/srv/data".into()
            ]
        );
        assert_eq!(target.policy.read_only[0], PathBuf::from("/etc/creds"));
        assert!(target.policy.read_only.contains(&path("repo/.git/config")));
    }

    #[test]
    fn missing_protected_paths_get_placeholders_that_mean_no_settings() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join(".agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("config.toml"), "kept = 1\n").unwrap();
        for (name, is_dir) in [
            ("settings.json", false),
            ("mcp.json", false),
            ("models.json", false),
            ("AGENTS.md", false),
            ("config.toml", false),
            ("hooks", true),
            ("nested/deep/skills", true),
        ] {
            create_placeholder(&agent.join(name), &agent, is_dir);
        }
        let read = |name: &str| std::fs::read_to_string(agent.join(name)).unwrap();
        assert_eq!(read("settings.json"), "{}\n");
        assert_eq!(read("mcp.json"), "{\"mcpServers\": {}}\n");
        assert_eq!(read("models.json"), "{\"providers\": {}}\n");
        assert_eq!(read("AGENTS.md"), "");
        assert_eq!(read("config.toml"), "kept = 1\n", "an existing file stays");
        assert!(agent.join("hooks").is_dir() && agent.join("nested/deep/skills").is_dir());
        // Nothing outside the writable directory.
        create_placeholder(&dir.path().join("other/settings.json"), &agent, false);
        assert!(!dir.path().join("other").exists());
    }

    /// A repository with a commit and a linked worktree `wt` beside it.
    #[cfg(unix)]
    fn repo_with_worktree(dir: &Path) -> (PathBuf, PathBuf) {
        let (repo, wt) = (dir.join("repo"), dir.join("wt"));
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&repo, &["worktree", "add", "-q", "-b", "wt", "../wt"]);
        (repo, wt)
    }

    #[cfg(unix)]
    #[test]
    fn git_metadata_that_runs_commands_stays_read_only() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&dir);
        let meta = VcsMetadata::of(&wt, &repo, true, None, None);
        let git = repo.join(".git");
        let admin = git.join("worktrees/wt");
        for path in [
            wt.join(".git"),
            git.join("config"),
            git.join("config.worktree"),
            git.join("hooks"),
            git.join("worktrees"),
            admin.join("commondir"),
            admin.join("gitdir"),
            admin.join("config.worktree"),
            git.join("commondir"),
        ] {
            assert!(meta.read_only.contains(&path), "{path:?}: {meta:?}");
        }
        // git would take the main directory's own `commondir` too.
        assert!(meta.missing.contains(&(git.join("commondir"), false)));
        assert_eq!(placeholder_content(&git.join("commondir")), ".\n");
        assert!(meta.writable.contains(&admin) && meta.writable.contains(&git));
        assert!(meta.missing.contains(&(git.join("config.worktree"), false)));
        assert!(
            !meta
                .missing
                .iter()
                .any(|(path, _)| path == &git.join("config"))
        );
        // Without .hg there is nothing of hg's.
        assert!(!meta.writable.iter().any(|p| p.ends_with(".hg")));
        // A `commondir` an older sandbox planted in the main directory is
        // set back to `.`, not locked as it is.
        std::fs::write(git.join("commondir"), "/elsewhere\n").unwrap();
        VcsMetadata::of(&wt, &repo, true, None, None);
        assert_eq!(
            std::fs::read_to_string(git.join("commondir")).unwrap(),
            ".\n"
        );

        // On another machine the administrative directory is guessed
        // before the first poll. It stays writable inside the read-only list
        // of worktrees.
        let remote = VcsMetadata::of(&wt, &repo, false, None, None);
        assert!(remote.read_only.contains(&wt.join(".git")));
        assert!(remote.read_only.contains(&admin.join("commondir")));
        assert!(remote.read_only.contains(&git.join("hooks")));
        assert!(remote.read_only.contains(&git.join("worktrees")));
        assert!(remote.writable.contains(&admin));
        // The poll's list names it, whatever the worktree is called.
        let listed = git.join("worktrees/other");
        let remote = VcsMetadata::of(&wt, &repo, false, None, Some(&listed));
        assert!(remote.writable.contains(&listed) && !remote.writable.contains(&admin));
        assert!(remote.read_only.contains(&listed.join("gitdir")));
        // That machine creates what may be missing before bwrap starts.
        assert!(remote.missing.contains(&(git.join("commondir"), false)));
        assert!(remote.missing.contains(&(git.join("hooks"), true)));
        let main = VcsMetadata::of(&repo, &repo, false, None, None);
        assert!(main.read_only.contains(&git.join("worktrees")));
        assert!(main.read_only.contains(&git.join("commondir")));
    }

    /// `git` for the tests, without the user's configuration.
    #[cfg(unix)]
    fn run_git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_directories_outside_the_worktree_and_behind_links_are_protected() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        // A `.git` file that leads to a git directory of its own, as
        // `--separate-git-dir` and a submodule's checkout leave it.
        let (repo, sep) = (dir.join("repo"), dir.join("sep.git"));
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "--separate-git-dir", "../sep.git"]);
        let meta = VcsMetadata::of(&repo, &repo, true, None, None);
        assert!(meta.writable.contains(&sep), "{meta:?}");
        for path in [
            repo.join(".git"),
            sep.join("config"),
            sep.join("hooks"),
            sep.join("worktrees"),
            sep.join("commondir"),
        ] {
            assert!(meta.read_only.contains(&path), "{path:?}: {meta:?}");
        }
        assert!(meta.missing.contains(&(sep.join("commondir"), false)));

        // A `.git` symlink is followed.
        let (linked, real) = (dir.join("linked"), dir.join("real.git"));
        std::fs::create_dir_all(&linked).unwrap();
        run_git(&dir, &["init", "-q", "--bare", "real.git"]);
        run_git(&real, &["config", "core.bare", "false"]);
        std::os::unix::fs::symlink(&real, linked.join(".git")).unwrap();
        let meta = VcsMetadata::of(&linked, &linked, true, None, None);
        assert!(meta.writable.contains(&real), "{meta:?}");
        assert!(meta.read_only.contains(&real.join("config")), "{meta:?}");
        assert!(meta.read_only.contains(&real.join("commondir")), "{meta:?}");
        // bwrap cannot mount over the link itself.
        assert!(!meta.read_only.contains(&linked.join(".git")), "{meta:?}");
    }

    /// Whether bwrap and git work here.
    #[cfg(target_os = "linux")]
    fn bwrap_and_git() -> bool {
        let bwrap = std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-all", "true"])
            .output();
        let git = std::process::Command::new("git").arg("--version").output();
        let found = bwrap.is_ok_and(|o| o.status.success()) && git.is_ok();
        if !found {
            eprintln!("bwrap or git is missing; skipped");
        }
        found
    }

    /// The project of the git repository at `repo` on `env`, with the git
    /// directory recorded as the app records it when the project is added.
    #[cfg(target_os = "linux")]
    fn recorded_project(env: &Environment, repo: &Path) -> Project {
        let known = (crate::vcs::discover(env, repo).unwrap().known()).unwrap();
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "env": env, "root": known.root, "vcs": "git",
            "git_dir": known.git_dir,
        }))
        .unwrap()
    }

    /// The sandbox of worktree `wt` of `project`, with no network, or
    /// `None` when it is no bwrap sandbox.
    #[cfg(target_os = "linux")]
    fn project_sandbox(dir: &Path, project: &Project, wt: &Path) -> Option<Environment> {
        polled_sandbox(dir, project, wt, None)
    }

    /// [`project_sandbox`], with the worktree's git directory from a poll.
    #[cfg(target_os = "linux")]
    fn polled_sandbox(
        dir: &Path,
        project: &Project,
        wt: &Path,
        git_dir: Option<&Path>,
    ) -> Option<Environment> {
        let settings = SandboxSettings {
            network: false,
            ..SandboxSettings::default()
        };
        let env = sandbox_env(
            &settings,
            &SandboxRules::default(),
            project,
            wt,
            &SandboxDirs {
                shell: &dir.join("shell"),
                files: &dir.join("files"),
                app: &cmdterm_env::AppDirs::under(&dir.join("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            git_dir,
        );
        matches!(&env, Environment::Sandbox(t) if t.policy.kind == SandboxKind::Bwrap)
            .then_some(env)
    }

    /// The sandbox of a local project's worktree `wt` of the repository
    /// whose main worktree is `repo`, with no network, or `None` without
    /// bwrap and git.
    #[cfg(target_os = "linux")]
    fn git_sandbox(dir: &Path, repo: &Path, wt: &Path) -> Option<Environment> {
        if !bwrap_and_git() {
            return None;
        }
        project_sandbox(dir, &recorded_project(&Environment::Local, repo), wt)
    }

    /// Runs `script` in `env` in `cwd`, without the user's git settings,
    /// and returns what it printed.
    #[cfg(target_os = "linux")]
    fn sandboxed(env: &Environment, cwd: &Path, script: &str) -> String {
        let script = format!(
            "export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 \
             GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t \
             GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t; {script}"
        );
        let out = env
            .command("sh")
            .args(["-c", &script])
            .cwd(cwd)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        stdout
    }

    /// A sandbox on an SSH host starts from that host's environment. The
    /// app's own policy, run through real bubblewrap over an ssh stand-in,
    /// drops what looks like a credential there, also under a name the
    /// shell cannot unset, and keeps listed names and what the app sets.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandbox_on_an_ssh_host_loses_that_hosts_credentials() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        if !bwrap_and_git() {
            return;
        }
        run_git(&repo, &["init", "-q"]);
        // Runs the command with the environment a login on the host has.
        let shim = dir.join("fake-ssh");
        let script = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in \
             -o|-p|-i|-l|-F|-J|-L|-R|-D) shift 2 ;; -*) shift ;; *) break ;; esac; done\n\
             shift\nexec env GITHUB_TOKEN=host-real AWS_SECRET_ACCESS_KEY=host-real \
             'MY-TOKEN=host-real' HF_TOKEN=host-kept PLAIN_SETTING=host-plain \
             sh -c \"$1\"\n";
        std::fs::write(&shim, script).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..100 {
            let ran = std::process::Command::new(&shim)
                .args(["x", "true"])
                .status();
            if !ran.is_err_and(|error| error.raw_os_error() == Some(26)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ssh = Environment::Ssh(cmdterm_env::SshTarget {
            program: Some(shim.display().to_string()),
            ..cmdterm_env::SshTarget::new("fakehost")
        });
        let project = recorded_project(&ssh, &repo);
        let settings = SandboxSettings {
            network: false,
            pass_env: vec!["hf_token".into()],
            ..SandboxSettings::default()
        };
        let env = sandbox_env(
            &settings,
            &SandboxRules::default(),
            &project,
            &repo,
            &SandboxDirs {
                shell: &dir.join("shell"),
                files: &dir.join("files"),
                app: &cmdterm_env::AppDirs::under(&dir.join("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            None,
        );
        let Environment::Sandbox(target) = &env else {
            panic!("no sandbox on the ssh host");
        };
        assert!(target.policy.strip_credentials.is_some());
        let out = env
            .command("sh")
            .args(["-c", "env | cut -d= -f1 | sort"])
            .var("SET_TOKEN", "app-set")
            .cwd(&repo)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let names = String::from_utf8_lossy(&out.stdout);
        let names: Vec<&str> = names.lines().collect();
        assert!(out.status.success(), "{stderr}");
        for gone in ["GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "MY-TOKEN"] {
            assert!(
                !names.contains(&gone),
                "{gone} reached the sandbox: {names:?}"
            );
        }
        for kept in ["HF_TOKEN", "PLAIN_SETTING", "SET_TOKEN"] {
            assert!(
                names.contains(&kept),
                "{kept} is missing: {names:?} {stderr}"
            );
        }
    }

    /// git reads `commondir` in a main git directory too: a sandbox that
    /// wrote one there could point the app's polls at a copy of the
    /// metadata with a filter of its own.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandbox_cannot_point_git_at_another_common_directory() {
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("f"), "a").unwrap();
        run_git(&repo, &["add", "f"]);
        run_git(&repo, &["commit", "-q", "-m", "first"]);
        let Some(env) = git_sandbox(&dir, &repo, &repo) else {
            return;
        };
        let pwned = dir.join("PWNED");
        let script = format!(
            "cp -r .git evil && mkdir -p evil/info && \
             git config -f evil/config filter.x.clean 'touch {p}; cat' && \
             echo '* filter=x' > evil/info/attributes && touch f && \
             {{ echo {r}/evil > .git/commondir 2>/dev/null || echo commondir; }}; \
             git commit -q --allow-empty -m s && echo committed",
            p = pwned.display(),
            r = repo.display(),
        );
        assert_eq!(sandboxed(&env, &repo, &script), "commondir\ncommitted\n");
        assert_eq!(
            std::fs::read_to_string(repo.join(".git/commondir")).unwrap(),
            ".\n"
        );
        // The app's own discovery and poll, on the host.
        let found = crate::vcs::discover(&Environment::Local, &repo).unwrap();
        assert_eq!(found.root, repo);
        let status = found.status(&repo);
        assert!(status.is_ok(), "{status:?}");
        assert!(!pwned.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_separate_or_linked_git_directory_keeps_its_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (repo, sep) = (dir.join("repo"), dir.join("sep.git"));
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q", "--separate-git-dir", "../sep.git"]);
        let Some(env) = git_sandbox(&dir, &repo, &repo) else {
            return;
        };
        let script = format!(
            "git config core.fsmonitor x 2>/dev/null || echo config; \
             echo x > {g}/hooks/post-commit 2>/dev/null || echo hooks; \
             echo /tmp > {g}/commondir 2>/dev/null || echo commondir; \
             mv {g} {d}/moved 2>/dev/null || echo pinned; \
             git commit -q --allow-empty -m s && echo committed",
            g = sep.display(),
            d = dir.display(),
        );
        assert_eq!(
            sandboxed(&env, &repo, &script),
            "config\nhooks\ncommondir\npinned\ncommitted\n"
        );
        assert!(
            !std::fs::read_to_string(sep.join("config"))
                .unwrap()
                .contains("fsmonitor")
        );

        // A `.git` symlink leads to protected metadata.
        let (linked, real) = (dir.join("linked"), dir.join("real.git"));
        std::fs::create_dir_all(&linked).unwrap();
        run_git(&dir, &["init", "-q", "--bare", "real.git"]);
        run_git(&real, &["config", "core.bare", "false"]);
        std::os::unix::fs::symlink(&real, linked.join(".git")).unwrap();
        let Some(env) = git_sandbox(&dir, &linked, &linked) else {
            return;
        };
        let script = format!(
            "git config core.fsmonitor x 2>/dev/null || echo config; \
             echo /tmp > {g}/commondir 2>/dev/null || echo commondir; \
             git commit -q --allow-empty -m s && echo committed",
            g = real.display(),
        );
        assert_eq!(
            sandboxed(&env, &linked, &script),
            "config\ncommondir\ncommitted\n"
        );
    }

    /// Commands that make a git directory of the sandbox's own in `.git`
    /// of `$1`, a worktree, from a copy of `$2`, with a filter that
    /// touches `$3` on every file, and touch a file of the worktree, `$4`,
    /// so the next `git status` runs the filter on it.
    #[cfg(target_os = "linux")]
    const PLANT: &str = "w=$1 g=$2 p=$3 f=$4; rm -rf \"$w/.git\" && mkdir -p \"$w\" && cp -r \"$g\" \"$w/.git\" && \
        mkdir -p \"$w/.git/info\" && echo '* filter=x' > \"$w/.git/info/attributes\" && \
        git config -f \"$w/.git/config\" filter.x.clean \"touch $p; cat\" && touch \"$f\"";

    /// bwrap cannot mount on a `.git` symlink, so a sandbox can put a git
    /// directory of its own in its place. The app's polls read the git
    /// directory recorded for the project, not the link.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_replaced_git_link_runs_nothing_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (linked, real) = (dir.join("linked"), dir.join("real.git"));
        std::fs::create_dir_all(&linked).unwrap();
        run_git(&dir, &["init", "-q", "--bare", "real.git"]);
        run_git(&real, &["config", "core.bare", "false"]);
        std::os::unix::fs::symlink(&real, linked.join(".git")).unwrap();
        std::fs::write(linked.join("f"), "a").unwrap();
        run_git(&linked, &["add", "f"]);
        run_git(&linked, &["commit", "-q", "-m", "first"]);
        let Some(env) = git_sandbox(&dir, &linked, &linked) else {
            return;
        };
        let project = recorded_project(&Environment::Local, &linked);
        assert_eq!(project.git_dir.as_deref(), Some(real.as_path()));
        let known = project.known_repo();
        let intact = crate::vcs::open(&Environment::Local, &linked, false, known.as_ref());
        assert!(intact.unwrap().replaced_dot_git().is_empty());
        let pwned = dir.join("PWNED");
        let script = format!(
            "set -- {w} {g} {p} {w}/f; {PLANT} && echo replaced",
            w = linked.display(),
            g = real.display(),
            p = pwned.display(),
        );
        assert_eq!(sandboxed(&env, &linked, &script), "replaced\n");
        assert!(!linked.join(".git").is_symlink());
        // The app's own poll and reads, on the host.
        let known = project.known_repo();
        let repo = crate::vcs::open(&Environment::Local, &linked, false, known.as_ref()).unwrap();
        // The poll tells the user, whose own git would read the new one.
        assert_eq!(repo.replaced_dot_git(), std::slice::from_ref(&linked));
        let status = repo.status(&linked);
        assert!(status.is_ok(), "{status:?}");
        repo.changes(&linked).unwrap();
        repo.review(&linked, "HEAD", true).unwrap();
        assert!(!pwned.exists());
    }

    /// A sandbox may write the `.git` file of a linked worktree inside its
    /// own worktree. The app's polls take that worktree's git directory
    /// from the repository's list of worktrees instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_rewritten_git_file_of_a_nested_worktree_runs_nothing_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("f"), "a").unwrap();
        run_git(&repo, &["add", "f"]);
        run_git(&repo, &["commit", "-q", "-m", "first"]);
        run_git(&repo, &["worktree", "add", "-q", "inner"]);
        let inner = repo.join("inner");
        let Some(env) = git_sandbox(&dir, &repo, &repo) else {
            return;
        };
        let pwned = dir.join("PWNED");
        // A copy of the metadata with a filter, and the nested worktree's
        // `.git` pointing at its copy of the worktree's own directory.
        let script = format!(
            "set -- {r}/evil {r}/.git {p} {i}/f; {PLANT} && \
             printf 'gitdir: %s\\n' {r}/evil/.git/worktrees/inner > {i}/.git && echo planted",
            r = repo.display(),
            i = inner.display(),
            p = pwned.display(),
        );
        assert_eq!(sandboxed(&env, &repo, &script), "planted\n");
        // The app's own poll, on the host.
        let known = recorded_project(&Environment::Local, &repo).known_repo();
        let found = crate::vcs::open(&Environment::Local, &repo, false, known.as_ref()).unwrap();
        assert_eq!(found.replaced_dot_git(), std::slice::from_ref(&inner));
        let status = found.status(&inner);
        assert!(status.is_ok(), "{status:?}");
        found.changes(&inner).unwrap();
        // git refuses to remove a worktree whose `.git` leads elsewhere.
        assert!(found.remove_worktree(&inner, false).is_err());
        assert!(!pwned.exists());
    }

    /// On another machine the app cannot read a worktree's `.git` file
    /// without blocking the UI, so a separate git directory comes from the
    /// project's record, which discovery read through the project's
    /// environment. Runs through an `ssh` that runs commands here.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_remote_separate_git_directory_keeps_its_configuration() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (repo, sep) = (dir.join("repo"), dir.join("sep.git"));
        let (first, wt) = (dir.join("a/wt"), dir.join("b/wt"));
        std::fs::create_dir_all(&repo).unwrap();
        if !bwrap_and_git() {
            return;
        }
        run_git(&repo, &["init", "-q", "--separate-git-dir", "../sep.git"]);
        run_git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        // The second worktree's administrative directory is `wt1`.
        run_git(&repo, &["worktree", "add", "-q", "-b", "a", "../a/wt"]);
        run_git(&repo, &["worktree", "add", "-q", "-b", "b", "../b/wt"]);
        // Skips the options and the destination, then runs the command the
        // way a remote login shell would.
        let shim = dir.join("fake-ssh");
        let script = "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in \
             -o|-p|-i|-l|-F|-J|-L|-R|-D) shift 2 ;; -*) shift ;; *) break ;; esac; done\n\
             shift\nexec sh -c \"$1\"\n";
        std::fs::write(&shim, script).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A child that another test thread forked while the file was open
        // for writing keeps it busy until that child runs its program.
        for _ in 0..100 {
            let ran = std::process::Command::new(&shim)
                .args(["x", "true"])
                .status();
            if !ran.is_err_and(|error| error.raw_os_error() == Some(26)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ssh = Environment::Ssh(cmdterm_env::SshTarget {
            program: Some(shim.display().to_string()),
            ..cmdterm_env::SshTarget::new("fakehost")
        });
        let project = recorded_project(&ssh, &repo);
        assert_eq!(project.git_dir.as_deref(), Some(sep.as_path()));
        // The app's queries name the git directories over there too.
        let known = project.known_repo();
        let found = crate::vcs::open(&ssh, &repo, false, known.as_ref()).unwrap();
        let admin = sep.join("worktrees/wt1");
        assert_eq!(
            found.git.as_ref().map(|git| git.linked.clone()),
            Some(vec![
                (first, sep.join("worktrees/wt")),
                (wt.clone(), admin.clone())
            ])
        );
        let status = found.status(&wt);
        assert!(status.is_ok(), "{status:?}");

        let Some(env) = project_sandbox(&dir, &project, &repo) else {
            return;
        };
        let Environment::Sandbox(target) = &env else {
            unreachable!();
        };
        assert!(target.policy.writable.contains(&sep));
        // bwrap cannot mount on `.git` when it is a link.
        assert!(!target.policy.writable.contains(&repo.join(".git")));
        for path in ["config", "hooks", "commondir", "worktrees"] {
            assert!(target.policy.read_only.contains(&sep.join(path)), "{path}");
        }
        let script = format!(
            "git config core.fsmonitor x 2>/dev/null || echo config; \
             echo x > {g}/hooks/post-commit 2>/dev/null || echo hooks; \
             echo /tmp > {g}/commondir 2>/dev/null || echo commondir; \
             mv {g} {d}/moved 2>/dev/null || echo pinned; \
             git commit -q --allow-empty -m s && echo committed",
            g = sep.display(),
            d = dir.display(),
        );
        assert_eq!(
            sandboxed(&env, &repo, &script),
            "config\nhooks\ncommondir\npinned\ncommitted\n"
        );
        // A linked worktree's administrative directory is the one the poll
        // listed, writable inside the read-only list of worktrees.
        let git_dir = found
            .git
            .as_ref()
            .and_then(|git| git.git_dir(&repo, &wt, false));
        assert_eq!(git_dir, Some(admin.as_path()));
        let Some(env) = polled_sandbox(&dir, &project, &wt, git_dir) else {
            return;
        };
        let script = format!(
            "echo /tmp > {a}/commondir 2>/dev/null || echo commondir; \
             mkdir {g}/worktrees/planted 2>/dev/null || echo worktrees; \
             git config core.fsmonitor x 2>/dev/null || echo config; \
             git commit -q --allow-empty -m s && echo committed",
            a = admin.display(),
            g = sep.display(),
        );
        assert_eq!(
            sandboxed(&env, &wt, &script),
            "commondir\nworktrees\nconfig\ncommitted\n"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sandbox_commits_but_cannot_change_what_git_runs() {
        let bwrap = std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-all", "true"])
            .output();
        let git = std::process::Command::new("git").arg("--version").output();
        if !bwrap.is_ok_and(|o| o.status.success()) || git.is_err() {
            eprintln!("bwrap or git is missing; skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = cmdterm_env::canonicalize(dir.path()).unwrap();
        let (repo, wt) = repo_with_worktree(&dir);
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": &repo, "vcs": "git",
        }))
        .unwrap();
        let settings = SandboxSettings {
            network: false,
            ..SandboxSettings::default()
        };
        let env = sandbox_env(
            &settings,
            &SandboxRules::default(),
            &project,
            &wt,
            &SandboxDirs {
                shell: &dir.join("shell"),
                files: &dir.join("files"),
                app: &cmdterm_env::AppDirs::under(&dir.join("config")),
                agents: None,
            },
            None,
            None,
            None,
            None,
            None,
        );
        if !matches!(&env, Environment::Sandbox(t) if t.policy.kind == SandboxKind::Bwrap) {
            return;
        }
        let script = format!(
            "export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1; \
             git config core.fsmonitor 'touch {m}' 2>/dev/null || echo config; \
             git config --worktree core.fsmonitor x 2>/dev/null || echo worktree-config; \
             echo x > {g}/hooks/post-index-change 2>/dev/null || echo hooks; \
             mkdir {g}/worktrees/fake 2>/dev/null || echo worktrees; \
             echo /tmp > {g}/worktrees/wt/commondir 2>/dev/null || echo commondir; \
             echo 'gitdir: /tmp' > .git 2>/dev/null || echo dot-git; \
             mv {g} {d}/moved 2>/dev/null || echo pinned; \
             git -c user.name=t -c user.email=t@t commit -q --allow-empty -m s && echo committed",
            m = dir.join("ran").display(),
            g = repo.join(".git").display(),
            d = dir.display(),
        );
        let out = env
            .command("sh")
            .args(["-c", &script])
            .cwd(&wt)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "config\nworktree-config\nhooks\nworktrees\ncommondir\ndot-git\npinned\ncommitted\n",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
        assert!(!config.contains("fsmonitor"));
        // The placeholder git ignores without `extensions.worktreeConfig`.
        assert_eq!(
            std::fs::read_to_string(repo.join(".git/config.worktree")).unwrap(),
            ""
        );
    }

    #[test]
    fn a_sidecar_replaces_the_host_engines() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        std::fs::create_dir_all(path("repo")).unwrap();
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": path("repo"), "vcs": "git",
        }))
        .unwrap();
        let settings = SandboxSettings {
            engines: EngineAccess::Sidecar,
            ..SandboxSettings::default()
        };
        let sidecar = Sidecar {
            name: "cmdterm-sidecar-x".into(),
            engine: "podman".into(),
            socket: path("sidecar/podman.sock"),
            vars: vec![("DOCKER_HOST".into(), "unix:///s/podman.sock".into())],
        };
        let env = sandbox_env(
            &settings,
            &SandboxRules::default(),
            &project,
            &path("repo"),
            &SandboxDirs {
                shell: &path("shell"),
                files: &path("files"),
                app: &cmdterm_env::AppDirs::under(&path("config")),
                agents: None,
            },
            None,
            None,
            None,
            Some(&sidecar),
            None,
        );
        let Environment::Sandbox(target) = env else {
            panic!("not a sandbox");
        };
        let policy = &target.policy;
        assert_eq!(policy.sockets, std::slice::from_ref(&sidecar.socket));
        // The host's engine variables go, the sidecar's come instead.
        assert!(policy.unset.iter().any(|v| v == "DOCKER_HOST"));
        assert!(policy.unset.iter().any(|v| v == "CONTAINER_HOST"));
        assert!(policy.vars.contains(&sidecar.vars[0]));
    }

    #[test]
    fn the_app_dirs_are_hidden_but_the_references_show_through() {
        let dir = tempfile::tempdir().unwrap();
        // Hidden paths are canonical, and the temporary directory has a
        // short name on Windows runners (RUNNER~1).
        let base = cmdterm_env::canonicalize(dir.path()).unwrap();
        let path = |name: &str| base.join(name);
        let app = cmdterm_env::AppDirs {
            config: path("config"),
            data: path("data"),
            state: path("state"),
            cache: path("home/.cache/app"),
            logs: path("state/logs"),
        };
        std::fs::create_dir_all(app.config.join("extensions/private")).unwrap();
        std::fs::create_dir_all(app.data.join("extensions")).unwrap();
        std::fs::create_dir_all(app.state.join("logs")).unwrap();
        std::fs::create_dir_all(&app.cache).unwrap();
        std::fs::write(app.data.join("CONFIGURATION.md"), "").unwrap();
        std::fs::create_dir_all(path("repo")).unwrap();
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": path("repo"), "vcs": "git",
        }))
        .unwrap();
        let policy = |hidden: Vec<String>| {
            let settings = SandboxSettings {
                hidden,
                writable: vec![path("home/.cache").to_string_lossy().into_owned()],
                ..SandboxSettings::default()
            };
            let env = sandbox_env(
                &settings,
                &SandboxRules::default(),
                &project,
                &path("repo"),
                &SandboxDirs {
                    shell: &path("shell"),
                    files: &path("files"),
                    app: &app,
                    agents: None,
                },
                None,
                None,
                None,
                None,
                None,
            );
            let Environment::Sandbox(target) = env else {
                panic!("not a sandbox");
            };
            target.policy
        };
        let real = |path: PathBuf| cmdterm_env::canonicalize(&path).unwrap();
        // Hidden although the settings do not name them.
        let open = policy(Vec::new());
        for dir in [&app.config, &app.data, &app.state, &app.cache] {
            assert!(open.hidden_dirs.contains(dir), "{}", dir.display());
        }
        assert!(
            !open.hidden_dirs.contains(&app.logs),
            "the state directory covers it"
        );
        assert!(open.readable.contains(&real(app.config.join("extensions"))));
        assert!(open.readable.contains(&real(app.data.join("extensions"))));
        assert!(
            open.readable
                .contains(&real(app.data.join("CONFIGURATION.md")))
        );
        assert!(
            open.read_only.contains(&real(app.cache.clone())),
            "a writable ~/.cache does not make the blocklists writable"
        );
        // A path the user hid inside them stays hidden.
        let private = app.config.join("extensions/private");
        let closed = policy(vec![private.to_string_lossy().into_owned()]);
        assert!(closed.hidden_dirs.contains(&private));
        assert!(
            !closed
                .readable
                .contains(&real(app.config.join("extensions")))
        );
        assert!(
            closed
                .readable
                .contains(&real(app.data.join("CONFIGURATION.md")))
        );
    }

    #[test]
    fn local_sandboxes_get_their_own_network_unless_the_host_network_is_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name);
        let app = cmdterm_env::AppDirs::under(&path("config"));
        std::fs::create_dir_all(path("repo")).unwrap();
        let project: Project = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "p", "root": path("repo"), "vcs": "git",
        }))
        .unwrap();
        let hub = path("ports.sock");
        let policy = |settings: &SandboxSettings, project: &Project| {
            let env = sandbox_env(
                settings,
                &SandboxRules::default(),
                project,
                &path("repo"),
                &SandboxDirs {
                    shell: &path("shell"),
                    files: &path("files"),
                    app: &app,
                    agents: None,
                },
                None,
                None,
                Some(&hub),
                None,
                None,
            );
            let Environment::Sandbox(target) = env else {
                panic!("not a sandbox");
            };
            target.policy
        };
        let mut settings = SandboxSettings {
            restrict_network: false,
            ..SandboxSettings::default()
        };
        assert!(!settings.host_network);
        assert!(isolates(&settings));
        let own = policy(&settings, &project);
        if own.kind == SandboxKind::Bwrap {
            assert!(!own.host_network);
            assert_eq!(own.ports.as_deref(), Some(hub.as_path()));
        }
        settings.host_network = true;
        assert!(!isolates(&settings));
        let shared = policy(&settings, &project);
        assert!(shared.host_network);
        // Without the network the namespace stays isolated either way.
        settings.network = false;
        assert!(isolates(&settings));
        // Allowed hosts only isolates too, even with host_network on.
        settings.network = true;
        settings.restrict_network = true;
        assert!(isolates(&settings));
        let restricted = policy(&settings, &project);
        if restricted.kind == SandboxKind::Bwrap {
            assert!(!restricted.host_network);
        }
        settings.restrict_network = false;

        // A sandbox on a remote host keeps its host's network and gets no hub.
        let remote: Project = serde_json::from_value(serde_json::json!({
            "id": 2, "name": "r", "root": "/srv/r", "vcs": "git",
            "env": { "kind": "ssh", "destination": "dev-box" },
        }))
        .unwrap();
        assert!(!remote.env.is_local());
        settings.network = true;
        settings.host_network = false;
        let far = policy(&settings, &remote);
        assert!(far.host_network && far.ports.is_none());
    }

    #[test]
    fn guards_take_the_users_mode_and_allowed_ones_drop_out() {
        let guard = |name: &str| GuardRule {
            id: format!("ext.{name}"),
            title: name.into(),
            hosts: vec!["*".into()],
            requests: vec!["POST /*".into()],
            mode: GuardMode::Ask,
            kind: None,
        };
        let rules = SandboxRules {
            guards: vec![guard("push"), guard("publish"), guard("upload")],
            ..SandboxRules::default()
        };
        let mut settings = SandboxSettings::default();
        // The proxy's own guards ask until the user picks otherwise.
        let builtin = cmdterm_env::builtin_guards();
        assert_eq!(
            proxy_rules(&settings, &rules).guards.len(),
            builtin.len() + 3
        );
        for guard in &builtin {
            settings.set_guard_mode(&guard.id, None, GuardMode::Allow);
        }
        settings.set_guard_mode("ext.push", None, GuardMode::Block);
        settings.set_guard_mode("ext.upload", None, GuardMode::Allow);
        let proxy = proxy_rules(&settings, &rules);
        let modes: Vec<(&str, GuardMode)> = proxy
            .guards
            .iter()
            .map(|g| (g.id.as_str(), g.mode))
            .collect();
        assert_eq!(
            modes,
            [
                ("ext.push", GuardMode::Block),
                ("ext.publish", GuardMode::Ask)
            ]
        );
        settings.set_guard_mode("ext.push", None, GuardMode::Allow);
        settings.set_guard_mode("ext.publish", None, GuardMode::Allow);
        assert!(proxy_rules(&settings, &rules).is_empty());
    }

    #[test]
    fn forge_guards_follow_their_kind_unless_set() {
        let guard = |name: &str, kind| GuardRule {
            id: format!("forge.{name}"),
            title: name.into(),
            hosts: vec!["forge.example".into()],
            requests: vec!["POST /*".into()],
            mode: GuardMode::Ask,
            kind: Some(kind),
        };
        let rules = SandboxRules {
            guards: vec![
                guard("create", GuardKind::Create),
                guard("comment", GuardKind::Comment),
                guard("merge", GuardKind::Merge),
            ],
            ..SandboxRules::default()
        };
        let mut settings = SandboxSettings::default();
        settings.change_requests.comment = GuardMode::Allow;
        settings.change_requests.merge = GuardMode::Block;
        settings.set_guard_mode("forge.merge", Some(GuardKind::Merge), GuardMode::Ask);
        let proxy = proxy_rules(&settings, &rules);
        let modes: Vec<(&str, GuardMode)> = (proxy.guards.iter())
            .filter(|g| g.id.starts_with("forge."))
            .map(|g| (g.id.as_str(), g.mode))
            .collect();
        assert_eq!(
            modes,
            [
                ("forge.create", GuardMode::Ask),
                ("forge.merge", GuardMode::Ask)
            ]
        );
    }
}
