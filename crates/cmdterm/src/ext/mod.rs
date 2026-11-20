//! Lua extension host for cmdterm.
//!
//! Every extension runs in its own Oximond VM with the full standard library
//! (minus LuaJIT `ffi`). Extensions talk to the app through a global `cmdterm`
//! table: they register event handlers and palette commands, and queue host
//! [`Request`]s that the app drains once per frame. They also register
//! recurring tasks, which the app runs on their schedule.
//!
//! Extensions can keep data in a store the app persists (`store.rs`), show
//! Markdown views in panels ([`ViewInfo`]) and offer tools agents call through the control
//! socket and its MCP server ([`ToolInfo`]).
//!
//! An extension may use only the permissions its `manifest.toml` declares
//! and the user granted (see `manifest.rs`).
//!
//! Nothing an extension does can take the host down. Every entry point
//! returns errors as values, and an extension that raises an error, exceeds
//! its time budget, or trips a panic inside the VM is disabled and its VM
//! dropped. Its error stays visible through [`ExtensionHost::extensions`].

mod api;
mod host;
mod json;
mod manifest;
mod output;
mod store;
mod watchdog;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use api::GLOBAL;
pub use cmdterm_env::{GuardMode, GuardRule, SecretRule};
pub use host::{ExtensionHost, SettingValues};
pub use manifest::{
    Logins, Manifest, Permission, Permissions, SettingKind, SettingSpec, SettingValue, https_host,
};
pub use oximond::{CompatibilityProfile, ExecutionMode};
pub use store::flush as flush_stores;

/// The version string Lua sees as `term.version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How [`ExtensionHost`] builds each extension's VM.
#[derive(Clone, Debug)]
pub struct Config {
    /// Lua language and library generation. Defaults to Lua 5.4.
    pub profile: CompatibilityProfile,
    /// Interpreter or tracing JIT. Defaults to the interpreter.
    pub execution_mode: ExecutionMode,
    /// Wall-clock limit for one call into an extension (loading its chunk,
    /// one event handler, one command). `None` disables the limit.
    pub time_budget: Option<Duration>,
    /// Where `term.cache_dir()` puts each extension's directory, as
    /// `<cache_dir>/<name>`. `None` makes `term.cache_dir()` raise an error.
    pub cache_dir: Option<PathBuf>,
    /// Where `term.store` keeps each extension's data, as
    /// `<data_dir>/<name>`. `None` keeps it in memory for this host only.
    pub data_dir: Option<PathBuf>,
    /// Where `term.project_store` keeps each extension's data of a project,
    /// as `<project_data_dir>/<project id>/<name>`. `None` keeps it in
    /// memory for this host only.
    pub project_data_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            profile: CompatibilityProfile::Lua54,
            execution_mode: ExecutionMode::Interpreter,
            time_budget: Some(Duration::from_millis(500)),
            cache_dir: None,
            data_dir: None,
            project_data_dir: None,
        }
    }
}

/// An app event that extensions can subscribe to with `term.on`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    AppStarted,
    ProjectAdded {
        name: String,
        root: PathBuf,
        env: String,
        /// The project's storage id, see [`Context::project_id`].
        project_id: String,
    },
    WorktreeActivated {
        project: String,
        project_id: String,
        path: PathBuf,
        branch: Option<String>,
        env: String,
    },
    TerminalOpened {
        id: u64,
        cwd: Option<PathBuf>,
    },
    TerminalExited {
        id: u64,
        code: Option<i32>,
    },
    /// A worktree's terminal environment is up: a devcontainer started, a
    /// VM or remote project was first used this run, or a sandbox was turned
    /// on. Extensions copy files in or install tools here.
    EnvironmentReady {
        project: String,
        worktree: PathBuf,
        /// `devcontainer`, `sandbox`, `vm`, `remote` or `ssh`.
        kind: String,
        env: String,
        /// The label of the project's environment, the machine under a
        /// devcontainer or sandbox: `local` for this machine.
        host: String,
        /// True when the environment was just created, so it starts empty.
        fresh: bool,
    },
}

/// Number of [`Event`] kinds.
pub(crate) const EVENT_KINDS: usize = 6;

/// Lua names of the event kinds, indexed by [`Event::kind`].
pub(crate) const EVENT_NAMES: [&str; EVENT_KINDS] = [
    "app_started",
    "project_added",
    "worktree_activated",
    "terminal_opened",
    "terminal_exited",
    "environment_ready",
];

impl Event {
    /// The name Lua code passes to `term.on` for this event.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        EVENT_NAMES[self.kind()]
    }

    pub(crate) const fn kind(&self) -> usize {
        match self {
            Self::AppStarted => 0,
            Self::ProjectAdded { .. } => 1,
            Self::WorktreeActivated { .. } => 2,
            Self::TerminalOpened { .. } => 3,
            Self::TerminalExited { .. } => 4,
            Self::EnvironmentReady { .. } => 5,
        }
    }
}

/// The app state Lua reads through `term.context()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub project: Option<String>,
    /// Names the project for `term.project_store`: stable across restarts
    /// and renames, made from the project's environment and root. Letters,
    /// digits, `-` and `_`.
    pub project_id: Option<String>,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
    pub env: String,
    /// Where the worktree's terminals run: `host`, `devcontainer` or
    /// `sandbox`.
    pub runtime: String,
}

/// A function an extension passed as `done`, to call once a background
/// request finished. Ids are unique across hosts, so a reloaded host
/// ignores the callbacks of the one it replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Callback(pub u64);

/// How a background request ended, as its `done` function sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobResult {
    /// True when the command exited with 0, or the copy or trust worked.
    pub ok: bool,
    /// The exit code, `None` when the command did not run or was killed.
    pub code: Option<i32>,
    /// Up to [`JOB_OUTPUT_LIMIT`] bytes of the command's output.
    pub stdout: String,
    pub stderr: String,
    /// Why it did not run or failed, for requests that are not one command.
    pub error: Option<String>,
}

/// How much of a command's stdout and stderr a `done` function gets.
pub const JOB_OUTPUT_LIMIT: usize = 64 * 1024;

impl JobResult {
    /// A failure without a command behind it.
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// The result of a command that ran, with its output cut to
    /// [`JOB_OUTPUT_LIMIT`].
    pub fn from_output(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Self {
        let text = |bytes: &[u8]| {
            let end = bytes.len().min(JOB_OUTPUT_LIMIT);
            String::from_utf8_lossy(&bytes[..end]).into_owned()
        };
        Self {
            ok: code == Some(0),
            code,
            stdout: text(stdout),
            stderr: text(stderr),
            error: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotifyLevel {
    #[default]
    Info,
    Warning,
    Error,
}

/// A host action an extension asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    OpenTerminal {
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        title: Option<String>,
    },
    OpenBrowser {
        url: String,
    },
    Notify {
        level: NotifyLevel,
        message: String,
    },
    SendText {
        terminal: Option<u64>,
        text: String,
    },
    SwitchWorktree {
        path: PathBuf,
    },
    /// Copy a local file or directory into a worktree's terminal
    /// environment, in the background.
    Copy {
        from: PathBuf,
        /// Destination directory; may start with `~`.
        to: String,
        /// `None` is the active worktree.
        worktree: Option<PathBuf>,
        exclude: Vec<String>,
        /// Called with the outcome. Without one, failures become notices.
        done: Option<Callback>,
    },
    /// Run a command in a worktree's terminal environment, in the
    /// background. Failures become notices.
    Run {
        command: Vec<String>,
        worktree: Option<PathBuf>,
        cwd: Option<String>,
        /// Run in the project's environment instead: the machine under the
        /// worktree's devcontainer or sandbox.
        on_host: bool,
        /// Called with the outcome. Without one, failures become notices.
        done: Option<Callback>,
    },
    /// Add a PEM certificate of this machine to the trust store of a
    /// worktree's terminal environment, in the background.
    Trust {
        /// Names the certificate in the store: letters, digits, `-`, `_`
        /// and `.`.
        name: String,
        path: PathBuf,
        worktree: Option<PathBuf>,
        done: Option<Callback>,
    },
    /// Show a view in a panel of the active worktree: the panel that shows
    /// it already, else a new tab.
    OpenView {
        /// `<extension>.<name>`.
        view: String,
        key: String,
    },
    /// Render the open panels of a view again, all of them or the one of
    /// `key`.
    RefreshView {
        view: String,
        key: Option<String>,
    },
    /// An HTTP request, made in the background.
    Http(HttpRequest),
    /// A change request's discussion, from `term.forge_discussion`.
    ForgeDiscussion {
        forge: String,
        id: String,
        /// `Err` with the forge's message when it could not be read.
        discussion: Result<Discussion, String>,
    },
    /// What a forge's `check` found, from `term.forge_report`.
    ForgeReport {
        /// `<extension>.<name>`.
        forge: String,
        results: Vec<BranchReport>,
        /// Seconds to wait before the next check, such as after a rate
        /// limit answer.
        retry_after: Option<u64>,
    },
}

/// A Markdown view an extension registered with `term.view`. Its `render`
/// function turns a key into a title and Markdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewInfo {
    /// `<extension>.<name>`.
    pub id: String,
    pub extension: String,
}

/// What a view's `render` function returned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rendered {
    pub title: String,
    pub markdown: String,
}

/// A tool an extension offers agents with `term.tool`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInfo {
    /// Letters, digits, `_` and `-`, unique across extensions.
    pub name: String,
    pub description: String,
    /// The JSON schema of its arguments, an object schema.
    pub input_schema: serde_json::Value,
    pub extension: String,
}

/// Who calls a tool, as the control socket knows the agent's terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolCaller {
    pub project: Option<String>,
    /// See [`Context::project_id`].
    pub project_id: Option<String>,
    pub worktree: Option<PathBuf>,
    /// `local`, `sandbox`, `container`, `vm`, `remote` or `ssh`.
    pub environment: String,
    /// The agent's name on the agent list, if it set one.
    pub agent: Option<String>,
}

/// How a tool call ended: the result, or the error to show the agent.
pub type ToolResult = Result<serde_json::Value, String>;

/// A coding agent an extension registered with `term.agent`. The app treats
/// it like the agents it knows itself: the Agents page installs its
/// integration, the palette starts it, saved sessions resume, and sandboxes
/// let it keep its state. Paths are relative to `$HOME`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AgentInfo {
    /// What its hooks pass to `agent-state context` and what
    /// `cmdterm_session` names: a lowercase letter, then lowercase letters,
    /// digits and `-`. Unique across extensions.
    pub name: String,
    pub extension: String,
    pub title: String,
    /// The program that starts it, typed into shells.
    pub program: String,
    /// Its configuration directory; the agent counts as installed where it
    /// exists.
    pub config_dir: String,
    /// Where the app puts its skills, if the agent reads skills.
    pub skills_dir: Option<String>,
    pub integration: AgentIntegration,
    /// Where the debug browser's MCP server goes, if the agent speaks MCP.
    pub mcp: Option<AgentMcp>,
    /// Arguments after the program that resume a session, in front of its
    /// id. `None` when the app should not resume it.
    pub resume: Option<Vec<String>>,
    /// Whether its session start hook runs as soon as it starts, not only
    /// with the first prompt.
    pub session_at_start: bool,
    /// What it can do, as it opted in with `supports`. The app relies on
    /// nothing else.
    pub supports: Vec<crate::agents::Capability>,
    /// Arguments after the program that start its TUI with a first prompt,
    /// with `{prompt}` where it goes. Only with `prompt` in `supports`.
    pub prompt: Option<Vec<String>>,
    /// Where it keeps its sessions, so the app can tell whether a saved one
    /// still exists before it resumes it. `None` resumes without checking.
    pub sessions: Option<AgentSessions>,
    pub sandbox: AgentSandbox,
    /// The npm packages it is published as, for update checks.
    pub npm: Vec<String>,
}

/// How the app installs an extension agent's integration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum AgentIntegration {
    /// Nothing beyond the skills and the MCP server: the agent reports its
    /// state itself.
    #[default]
    Native,
    /// Hook entries in a Claude Code style JSON file the user keeps too.
    Hooks {
        file: String,
        hooks: Vec<AgentHook>,
        /// Hook timeouts per second: 1 for seconds, 1000 for milliseconds.
        timeout_unit: u64,
    },
    /// A file the app owns whole, such as a plugin.
    File { file: String, content: String },
}

/// One hook: the agent's event, its matcher and the `agent-state`
/// arguments it runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentHook {
    pub event: String,
    pub matcher: Option<String>,
    pub args: String,
}

/// The directory an agent keeps its sessions in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentSessions {
    pub dir: String,
    /// Whether a session's id is in the name of a file or directory there
    /// (`true`), or only in a file's contents.
    pub by_name: bool,
}

/// Where and how an agent lists MCP servers, in a JSON file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentMcp {
    pub file: String,
    /// The object that lists servers, such as `mcpServers`.
    pub key: String,
    pub style: McpStyle,
}

/// The shape of one server entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum McpStyle {
    /// `{ type = "stdio", command, args, env }`, as Claude Code reads it.
    Stdio,
    /// `{ command, args }`, as Gemini CLI reads it.
    Command,
    /// `{ type = "local", command = [..], enabled }`, as opencode reads it.
    Local,
}

/// What an extension agent may do in sandboxes, see [`AgentInfo`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AgentSandbox {
    /// Where it keeps its login, sessions and state.
    pub writable: Vec<String>,
    /// Entries inside `writable` that stay read-only: settings, hooks and
    /// anything else it runs.
    pub read_only: Vec<String>,
    /// Settings files it changes itself, copied into bubblewrap sandboxes
    /// and brought back after asking.
    pub synced: Vec<String>,
    /// `(variable, directory)` for agents that rename new settings over
    /// old ones.
    pub home_var: Option<(String, String)>,
    /// The variables it logs in with, such as `MY_AGENT_API_KEY`, which
    /// sandboxes keep although they look like credentials.
    pub login_vars: Vec<String>,
    /// The keyring secrets it logs in with, each by attributes it has,
    /// sorted by key. Sandboxes on Linux read and renew them without asking.
    pub keyring: Vec<Vec<(String, String)>>,
    /// Per synced file, the key paths whose changes need the user's answer.
    /// A file without an entry asks for every change.
    pub guarded: Vec<(String, Vec<Vec<String>>)>,
}

/// A forge an extension registered with `term.forge`: where the user signs
/// in, how the app sends the token, and the `check` function that looks up
/// the change requests of branches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgeInfo {
    /// `<extension>.<name>`.
    pub id: String,
    pub extension: String,
    pub title: String,
    /// Accounts the user signs in to: a host such as `github.com`, or a host
    /// and a path such as `github.com/acme` for the repositories under that
    /// owner, group or organization. Each has its own token. The user's
    /// list from the Accounts page, else [`Self::default_hosts`].
    pub hosts: Vec<String>,
    /// From `accounts.default`: the accounts before the user changes them.
    pub default_hosts: Vec<String>,
    /// From `accounts.example`: an account the add field shows, such as
    /// `github.com/acme`.
    pub example: Option<String>,
    /// From `accounts.hint`: what an account is, under the add field.
    pub hint: Option<String>,
    /// `(remote pattern, account location)`: remotes on other hosts that
    /// belong to an account, such as `ssh.dev.azure.com` to `dev.azure.com`.
    /// A pattern may end in a path that is cut off, such as
    /// `ssh.dev.azure.com/v3`, and a `*` in the location takes the label a
    /// `*.domain` pattern matched.
    pub aliases: Vec<(String, String)>,
    /// The header the token goes into, such as `Authorization`.
    pub auth_header: String,
    /// Its value, with `{token}` for the token, `{basic}` for the Base64 of
    /// `:token` and `{base64}` for the Base64 of the token as typed.
    pub auth_value: String,
    /// Variables the app takes a token from when the user did not sign in.
    pub env: Vec<String>,
    /// What the token needs, shown under the token field.
    pub scopes: Option<String>,
    /// Whether it has a `discussion` function.
    pub discussions: bool,
    /// Guards on its change requests, with the hosts `term.forge` named.
    /// [`Self::guard_rules`] adds the accounts' hosts.
    pub guards: Vec<GuardRule>,
}

impl ForgeInfo {
    /// The account a remote on `host` with repository `path` belongs to:
    /// of the accounts on its host, the one with the longest path that
    /// `path` starts with.
    #[must_use]
    pub fn account_for(&self, host: &str, path: &str) -> Option<&str> {
        let path = path.trim_matches('/');
        if self
            .hosts
            .iter()
            .any(|a| account_host(a).eq_ignore_ascii_case(host))
        {
            return self.longest_account(host, path);
        }
        // The alias with the longest pattern wins, so `x/v3` beats `x`.
        let (pattern, location) = self
            .aliases
            .iter()
            .filter(|(pattern, _)| {
                let (hosts, prefix) = pattern.split_once('/').unwrap_or((pattern, ""));
                host_matches(hosts, host) && strip_segments(path, prefix).is_some()
            })
            .max_by_key(|(pattern, _)| pattern.len())?;
        let (hosts, prefix) = pattern.split_once('/').unwrap_or((pattern, ""));
        let rest = strip_segments(path, prefix)?;
        let location = match hosts.strip_prefix("*.") {
            Some(domain) if location.contains('*') => {
                let label = &host[..host.len() - domain.len() - 1];
                location.replace('*', label)
            }
            _ => location.clone(),
        };
        let (target, prefix) = location.split_once('/').unwrap_or((&location, ""));
        let path = match (prefix.is_empty(), rest.is_empty()) {
            (true, _) => rest.to_owned(),
            (false, true) => prefix.to_owned(),
            (false, false) => format!("{prefix}/{rest}"),
        };
        self.longest_account(target, &path)
    }

    /// The forge's guards, each on the hosts of the forge's accounts and
    /// their subdomains, such as `api.github.com` for `github.com`, and on
    /// the hosts of its aliases, besides the hosts `term.forge` named.
    #[must_use]
    pub fn guard_rules(&self) -> Vec<GuardRule> {
        let mut hosts: Vec<String> = Vec::new();
        let mut add = |host: String| {
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        };
        for account in &self.hosts {
            let host = account_host(account).to_ascii_lowercase();
            add(format!("*.{host}"));
            add(host);
        }
        for (pattern, _) in &self.aliases {
            let host = pattern.split_once('/').map_or(&**pattern, |(host, _)| host);
            add(host.to_ascii_lowercase());
        }
        (self.guards.iter())
            .map(|guard| {
                let mut guard = guard.clone();
                for host in &hosts {
                    if !guard.hosts.contains(host) {
                        guard.hosts.push(host.clone());
                    }
                }
                guard
            })
            .collect()
    }

    fn longest_account(&self, host: &str, path: &str) -> Option<&str> {
        self.hosts
            .iter()
            .filter(|account| {
                let (own, prefix) = account.split_once('/').unwrap_or((account, ""));
                own.eq_ignore_ascii_case(host) && strip_segments(path, prefix).is_some()
            })
            .max_by_key(|account| account.len())
            .map(String::as_str)
    }
}

/// `entry` as a forge account: trimmed, lowercase, without a trailing
/// slash. `None` when it is not a host name or a host and a path.
#[must_use]
pub fn forge_account(entry: &str) -> Option<String> {
    let account = entry.trim().trim_end_matches('/').to_ascii_lowercase();
    let (host, path) = account.split_once('/').unwrap_or((&account, ""));
    let valid = !host.is_empty()
        && !host.starts_with("*.")
        && cmdterm_env::valid_host_pattern(host)
        && valid_forge_path(path);
    valid.then_some(account)
}

/// Whether `path`, the part of a forge account after its host, is empty or
/// slash-separated segments of URL path characters.
#[must_use]
pub fn valid_forge_path(path: &str) -> bool {
    path.is_empty()
        || path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        })
}

/// The host of an account, without its path: `github.com` for
/// `github.com/acme`.
#[must_use]
pub fn account_host(account: &str) -> &str {
    account.split_once('/').map_or(account, |(host, _)| host)
}

/// The lowercase host of an http(s) URL, without user or port.
#[must_use]
pub fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` is an account's host or one of its subdomains, such as
/// `api.github.com` for `github.com`.
#[must_use]
pub fn under_account(account: &str, host: &str) -> bool {
    let own = account_host(account);
    host.eq_ignore_ascii_case(own)
        || host.len() > own.len() + 1
            && host.as_bytes()[host.len() - own.len() - 1] == b'.'
            && host[host.len() - own.len()..].eq_ignore_ascii_case(own)
}

/// `path` without the whole segments of `prefix`, ignoring case; `None` when
/// it does not start with them.
fn strip_segments<'a>(path: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(path);
    }
    let head = path.get(..prefix.len())?;
    if !head.eq_ignore_ascii_case(prefix) {
        return None;
    }
    match &path[prefix.len()..] {
        "" => Some(""),
        rest => rest.strip_prefix('/'),
    }
}

/// Whether `host` is `pattern`, or a subdomain of it for `*.domain`.
#[must_use]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(domain) => host.len().checked_sub(domain.len() + 1).is_some_and(|cut| {
            host.as_bytes()[cut] == b'.' && host[cut + 1..].eq_ignore_ascii_case(domain)
        }),
        None => pattern.eq_ignore_ascii_case(host),
    }
}

/// One repository a forge's `check` function looks at, with the branches
/// the app wants the change requests of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgeCheck {
    /// The account the remote belongs to, `host` or `host/path`, which
    /// `term.http`'s `account` names.
    pub account: String,
    /// The remote's own host, such as `ssh.dev.azure.com`.
    pub host: String,
    /// The repository path in the remote URL, without `.git`.
    pub path: String,
    pub remote: String,
    pub branches: Vec<String>,
    /// Whether the app has a token for `account`.
    pub signed_in: bool,
}

/// The state of a branch's change request, as `term.forge_report` gives it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BranchState {
    /// The branch has no change request.
    #[default]
    None,
    Open,
    Merged,
    Closed,
}

/// What a forge found for one branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BranchReport {
    pub account: String,
    pub path: String,
    pub branch: String,
    pub state: BranchState,
    pub number: Option<u64>,
    pub url: Option<String>,
    pub title: Option<String>,
}

/// The change request whose discussion the app asks a forge's
/// `discussion` function for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscussionRequest {
    /// Names the answer; `term.forge_discussion` passes it back.
    pub id: String,
    pub account: String,
    pub host: String,
    pub path: String,
    pub remote: String,
    pub branch: String,
    pub number: u64,
    pub signed_in: bool,
}

/// A change request's discussion, as `term.forge_discussion` gives it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Discussion {
    pub title: String,
    pub url: Option<String>,
    pub state: BranchState,
    pub author: Option<String>,
    pub body: Option<String>,
    /// The change request is a draft.
    pub draft: bool,
    /// The branch it merges into.
    pub target: Option<String>,
    /// The forge says it does not merge cleanly.
    pub conflicts: bool,
    /// The latest verdict of each reviewer, and reviewers asked for one.
    pub reviews: Vec<Review>,
    /// CI pipelines, jobs and status checks on the head.
    pub checks: Vec<Check>,
    /// Heads of the source branch the forge knows of, oldest first.
    pub pushes: Vec<Push>,
    /// Comments on the change request as a whole, oldest first.
    pub comments: Vec<Comment>,
    pub threads: Vec<Thread>,
}

/// What a reviewer said last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    /// Asked for a review and gave none yet.
    #[default]
    Pending,
    Commented,
    Approved,
    Changes,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Review {
    pub author: String,
    pub verdict: Verdict,
}

/// Where a CI check stands.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum CheckState {
    /// Ordered so the worst state of a list is its maximum.
    Success,
    Skipped,
    Canceled,
    #[default]
    Pending,
    Running,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    pub url: Option<String>,
}

/// A head of the source branch, as the forge saw it pushed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Push {
    pub commit: String,
    /// ISO 8601, as the forge gives it.
    pub at: Option<String>,
    /// The commit it replaced, when the forge reports a force push.
    pub before: Option<String>,
    /// Whether the forge calls it a force push.
    pub force: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Comment {
    pub author: String,
    pub body: String,
    /// ISO 8601, as the forge gives it.
    pub at: Option<String>,
}

/// Comments on one place in the code.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Thread {
    pub path: String,
    /// The line the comment was written on, in `commit`'s version of the
    /// file, or in the base's for `old_side`.
    pub line: Option<u64>,
    /// On a removed line, in the base's version of the file.
    pub old_side: bool,
    /// The commit the line belongs to.
    pub commit: Option<String>,
    /// The diff hunk the forge shows above the comment; its last line is
    /// the commented one.
    pub hunk: Option<String>,
    pub resolved: bool,
    /// Whether the forge itself calls it outdated.
    pub outdated: bool,
    pub comments: Vec<Comment>,
}

/// An HTTP request an extension made with `term.http`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// `(forge id, account host)` whose token the app adds.
    pub account: Option<(String, String)>,
    pub done: Option<Callback>,
}

/// How a `term.http` request ended, as its `done` function sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HttpResponse {
    /// 0 when no response came.
    pub status: u16,
    /// Lowercase names.
    pub headers: Vec<(String, String)>,
    /// Up to [`HTTP_BODY_LIMIT`] bytes.
    pub body: String,
    pub error: Option<String>,
}

/// How much of a response body a `done` function gets.
pub const HTTP_BODY_LIMIT: u64 = 4 * 1024 * 1024;

/// A palette command registered with `term.command`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: String,
    pub title: String,
    /// Name of the extension that registered it.
    pub extension: String,
    /// Key sequences the extension asks for, from the command's `keys` field.
    /// The app parses them and drops the ones it cannot read.
    pub keys: Vec<KeyRequest>,
}

/// One key sequence for a command, such as `"ctrl+alt+g"` or `"ctrl+b g"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRequest {
    /// The keymap preset this sequence is for. `None` applies to every preset
    /// that has no sequence of its own.
    pub keymap: Option<String>,
    pub keys: String,
}

/// A recurring task registered with `term.task`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInfo {
    /// `<extension>.<name>`: what the app records the last run under.
    pub id: String,
    pub title: String,
    /// Name of the extension that registered it.
    pub extension: String,
    /// Seconds between two runs, at least [`MIN_TASK_INTERVAL`].
    pub every: u64,
    /// Project names or root paths the task runs for, with `*` matching any
    /// run of characters. `None` runs the task once per period, for no
    /// project.
    pub projects: Option<Vec<String>>,
}

/// The shortest interval `term.task` accepts, in seconds.
pub const MIN_TASK_INTERVAL: u64 = 60;

impl TaskInfo {
    /// Whether the task runs for the project `name` with main worktree
    /// `root`. A pattern with a `/`, or one starting with `~`, is compared
    /// with the root, `~` standing for this machine's home directory; any
    /// other pattern with the name.
    #[must_use]
    pub fn matches(&self, name: &str, root: &Path) -> bool {
        let Some(patterns) = &self.projects else {
            return false;
        };
        let root = root.to_string_lossy();
        patterns.iter().any(|pattern| {
            if let Some(rest) = pattern.strip_prefix('~') {
                let home = std::env::var("HOME").unwrap_or_default();
                wildcard(&format!("{}{rest}", home.trim_end_matches('/')), &root)
            } else if pattern.contains('/') {
                wildcard(pattern, &root)
            } else {
                wildcard(pattern, name)
            }
        })
    }
}

/// Whether `text` matches `pattern`, where `*` matches any run of
/// characters, `/` included.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0, 0);
    // Position after the last `*` and the text position it matched up to.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p + 1, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((after, matched)) = star {
            p = after;
            t = matched + 1;
            star = Some((after, matched + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&b| b == b'*')
}

/// The project a task runs for, as its `run` function sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskProject {
    pub name: String,
    /// See [`Context::project_id`].
    pub id: String,
    pub root: PathBuf,
    pub env: String,
    pub worktrees: Vec<TaskWorktree>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskWorktree {
    pub path: PathBuf,
    /// `None` when detached.
    pub branch: Option<String>,
    /// `host`, `devcontainer` or `sandbox`.
    pub runtime: String,
}

/// What sandboxed terminals may reach besides the user's sandbox settings,
/// from `term.sandbox` and `term.secret`. Paths are this machine's and
/// may start with `~/`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxRules {
    /// Hosts sandboxes with a restricted network may reach, such as
    /// `crates.io` or `*.github.com`.
    pub hosts: Vec<String>,
    pub writable: Vec<String>,
    pub hidden: Vec<String>,
    /// `(host, sandbox)`: a host directory that stands in for a sandbox path.
    pub redirects: Vec<(String, String)>,
    /// Variables set inside, such as a token variable that holds a fake.
    pub vars: Vec<(String, String)>,
    /// `(sandbox, content)`: content that stands in for a sandbox file, such
    /// as a config file with fake credentials.
    pub files: Vec<(String, String)>,
    /// Real credentials the proxy sends in place of their fakes.
    pub secrets: Vec<SecretRule>,
    /// Requests that publish. Their mode is always [`GuardMode::Ask`] here;
    /// the app applies the user's choice.
    pub guards: Vec<GuardRule>,
    /// Hosts the proxy refuses, even with the whole network. A name blocks
    /// its subdomains too; `*.example.com` only the subdomains.
    pub blocked: Vec<String>,
    /// Hosts no block applies to, in the same form as [`Self::blocked`].
    pub unblocked: Vec<String>,
    /// Lists of hosts to refuse that the app downloads.
    pub blocklists: Vec<BlocklistSource>,
}

/// A list of domains the app downloads, caches and hands to the proxy.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlocklistSource {
    /// `<extension>.<name>`: the cache file and what refusals name.
    pub id: String,
    /// `https://` URLs of the same list, tried in order: the first is the
    /// source, the others are mirrors.
    pub urls: Vec<String>,
    /// How old, in seconds, a cached copy may get before it is downloaded
    /// again.
    pub max_age: u64,
}

impl SandboxRules {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.writable.is_empty()
            && self.hidden.is_empty()
            && self.redirects.is_empty()
            && self.vars.is_empty()
            && self.files.is_empty()
            && self.secrets.is_empty()
            && self.guards.is_empty()
            && self.blocked.is_empty()
            && self.unblocked.is_empty()
            && self.blocklists.is_empty()
    }

    /// Adds `other`'s entries that `self` does not have yet.
    pub fn merge(&mut self, other: &Self) {
        fn add<T: Clone + PartialEq>(into: &mut Vec<T>, from: &[T]) {
            for item in from {
                if !into.contains(item) {
                    into.push(item.clone());
                }
            }
        }
        add(&mut self.hosts, &other.hosts);
        add(&mut self.writable, &other.writable);
        add(&mut self.hidden, &other.hidden);
        add(&mut self.redirects, &other.redirects);
        // The first extension to set a variable or a file keeps it.
        for (key, value) in &other.vars {
            if !self.vars.iter().any(|(k, _)| k == key) {
                self.vars.push((key.clone(), value.clone()));
            }
        }
        for (path, content) in &other.files {
            if !self.files.iter().any(|(p, _)| p == path) {
                self.files.push((path.clone(), content.clone()));
            }
        }
        for secret in &other.secrets {
            match self.secrets.iter_mut().find(|s| s.fake == secret.fake) {
                Some(known) => add(&mut known.hosts, &secret.hosts),
                None => self.secrets.push(secret.clone()),
            }
        }
        for guard in &other.guards {
            if !self.guards.iter().any(|g| g.id == guard.id) {
                self.guards.push(guard.clone());
            }
        }
        add(&mut self.blocked, &other.blocked);
        add(&mut self.unblocked, &other.unblocked);
        for source in &other.blocklists {
            if !self.blocklists.iter().any(|s| s.id == source.id) {
                self.blocklists.push(source.clone());
            }
        }
    }
}

/// One loaded (or failed) extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionInfo {
    pub name: String,
    /// The file the extension was loaded from, `None` for [`ExtensionHost::load_source`].
    pub path: Option<PathBuf>,
    /// False once the extension failed to load or raised an error.
    pub enabled: bool,
    /// The error that disabled it.
    pub last_error: Option<String>,
    /// True when the user turned it off: it is listed but was never loaded.
    pub turned_off: bool,
    /// From its `manifest.toml`: the name the settings show, else `name`.
    pub title: Option<String>,
    /// From its `manifest.toml`.
    pub description: Option<String>,
    /// What its manifest declares.
    pub permissions: Permissions,
    /// Declared permissions the user has not granted. The extension runs
    /// with the granted ones; while none of the declared ones are granted it
    /// is listed but not loaded.
    pub missing: Permissions,
    /// From its manifest's `hosts`: what `term.http` may reach besides its
    /// forges' accounts and the values of its `hosts` settings.
    pub hosts: Vec<String>,
    /// The settings its manifest declares, in order.
    pub settings: Vec<SettingSpec>,
    /// Its manifest's `[logins]` table.
    pub logins: Option<Logins>,
    /// False when its manifest says `enabled = false`: it loads only once
    /// the user turns it on.
    pub default_on: bool,
}

impl ExtensionInfo {
    /// The name the settings show: the manifest's `title`, else the
    /// directory name.
    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.name)
    }
}

/// An error raised by, or about, one extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtError {
    /// The extension's name. Host-level errors (such as an unknown command)
    /// use `"scriptschnell-term"`.
    pub extension: String,
    pub message: String,
}

impl ExtError {
    pub(crate) fn new(extension: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            extension: extension.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ExtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "extension {}: {}", self.extension, self.message)
    }
}

impl std::error::Error for ExtError {}

#[cfg(test)]
mod tests;
