//! Persistent app state: the projects in the sidebar, their worktrees, and the
//! tabs saved per worktree. Each tab holds its own tree of split panes.
//!
//! Besides repositories the rail lists sessions. A local session is a
//! project opened temporarily: it lives until the app quits and never
//! reaches `state.json`. A global session is a machine's home directory
//! without a repository, for work on the machine itself such as its
//! dotfiles or installed tools. It starts temporary too and is kept on
//! request. [`StateStore::save`] leaves temporary projects out.
//!
//! The file is JSON at `state.json` in the state directory
//! ([`cmdterm_env::AppDirs::state`]): `$XDG_STATE_HOME/scriptschnell-term` or
//! `~/.local/state/scriptschnell-term` on Linux, or the one directory of
//! `--config-dir` and `$SCRIPTSCHNELL_TERM_CONFIG_DIR`.
//! Saves write a sibling temp file and rename it, so a crash never leaves a
//! half-written state behind.
//!
//! User preferences (fonts, color scheme, turned-off extensions, keybindings)
//! live in `settings.json` in the configuration directory, so they can be edited or
//! copied by hand without the project list.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::forge::ForgeRepo;
use crate::vcs::VcsKind;
use cmdterm_env::{Environment, GuardKind, GuardMode};
use serde::{Deserialize, Serialize};

mod pane;

pub use pane::{LEAST_SHARE, PaneLayout, SplitAxis};

pub const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<Project>,
    /// The project whose active worktree fills the main area.
    #[serde(default)]
    pub active_project: Option<u64>,
    #[serde(default)]
    pub next_id: u64,
    /// When each extension task last ran, in seconds since the Unix epoch.
    /// The key is the task's id, followed by `@` and the project id for a
    /// task that runs per project.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub task_runs: BTreeMap<String, u64>,
}

/// A repository, local or remote, with any number of worktrees.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub env: Environment,
    /// The main worktree's root, on the project's machine.
    pub root: PathBuf,
    /// `None` for a global session, which has no repository and is never
    /// polled by the VCS worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs: Option<VcsKind>,
    /// A git repository's common git directory, with symlinks resolved, as
    /// the app found it when the project was added (or first polled, for a
    /// project saved before it was recorded). The app's git commands read
    /// the metadata there, never through a worktree's `.git`, which a
    /// sandbox may replace. See [`crate::vcs::KnownRepo`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dir: Option<PathBuf>,
    /// Lives until the app quits: a local session, or a global session not
    /// kept yet. Never saved.
    #[serde(skip)]
    pub temporary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<ForgeRepo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_worktree: Option<PathBuf>,
    /// Saved configuration per worktree path. Worktrees without an entry use
    /// the default layout: one shell in the worktree.
    #[serde(default)]
    pub worktrees: Vec<WorktreeConfig>,
    /// Guarded requests the user allowed for good in this project's
    /// sandboxes, from the approval dialog's "Always allow".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub always_allowed: Vec<AlwaysAllowed>,
    /// Whether agents in this project may create and open worktrees and
    /// start agents there without asking, from the launch dialog's "Always
    /// allow in this project".
    #[serde(default, skip_serializing_if = "is_false")]
    pub allows_launches: bool,
    /// Whether the app reads this project as a Jujutsu repository where it
    /// has one, in place of [`Settings::jj`]. `None` follows the setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jj: Option<bool>,
    /// The branch or revision reviews compare against when none is named.
    /// `None` detects it, see [`crate::vcs::Repository::default_base`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Folders on the project's machine that its sandboxed worktrees may
    /// reach besides the worktree, on top of [`SandboxSettings`].
    #[serde(default, skip_serializing_if = "ProjectSandbox::is_empty")]
    pub sandbox: ProjectSandbox,
    /// Shown because its remote machine shares it (`remote.fused` in
    /// [`Settings`]), not because the user added it. It goes away when the
    /// machine stops listing it or the machine is no longer fused.
    #[serde(default, skip_serializing_if = "is_false")]
    pub fused: bool,
}

/// Folders one project's sandboxes may reach. Paths may start with `~/`,
/// which only a local project expands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectSandbox {
    /// Bound read-write, like the worktree.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<String>,
    /// Bound read-only. They show through the hidden paths of
    /// [`SandboxSettings`], so a project can read one hidden file or folder.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub readable: Vec<String>,
}

impl ProjectSandbox {
    pub fn is_empty(&self) -> bool {
        self.writable.is_empty() && self.readable.is_empty()
    }

    /// The writable or the readable list.
    pub fn list_mut(&mut self, writable: bool) -> &mut Vec<String> {
        if writable {
            &mut self.writable
        } else {
            &mut self.readable
        }
    }
}

/// A guard and a host (or a keyring secret's label) allowed for good.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlwaysAllowed {
    /// The guard's id, such as `cmdterm.git-push` or `cmdterm.keyring`.
    pub guard: String,
    /// The host the request went to, or the secret's label.
    pub host: String,
}

/// What a worktree looks like when it is switched to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorktreeConfig {
    pub path: PathBuf,
    /// Every panel of every tab. Tabs refer to them by index.
    #[serde(default)]
    pub panels: Vec<PanelSpec>,
    /// The tabs in strip order. Empty in a state written before tabs held
    /// their own panes; [`Self::tabs`] then builds them from the old fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<TabConfig>,
    /// Index of the shown tab.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_tab: usize,
    /// Extra environment variables for terminals opened in this worktree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<(String, String)>,
    /// Where this worktree's terminals run, a sandbox unless chosen
    /// otherwise. VCS work always runs in the project's environment.
    #[serde(default, skip_serializing_if = "Runtime::is_sandbox")]
    pub runtime: Runtime,
    /// Titles of the last agent sessions in this worktree's terminals,
    /// oldest first, at most [`MAX_AGENT_TITLES`]. The rail can name the
    /// worktree after the last one, and the rail's search looks at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_titles: Vec<String>,
    /// The agent sessions that ran in this worktree's terminals and named
    /// an id, so the Agents tab lists them after they ended and resumes
    /// them. Least recently seen first, at most [`MAX_AGENT_SESSIONS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_sessions: Vec<AgentRecord>,
    /// The panel in the focused pane, from before tabs. Read, never written.
    #[serde(default, skip_serializing)]
    active_panel: usize,
    /// Panes shared by every tab, from before tabs. Read, never written.
    #[serde(default, skip_serializing)]
    layout: Option<PaneLayout>,
    /// Focused pane of `layout`. Read, never written.
    #[serde(default, skip_serializing)]
    focused_pane: usize,
    /// The two-pane split saved before `layout` existed. Read, never written.
    #[serde(default, skip_serializing)]
    split: Option<LegacySplit>,
}

/// Agent session titles a worktree keeps.
pub const MAX_AGENT_TITLES: usize = 8;

/// Agent sessions a worktree keeps.
pub const MAX_AGENT_SESSIONS: usize = 20;

/// An agent session a worktree saw, as [`AgentSession`] names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// The agent's program: `claude`, `codex` and so on.
    pub agent: String,
    /// The agent's id for the session.
    pub session: String,
    /// The title the session gave its terminal, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// When the app first saw the session or its title last changed, in
    /// seconds since the Unix epoch.
    #[serde(default)]
    pub seen: u64,
}

impl AgentRecord {
    /// The session to open in a terminal to resume this one.
    pub fn resume(&self) -> AgentSession {
        AgentSession {
            agent: self.agent.clone(),
            session: Some(self.session.clone()),
            working: false,
        }
    }

    /// Whether `agent` is this session.
    pub fn is(&self, agent: &AgentSession) -> bool {
        self.agent == agent.agent && agent.session.as_deref() == Some(self.session.as_str())
    }
}

/// Where a worktree's terminals run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// In the project's environment, like VCS work.
    Host,
    /// In the worktree's devcontainer, started from `devcontainer.json`.
    Devcontainer,
    /// In a sandbox that may write only the worktree, set up by
    /// [`SandboxSettings`]: bubblewrap on Linux, Seatbelt on macOS, a
    /// restricted token on Windows. The default.
    #[default]
    Sandbox,
}

impl Runtime {
    pub fn is_sandbox(&self) -> bool {
        *self == Runtime::Sandbox
    }
}

/// One tab: its panes, holding indices into [`WorktreeConfig::panels`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabConfig {
    pub layout: PaneLayout,
    /// Index of the focused pane in `layout`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub focused: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
struct LegacySplit {
    axis: SplitAxis,
    secondary: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelSpec {
    Terminal {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Program and arguments. `None` starts the login shell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<Vec<String>>,
        /// Working directory. `None` is the worktree root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        /// The session the terminal runs in on a remote machine or a local
        /// VM. It outlives the app, and the terminal attaches to it again
        /// on the next start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// What the shell in `session` was given of the control socket, so
        /// it still reaches the socket after the terminal attaches again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        control: Option<SessionControl>,
        /// The coding agent open in the terminal, which the next start
        /// resumes there.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentSession>,
        /// Arguments after `command` for this start only, such as the prompt
        /// of an agent another agent launched. Never saved, so the next start
        /// does not send the prompt again.
        #[serde(skip)]
        once: Vec<String>,
    },
    Browser {
        url: String,
    },
    /// The file explorer and reader of the worktree.
    Files {
        /// The open file, relative to the worktree root.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<PathBuf>,
    },
    /// The history and change review of the worktree.
    Explore {
        /// Shows the review of the worktree's changes instead of its history.
        #[serde(default, skip_serializing_if = "is_false")]
        review: bool,
        /// Shows the change request of the worktree's branch.
        #[serde(default, skip_serializing_if = "is_false")]
        change: bool,
        /// The branch the review diffs against. `None` detects it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        /// The review leaves out staged, unstaged and untracked changes.
        #[serde(default, skip_serializing_if = "is_false")]
        committed: bool,
    },
    /// Load, memory, network and disk of the machine the project lives on.
    Monitor,
    /// The app's debug browser, which every such panel shares.
    DebugBrowser,
    /// Every panel of every worktree, the extension tasks and the app's
    /// threads, for debugging.
    Tasks,
    /// Markdown an extension renders (`term.view`).
    View {
        /// `<extension>.<name>`.
        view: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        key: String,
    },
}

/// The control socket as a terminal's remote session knows it: the name
/// of the socket forwarded to the far side and the terminal's token. The
/// shell keeps both in its environment, so a terminal that attaches to
/// the session again after a restart forwards the same socket and
/// registers the same token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionControl {
    pub socket: String,
    pub token: String,
}

/// A coding agent in a terminal and the session it has open, as its hooks
/// name them (`agents.rs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// The agent's program: `claude`, `codex` and so on.
    pub agent: String,
    /// The agent's id for the session. `None` when it named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The agent was in the middle of a turn: working, or asking for a
    /// permission or an answer. The app tells it to go on after resuming.
    #[serde(default, skip_serializing_if = "is_false")]
    pub working: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PanelSpec {
    pub fn shell() -> Self {
        Self::Terminal {
            title: None,
            command: None,
            cwd: None,
            session: None,
            control: None,
            agent: None,
            once: Vec::new(),
        }
    }
}

impl WorktreeConfig {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            panels: vec![PanelSpec::shell()],
            tabs: vec![TabConfig {
                layout: PaneLayout::pane(0),
                focused: 0,
            }],
            active_tab: 0,
            env_vars: Vec::new(),
            runtime: Runtime::Sandbox,
            agent_titles: Vec::new(),
            agent_sessions: Vec::new(),
            active_panel: 0,
            layout: None,
            focused_pane: 0,
            split: None,
        }
    }

    /// The title of the last agent session here.
    pub fn agent_title(&self) -> Option<&str> {
        self.agent_titles.last().map(String::as_str)
    }

    /// Makes `title` the last agent session's. Returns whether that changed.
    pub fn note_agent_title(&mut self, title: &str) -> bool {
        if self.agent_title() == Some(title) {
            return false;
        }
        self.agent_titles.retain(|known| known != title);
        self.agent_titles.push(title.to_owned());
        let extra = self.agent_titles.len().saturating_sub(MAX_AGENT_TITLES);
        self.agent_titles.drain(..extra);
        true
    }

    /// Keeps `agent`'s session with `title`, seen at `now`. Returns whether
    /// that changed anything. A session without an id is not kept, since
    /// nothing could resume it, and a missing title keeps the known one.
    pub fn note_agent_session(
        &mut self,
        agent: &AgentSession,
        title: Option<&str>,
        now: u64,
    ) -> bool {
        let Some(id) = agent.session.as_deref() else {
            return false;
        };
        if let Some(index) = self.agent_sessions.iter().position(|r| r.is(agent)) {
            let record = &mut self.agent_sessions[index];
            if title.is_none() || record.title.as_deref() == title {
                return false;
            }
            record.title = title.map(str::to_owned);
            record.seen = now;
            let record = self.agent_sessions.remove(index);
            self.agent_sessions.push(record);
            return true;
        }
        self.agent_sessions.push(AgentRecord {
            agent: agent.agent.clone(),
            session: id.to_owned(),
            title: title.map(str::to_owned),
            seen: now,
        });
        let extra = self.agent_sessions.len().saturating_sub(MAX_AGENT_SESSIONS);
        self.agent_sessions.drain(..extra);
        true
    }

    /// Saves the tabs and the shown one, dropping the fields of older states.
    pub fn set_tabs(&mut self, tabs: Vec<TabConfig>, active: usize) {
        self.active_tab = active.min(tabs.len().saturating_sub(1));
        self.tabs = tabs;
        self.active_panel = 0;
        self.layout = None;
        self.focused_pane = 0;
        self.split = None;
    }

    /// The saved tabs and the shown one. A state written before tabs held
    /// their own panes gives one tab with its shared panes, holding the
    /// focused panel, and one tab for every panel those panes did not show,
    /// in the order of the old tab strip.
    pub fn tabs(&self) -> (Vec<TabConfig>, usize) {
        if !self.tabs.is_empty() {
            return (self.tabs.clone(), self.active_tab);
        }
        let (layout, focused) = self.legacy_panes();
        let mut shown = Vec::new();
        layout.for_each(&mut |index| shown.push(index));
        let mut tabs = Vec::new();
        let mut active = 0;
        let mut split = Some(TabConfig { layout, focused });
        for index in 0..self.panels.len() {
            if !shown.contains(&index) {
                tabs.push(TabConfig {
                    layout: PaneLayout::pane(index),
                    focused: 0,
                });
            } else if let Some(tab) = split.take() {
                active = tabs.len();
                tabs.push(tab);
            }
        }
        (tabs, active)
    }

    /// The panes shared by every tab in states written before tabs.
    fn legacy_panes(&self) -> (PaneLayout, usize) {
        if let Some(layout) = &self.layout {
            return (layout.clone(), self.focused_pane);
        }
        match self.split {
            Some(LegacySplit { axis, secondary }) => (
                PaneLayout::Split {
                    axis,
                    first: Box::new(PaneLayout::pane(self.active_panel)),
                    second: Box::new(PaneLayout::pane(secondary)),
                    share: 0,
                },
                0,
            ),
            None => (PaneLayout::pane(self.active_panel), 0),
        }
    }
}

impl Project {
    /// The saved config for `path`, if any.
    pub fn worktree(&self, path: &Path) -> Option<&WorktreeConfig> {
        self.worktrees.iter().find(|config| config.path == path)
    }

    /// The saved config for `path`, created with the default layout on first use.
    pub fn worktree_mut(&mut self, path: &Path) -> &mut WorktreeConfig {
        let index = match self.worktrees.iter().position(|config| config.path == path) {
            Some(index) => index,
            None => {
                self.worktrees.push(WorktreeConfig::new(path.to_path_buf()));
                self.worktrees.len() - 1
            }
        };
        &mut self.worktrees[index]
    }

    /// The worktree shown when this project is selected.
    pub fn current_worktree(&self) -> &Path {
        self.active_worktree.as_deref().unwrap_or(&self.root)
    }

    /// A machine's home directory without a repository.
    pub fn is_global(&self) -> bool {
        self.vcs.is_none()
    }

    /// Whether this project uses Jujutsu where it has a jj repository,
    /// with `default` from [`Settings::jj`].
    pub fn uses_jj(&self, default: bool) -> bool {
        self.jj.unwrap_or(default)
    }

    /// The git repository the app recorded for this project, see
    /// [`Self::git_dir`].
    pub fn known_repo(&self) -> Option<crate::vcs::KnownRepo> {
        Some(crate::vcs::KnownRepo {
            root: self.root.clone(),
            git_dir: self.git_dir.clone()?,
        })
    }
}

impl AppState {
    pub fn project(&self, id: u64) -> Option<&Project> {
        self.projects.iter().find(|project| project.id == id)
    }

    pub fn project_mut(&mut self, id: u64) -> Option<&mut Project> {
        self.projects.iter_mut().find(|project| project.id == id)
    }

    pub fn active(&self) -> Option<&Project> {
        self.active_project.and_then(|id| self.project(id))
    }

    /// Adds a project unless one with the same machine and root exists.
    /// Returns its id either way.
    pub fn add_project(
        &mut self,
        name: String,
        env: Environment,
        root: PathBuf,
        vcs: VcsKind,
        forge: Option<ForgeRepo>,
    ) -> u64 {
        self.insert_project(name, env, root, Some(vcs), forge)
    }

    /// Adds a temporary global session in `root`, a home directory, unless
    /// that machine already has one. Returns its id either way. Its
    /// terminals run on the host: a sandbox may write only the worktree,
    /// and a global session is there to change the machine.
    pub fn add_global_session(&mut self, name: String, env: Environment, root: PathBuf) -> u64 {
        let known = self
            .projects
            .iter()
            .any(|p| p.is_global() && p.env == env && p.root == root);
        let id = self.insert_project(name, env, root.clone(), None, None);
        if !known && let Some(project) = self.project_mut(id) {
            project.temporary = true;
            project.worktree_mut(&root).runtime = Runtime::Host;
        }
        id
    }

    fn insert_project(
        &mut self,
        name: String,
        env: Environment,
        root: PathBuf,
        vcs: Option<VcsKind>,
        forge: Option<ForgeRepo>,
    ) -> u64 {
        if let Some(existing) = self.projects.iter().find(|project| {
            project.env == env && project.root == root && project.vcs.is_none() == vcs.is_none()
        }) {
            return existing.id;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.projects.push(Project {
            id,
            name,
            env,
            root,
            vcs,
            git_dir: None,
            temporary: false,
            forge,
            active_worktree: None,
            worktrees: Vec::new(),
            always_allowed: Vec::new(),
            allows_launches: false,
            jj: None,
            base: None,
            sandbox: ProjectSandbox::default(),
            fused: false,
        });
        id
    }

    pub fn remove_project(&mut self, id: u64) {
        self.projects.retain(|project| project.id != id);
        if self.active_project == Some(id) {
            self.active_project = self.projects.first().map(|project| project.id);
        }
    }
}

/// Reads and writes [`AppState`] at a fixed path.
#[derive(Clone, Debug)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store at the default location (see the crate docs).
    pub fn default_location() -> Self {
        Self::new(cmdterm_env::AppDirs::resolve(None).state.join("state.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the state, or returns an empty one when the file does not exist.
    pub fn load(&self) -> io::Result<AppState> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AppState {
                version: STATE_VERSION,
                ..AppState::default()
            }),
            Err(error) => Err(error),
        }
    }

    /// Writes `state` without its temporary projects. When the active
    /// project is one of them, the first kept project becomes active in
    /// the file.
    pub fn save(&self, state: &AppState) -> io::Result<()> {
        if !state.projects.iter().any(|project| project.temporary) {
            return write_json(&self.path, state);
        }
        let mut kept = state.clone();
        kept.projects.retain(|project| !project.temporary);
        if kept
            .active_project
            .is_some_and(|id| kept.project(id).is_none())
        {
            kept.active_project = kept.projects.first().map(|project| project.id);
        }
        write_json(&self.path, &kept)
    }
}

/// Writes `value` as pretty JSON through a sibling temp file and a rename.
/// On Unix only the user may read the file (mode 0600): `state.json` holds
/// the control socket tokens of remote sessions.
fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let temp = path.with_extension("json.tmp");
    // A temp file left by a crash keeps its mode when opened again.
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    io::Write::write_all(&mut file, &bytes)?;
    drop(file);
    std::fs::rename(&temp, path)
}

/// Default terminal font size in logical pixels.
pub const DEFAULT_FONT_SIZE: f32 = 14.0;
/// Smallest and largest terminal font size the settings accept.
pub const FONT_SIZE_RANGE: (f32, f32) = (6.0, 48.0);
/// The color scheme id that follows the desktop's light or dark preference.
pub const SYSTEM_SCHEME: &str = "system";
/// The language id that follows the system locale.
pub const SYSTEM_LANGUAGE: &str = "system";
/// The keymap preset used when the settings name none.
pub const DEFAULT_KEYMAP: &str = "cmdterm";

/// App-wide preferences. Missing fields take their defaults, so an old or
/// hand-trimmed file still loads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Terminal font size in logical pixels.
    pub font_size: f32,
    /// Installed font family for terminals. `None` is the bundled mono face.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// A scheme id from the app's scheme list, or [`SYSTEM_SCHEME`].
    pub color_scheme: String,
    /// Extensions listed in the settings but not loaded, by name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub disabled_extensions: Vec<String>,
    /// Extensions whose manifest says `enabled = false` that the user
    /// turned on, by name.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enabled_extensions: Vec<String>,
    /// Permissions the user granted, per extension name, such as
    /// `"npm": ["env", "secrets"]`. An extension whose `manifest.toml`
    /// declares more is not loaded. Shipped extensions have what their
    /// shipped manifest declares until an entry here replaces it; revoking
    /// one leaves an empty entry.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extension_permissions: BTreeMap<String, Vec<String>>,
    /// Values of the settings extensions declare in their manifests, per
    /// extension name, then setting key. Missing ones take the manifest's
    /// default.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extension_settings: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
    /// Forge accounts the user signed in to on the Accounts page, from
    /// `<forge id>/<host>` to the name the forge gave the token's owner.
    /// The tokens themselves live in the system's secret storage.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub forge_accounts: BTreeMap<String, String>,
    /// The accounts of each forge, by forge id such as `forgejo.forgejo`,
    /// as the Accounts page of the settings edits them. A forge without an
    /// entry has the accounts its extension names.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub forge_hosts: BTreeMap<String, Vec<String>>,
    /// Which extensions the Extensions page of the settings lists.
    pub extension_filter: ExtensionFilter,
    /// Keymap preset id: `cmdterm`, `tmux`, `vscode`, `cursor`,
    /// `windows-terminal` or `konsole`.
    pub keymap: String,
    /// The user's own bindings, from a key sequence such as `"ctrl+b |"` to an
    /// action name. `null` removes the preset's or an extension's binding.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keybindings: BTreeMap<String, Option<String>>,
    /// UI language id: `en`, `de`, or [`SYSTEM_LANGUAGE`].
    pub language: String,
    /// Where the terminals of a worktree the app has not seen yet run.
    /// The first-start setup asks for it.
    #[serde(skip_serializing_if = "Runtime::is_sandbox")]
    pub default_runtime: Runtime,
    /// What sandboxed worktrees may reach.
    pub sandbox: SandboxSettings,
    /// Container engine for devcontainers: `docker`, `podman`, or a path.
    /// `None` uses docker when the host has it, else podman.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_engine: Option<String>,
    /// Whether the app lets other machines connect to this one.
    pub remote: RemoteSettings,
    /// Where the VPN networks page finds Tailscale, NetBird and Nebula.
    #[serde(skip_serializing_if = "VpnSettings::is_default")]
    pub vpn: VpnSettings,
    /// Whether the app looks for outdated agent integrations and newer
    /// releases of agents installed with npm or mise, at start and every
    /// 12 hours.
    pub check_updates: bool,
    /// Whether an update check that finds a newer scriptschnell-term
    /// installs it right away, on Windows and in an AppImage. Off, the bell
    /// offers it with an Update button.
    pub auto_update: bool,
    /// Minutes an agent may sit idle in a hidden terminal before the app
    /// stops it, to resume its session when the terminal shows again. Zero
    /// keeps every agent running.
    pub freeze_idle_agents: u32,
    /// How much CPU terminals may use, on this machine and on others. The
    /// palette switches it while they run.
    pub cpu_mode: CpuModes,
    /// On Linux, agents a terminal runs directly get bash as `$SHELL`
    /// instead of the user's shell, on this machine and in its sandboxes.
    /// Agents run their tool commands in `$SHELL`, and many expect a POSIX
    /// shell there.
    pub agents_use_bash: bool,
    /// Which terminals get the control socket agents use to read and change
    /// extensions, with the user's approval for every change.
    pub agent_control: AgentControl,
    /// The debug browser: one Chrome or Firefox for the whole app, shown in
    /// debug browser panels and driven by agents as MCP tools.
    pub debug_browser: DebugBrowserSettings,
    /// What the rail names a worktree after.
    pub worktree_titles: WorktreeTitles,
    /// Who draws the window's title bar and its buttons.
    pub window_frame: WindowFrame,
    /// Read a repository that has a Jujutsu workspace with `jj`, even when
    /// git shares its directory. Projects can say otherwise
    /// ([`Project::jj`]).
    #[serde(skip_serializing_if = "is_false")]
    pub jj: bool,
    /// Windows: the shell new terminals start. `None` is `%COMSPEC%`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellChoice>,
    /// Windows: more shells the new panel list offers, after New shell.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<ShellChoice>,
}

/// A shell in the settings: one the app finds by id (`git-bash`, `pwsh`,
/// `powershell`, `cmd`, `wsl`), a program, or a program with arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ShellChoice {
    /// An id of [`cmdterm_env::installed_shells`], else a program.
    Name(String),
    /// Program and arguments.
    Command(Vec<String>),
}

impl ShellChoice {
    /// What to start and what to call it, from `installed` (the shells
    /// [`cmdterm_env::installed_shells`] found). `None` when an id names a
    /// shell that is not installed, or the command is empty.
    pub fn resolve(
        &self,
        installed: &[cmdterm_env::ShellOption],
    ) -> Option<(String, Vec<std::ffi::OsString>)> {
        let argv: Vec<std::ffi::OsString> = match self {
            ShellChoice::Name(name) => {
                if let Some(shell) = installed.iter().find(|shell| shell.id == name) {
                    return Some((shell.name.to_owned(), shell.argv.clone()));
                }
                if crate::state::KNOWN_SHELLS.contains(&name.as_str()) || name.is_empty() {
                    return None;
                }
                vec![name.into()]
            }
            ShellChoice::Command(argv) => argv.iter().map(Into::into).collect(),
        };
        let program = Path::new(argv.first()?);
        let name = program.file_stem().unwrap_or(program.as_os_str());
        Some((name.to_string_lossy().into_owned(), argv))
    }

    /// The id this choice names, if it names one.
    pub fn id(&self) -> Option<&str> {
        match self {
            ShellChoice::Name(name) => Some(name),
            ShellChoice::Command(_) => None,
        }
    }
}

/// The ids [`ShellChoice::Name`] may name. One that is not installed
/// resolves to nothing rather than to a program of that name.
pub const KNOWN_SHELLS: [&str; 5] = ["git-bash", "pwsh", "powershell", "cmd", "wsl"];

/// What the rail names a worktree after.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeTitles {
    /// The title of the last agent session in its terminals, else its
    /// branch. The branch moves under the title.
    #[default]
    Agent,
    /// Its branch, or its revision when detached.
    Branch,
}

/// Who draws the window's title bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowFrame {
    /// The window opens without the system's frame. The tab strip carries
    /// the minimize, maximize and close buttons, its empty part moves the
    /// window, and the window's edges resize it.
    #[default]
    App,
    /// The window system draws the title bar and the border.
    System,
}

/// The debug browser's settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DebugBrowserSettings {
    /// `auto` (Chrome or Chromium when installed, else Firefox), `chrome`
    /// or `firefox`.
    pub browser: DebugBrowserKind,
    /// The browser's executable. `None` looks on `PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Runs without a window of its own; the panels show it.
    pub headless: bool,
    /// Which terminals' agents may drive it.
    pub agents: DebugBrowserAgents,
}

impl Default for DebugBrowserSettings {
    fn default() -> Self {
        Self {
            browser: DebugBrowserKind::Auto,
            executable: None,
            headless: true,
            agents: DebugBrowserAgents::Local,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugBrowserKind {
    #[default]
    Auto,
    Chrome,
    Firefox,
}

/// Which terminals' agents get the debug browser's tools.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DebugBrowserAgents {
    /// Every terminal that reaches the control socket, also on SSH hosts,
    /// remote machines and VMs.
    On,
    /// Terminals on this machine: unsandboxed, in sandboxes and in
    /// devcontainers.
    #[default]
    Local,
    Off,
}

/// Which terminals reach the app's control socket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentControl {
    /// Every terminal that can: on this machine, in sandboxes, on SSH hosts,
    /// remote machines and VMs.
    On,
    /// Only terminals on this machine and in its sandboxes.
    Local,
    /// Only terminals that run directly on this machine, outside sandboxes
    /// and devcontainers.
    #[default]
    Host,
    Off,
}

/// Which extensions the Extensions page of the settings lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionFilter {
    /// Every extension, so one that ships turned off can be found.
    #[default]
    All,
    /// The ones not turned off, whether they loaded or not.
    Active,
    /// The ones turned off.
    Disabled,
}

impl ExtensionFilter {
    /// Whether an extension that is `turned_off` or not passes.
    pub fn shows(self, turned_off: bool) -> bool {
        match self {
            Self::All => true,
            Self::Active => !turned_off,
            Self::Disabled => turned_off,
        }
    }
}

/// This machine as a remote machine for others, while the app runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteSettings {
    /// Serve at startup and keep serving.
    pub serve: bool,
    /// Address and port to listen on.
    pub listen: String,
    /// Remote machines whose projects the sidebar shows next to this
    /// machine's own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fused: Vec<FusedMachine>,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            serve: false,
            listen: "0.0.0.0:7722".to_owned(),
            fused: Vec::new(),
        }
    }
}

/// A remote machine whose shared projects show in the sidebar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FusedMachine {
    /// `host:port`, as `known_hosts` names it.
    pub address: String,
    /// Roots of shared projects the user removed from the sidebar, which
    /// stay out of it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<PathBuf>,
}

/// How much of a machine's CPU the terminals there may use, see
/// [`crate::cpu`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuMode {
    /// No cap, and hidden agents keep their share.
    Off,
    /// One core stays free, and agents in hidden terminals run at nice 10,
    /// unless [`CpuModes::balanced`] says otherwise.
    #[default]
    Balanced,
    /// Half the cores stay free, and agents in hidden terminals run at
    /// nice 19, unless [`CpuModes::quiet`] says otherwise.
    Quiet,
}

/// The [`CpuMode`] of terminals on this machine, also in its sandboxes,
/// devcontainers and VMs, and of terminals on other machines, and what the
/// balanced and quiet modes do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CpuModes {
    pub local: CpuMode,
    pub remote: CpuMode,
    #[serde(skip_serializing_if = "CpuTuning::is_default")]
    pub balanced: CpuTuning,
    #[serde(skip_serializing_if = "CpuTuning::is_default")]
    pub quiet: CpuTuning,
    /// On machines with efficiency and performance cores: hidden
    /// terminals run on the efficiency cores, and the cores a mode keeps
    /// free are performance cores. False leaves the cores to the kernel.
    #[serde(skip_serializing_if = "is_true")]
    pub efficiency_cores: bool,
}

fn is_true(value: &bool) -> bool {
    *value
}

impl Default for CpuModes {
    fn default() -> Self {
        Self {
            local: CpuMode::Balanced,
            remote: CpuMode::Off,
            balanced: CpuTuning::default(),
            quiet: CpuTuning::default(),
            efficiency_cores: true,
        }
    }
}

/// Changes to what a [`CpuMode`] does. A value left out keeps the mode's
/// own, so `{"max_percent": 25}` changes only the cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CpuTuning {
    /// Cores the machine's terminals leave free together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_cores: Option<u32>,
    /// Percent of the cores they may use at most.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_percent: Option<u32>,
    /// Nice level of agents in hidden terminals, 0 to 19.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_nice: Option<u8>,
    /// Whether shown terminals may use the performance cores, on machines
    /// that have efficiency cores too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance_cores: Option<bool>,
}

impl CpuTuning {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Where the VPN networks page looks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VpnSettings {
    /// Nebula's configuration file or directory. `None` takes the one the
    /// running `nebula` was started with, else `/etc/nebula/config.yml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nebula_config: Option<String>,
}

impl VpnSettings {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Hosts that belong together, which the first start offers to allow as
/// one switch. The app names a group by its `id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostGroup {
    pub id: &'static str,
    /// Allowed in the default settings.
    pub default: bool,
    pub hosts: &'static [&'static str],
}

/// The groups the first start asks about, in the order it shows them. No
/// host is in two groups.
pub const HOST_GROUPS: &[HostGroup] = &[
    HostGroup {
        id: "forges",
        default: true,
        hosts: &[
            "github.com",
            "*.github.com",
            "*.githubusercontent.com",
            "gitlab.com",
            "codeberg.org",
            "bitbucket.org",
        ],
    },
    HostGroup {
        id: "search",
        default: true,
        hosts: &[
            "duckduckgo.com",
            "*.duckduckgo.com",
            "www.google.com",
            "www.bing.com",
            "search.brave.com",
            "kagi.com",
            "www.startpage.com",
        ],
    },
    HostGroup {
        id: "search_apis",
        default: true,
        hosts: &[
            "api.exa.ai",
            // opencode's web and code search tools call Exa's hosted MCP server.
            "mcp.exa.ai",
            "api.tavily.com",
            "mcp.tavily.com",
            "api.search.brave.com",
            "api.bing.microsoft.com",
            "google.serper.dev",
            "serpapi.com",
            "s.jina.ai",
            "r.jina.ai",
            "api.firecrawl.dev",
            "api.perplexity.ai",
        ],
    },
    HostGroup {
        id: "ai",
        default: true,
        hosts: &[
            "api.anthropic.com",
            "api.openai.com",
            "generativelanguage.googleapis.com",
            "openrouter.ai",
            "api.mistral.ai",
            "api.groq.com",
            "api.deepseek.com",
            // GitHub Copilot CLI.
            "*.githubcopilot.com",
            // Gemini CLI and Antigravity CLI with a Google login.
            "cloudcode-pa.googleapis.com",
            // Antigravity CLI.
            "businessaicode.googleapis.com",
            // Qwen Code.
            "dashscope.aliyuncs.com",
            "coding.dashscope.aliyuncs.com",
            "coding-intl.dashscope.aliyuncs.com",
            "chat.qwen.ai",
            // Kimi Code.
            "api.kimi.com",
            "auth.kimi.com",
            "api.moonshot.ai",
        ],
    },
    HostGroup {
        id: "docs",
        default: true,
        hosts: &[
            "docs.rs",
            "doc.rust-lang.org",
            "docs.python.org",
            "developer.mozilla.org",
            "stackoverflow.com",
            "*.stackexchange.com",
            "*.wikipedia.org",
        ],
    },
    HostGroup {
        id: "rust",
        default: true,
        hosts: &["crates.io", "*.crates.io", "static.rust-lang.org"],
    },
    HostGroup {
        id: "javascript",
        default: true,
        hosts: &["registry.npmjs.org", "registry.yarnpkg.com", "nodejs.org"],
    },
    HostGroup {
        id: "python",
        default: true,
        hosts: &["pypi.org", "files.pythonhosted.org"],
    },
    HostGroup {
        id: "go",
        default: true,
        hosts: &["proxy.golang.org", "sum.golang.org"],
    },
    HostGroup {
        id: "local",
        default: true,
        hosts: &["localhost", "127.0.0.1", "::1"],
    },
    HostGroup {
        id: "jvm",
        default: false,
        hosts: &[
            "repo.maven.apache.org",
            "repo1.maven.org",
            "plugins.gradle.org",
            "services.gradle.org",
            "downloads.gradle.org",
        ],
    },
    HostGroup {
        id: "dotnet",
        default: false,
        hosts: &["api.nuget.org", "www.nuget.org", "globalcdn.nuget.org"],
    },
    HostGroup {
        id: "ruby_php",
        default: false,
        hosts: &[
            "rubygems.org",
            "index.rubygems.org",
            "packagist.org",
            "repo.packagist.org",
        ],
    },
    HostGroup {
        id: "containers",
        default: false,
        hosts: &[
            "registry-1.docker.io",
            "auth.docker.io",
            "production.cloudflare.docker.com",
            "ghcr.io",
            "quay.io",
            "*.quay.io",
            "registry.gitlab.com",
        ],
    },
    HostGroup {
        id: "linux",
        default: false,
        hosts: &[
            "deb.debian.org",
            "security.debian.org",
            "archive.ubuntu.com",
            "security.ubuntu.com",
            "dl-cdn.alpinelinux.org",
        ],
    },
];

/// How sandboxes reach the container engines (docker, podman, containerd,
/// buildkit) of the machine they run on, and WSL interop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineAccess {
    /// The engines' API sockets or pipes are open to sandboxes as they are.
    /// The engines run outside the sandbox, so what they start is not
    /// sandboxed.
    #[default]
    Passthrough,
    /// Each sandboxed worktree gets an engine of its own in a container on
    /// this machine's engine ([`SidecarSettings`]), and cannot reach the
    /// host's engines. Linux only.
    Sidecar,
    /// Sandboxes cannot reach the engines.
    Off,
}

/// The engine inside a sidecar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarEngine {
    /// podman in podman (`quay.io/podman/stable`).
    #[default]
    Podman,
    /// docker in docker (`docker:dind`).
    Docker,
}

/// How [`EngineAccess::Sidecar`] runs its engines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidecarSettings {
    pub engine: SidecarEngine,
    /// An image in place of the engine's default one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// What a sandboxed worktree may reach besides the worktree itself. Paths
/// may start with `~/`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    /// Extra writable paths, such as caches.
    pub writable: Vec<String>,
    /// Files and directories the sandbox must not read: credentials and
    /// keys. Missing ones are skipped.
    pub hidden: Vec<String>,
    /// Names of variables that look like credentials but reach sandboxes
    /// anyway, ignoring case. Every other such variable the app inherited
    /// is removed in local sandboxes, see `runtime::sandbox_env`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    pub network: bool,
    /// With `network` on, sandboxes reach only `allowed_hosts`, through a
    /// filtering proxy on this machine.
    pub restrict_network: bool,
    /// With `network` on, a sandbox on this machine keeps the host's network
    /// (and so the host's ports) instead of getting a loopback of its own.
    /// Off, servers in sandboxes bind no host ports, the app forwards them
    /// where needed, and traffic leaves only through the filtering proxy.
    /// Linux with bubblewrap only; other sandboxes ignore it.
    pub host_network: bool,
    /// Experimental, Windows only. Runs sandboxed commands in an AppContainer
    /// (a profile per project) instead of under a write-restricted token.
    /// The AppContainer has no network capability: it reaches the filtering
    /// proxy and the port hub through a loopback exemption, which needs an
    /// administrator once. Without it the launcher says why and uses the
    /// restricted token.
    pub windows_appcontainer: bool,
    /// Host names, or `*.example.com` for every subdomain.
    pub allowed_hosts: Vec<String>,
    /// With `restrict_network`, the proxy decrypts HTTPS with its own CA,
    /// which sandboxes trust. Hosts with invalid certificates get a second
    /// CA nothing trusts.
    pub intercept_tls: bool,
    /// The first-start setup, which ends with the question about
    /// [`HOST_GROUPS`], was finished. Until then every start opens it.
    pub hosts_chosen: bool,
    /// Whether sandboxes on this machine reach its container engines and,
    /// inside WSL, Windows.
    pub engines: EngineAccess,
    /// The user picked passthrough or said to keep it, so the status bar
    /// no longer warns about it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub engines_acknowledged: bool,
    pub sidecar: SidecarSettings,
    /// The user's choice per extension guard (`<extension>.<name>`), such
    /// as whether sandboxes may `git push`. Missing ones follow
    /// [`Self::change_requests`] when they have a kind, else ask.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub guards: BTreeMap<String, GuardMode>,
    /// The mode of every forge's guards on change requests, per kind, for
    /// guards without a choice of their own in [`Self::guards`].
    #[serde(skip_serializing_if = "ChangeRequestModes::is_default")]
    pub change_requests: ChangeRequestModes,
}

/// What sandboxes may do to change requests on every forge. Each kind
/// asks by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChangeRequestModes {
    pub create: GuardMode,
    pub comment: GuardMode,
    pub merge: GuardMode,
}

impl ChangeRequestModes {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    #[must_use]
    pub const fn get(&self, kind: GuardKind) -> GuardMode {
        match kind {
            GuardKind::Create => self.create,
            GuardKind::Comment => self.comment,
            GuardKind::Merge => self.merge,
        }
    }

    pub fn set(&mut self, kind: GuardKind, mode: GuardMode) {
        match kind {
            GuardKind::Create => self.create = mode,
            GuardKind::Comment => self.comment = mode,
            GuardKind::Merge => self.merge = mode,
        }
    }
}

impl SandboxSettings {
    /// True when every host of `group` is allowed.
    pub fn group_allowed(&self, group: &HostGroup) -> bool {
        group
            .hosts
            .iter()
            .all(|host| self.allowed_hosts.iter().any(|h| h == host))
    }

    /// Adds the hosts of `group` to the allowed hosts, or removes them.
    /// Returns whether anything changed.
    pub fn allow_group(&mut self, group: &HostGroup, on: bool) -> bool {
        let before = self.allowed_hosts.len();
        if on {
            for host in group.hosts {
                if !self.allowed_hosts.iter().any(|h| h == host) {
                    self.allowed_hosts.push((*host).to_owned());
                }
            }
        } else {
            self.allowed_hosts
                .retain(|h| !group.hosts.contains(&h.as_str()));
        }
        self.allowed_hosts.len() != before
    }

    /// The mode of guard `id` of `kind`: the user's choice for it, else
    /// the one for its kind, else [`GuardMode::Ask`].
    pub fn guard_mode(&self, id: &str, kind: Option<GuardKind>) -> GuardMode {
        (self.guards.get(id).copied()).unwrap_or_else(|| self.inherited_mode(kind))
    }

    /// The mode a guard of `kind` has without a choice of its own.
    pub fn inherited_mode(&self, kind: Option<GuardKind>) -> GuardMode {
        kind.map_or_else(GuardMode::default, |kind| self.change_requests.get(kind))
    }

    /// Sets the mode of guard `id` of `kind`. A mode equal to the one it
    /// inherits drops its own choice, so it follows its kind again.
    /// Returns whether anything changed.
    pub fn set_guard_mode(&mut self, id: &str, kind: Option<GuardKind>, mode: GuardMode) -> bool {
        let before = self.guards.get(id).copied();
        if mode == self.inherited_mode(kind) {
            self.guards.remove(id);
        } else {
            self.guards.insert(id.to_owned(), mode);
        }
        self.guards.get(id).copied() != before
    }
}

impl Default for SandboxSettings {
    fn default() -> Self {
        let paths = |list: &[&str]| list.iter().map(|p| (*p).to_owned()).collect();
        Self {
            writable: paths(&["~/.cache"]),
            hidden: paths(&[
                "~/.ssh",
                "~/.gnupg",
                "~/.aws",
                "~/.azure",
                "~/.kube",
                "~/.docker",
                "~/.netrc",
                "~/.git-credentials",
                "~/.password-store",
                "~/.config/gh",
                "~/.config/gcloud",
                "~/.config/scriptschnell-term",
                "~/.local/share/keyrings",
                "~/Library/Keychains",
            ]),
            pass_env: Vec::new(),
            network: true,
            restrict_network: true,
            host_network: false,
            windows_appcontainer: false,
            intercept_tls: true,
            hosts_chosen: false,
            engines: EngineAccess::default(),
            engines_acknowledged: false,
            sidecar: SidecarSettings::default(),
            guards: BTreeMap::new(),
            change_requests: ChangeRequestModes::default(),
            allowed_hosts: HOST_GROUPS
                .iter()
                .filter(|group| group.default)
                .flat_map(|group| group.hosts)
                .map(|host| (*host).to_owned())
                .collect(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            font_family: None,
            color_scheme: SYSTEM_SCHEME.to_owned(),
            disabled_extensions: Vec::new(),
            enabled_extensions: Vec::new(),
            extension_permissions: BTreeMap::new(),
            extension_settings: BTreeMap::new(),
            forge_accounts: BTreeMap::new(),
            forge_hosts: BTreeMap::new(),
            extension_filter: ExtensionFilter::default(),
            keymap: DEFAULT_KEYMAP.to_owned(),
            keybindings: BTreeMap::new(),
            language: SYSTEM_LANGUAGE.to_owned(),
            default_runtime: Runtime::Sandbox,
            sandbox: SandboxSettings::default(),
            container_engine: None,
            remote: RemoteSettings::default(),
            vpn: VpnSettings::default(),
            check_updates: true,
            auto_update: true,
            freeze_idle_agents: 15,
            cpu_mode: CpuModes::default(),
            agents_use_bash: true,
            agent_control: AgentControl::Host,
            debug_browser: DebugBrowserSettings::default(),
            worktree_titles: WorktreeTitles::Agent,
            window_frame: WindowFrame::App,
            jj: false,
            shell: None,
            shells: Vec::new(),
        }
    }
}

impl Settings {
    /// `size` rounded to half pixels and kept inside [`FONT_SIZE_RANGE`].
    /// Non-finite input gives the default.
    pub fn clamp_font_size(size: f32) -> f32 {
        if !size.is_finite() {
            return DEFAULT_FONT_SIZE;
        }
        ((size * 2.0).round() / 2.0).clamp(FONT_SIZE_RANGE.0, FONT_SIZE_RANGE.1)
    }

    /// Whether extension `name` loads. `default_on` is false for one whose
    /// manifest says `enabled = false`.
    pub fn extension_enabled(&self, name: &str, default_on: bool) -> bool {
        if self.disabled_extensions.iter().any(|n| n == name) {
            return false;
        }
        default_on || self.enabled_extensions.iter().any(|n| n == name)
    }

    /// Grants extension `name` the permissions `names`, on top of what it
    /// had. Returns whether anything changed.
    pub fn grant_extension(&mut self, name: &str, names: &[String]) -> bool {
        let granted = self
            .extension_permissions
            .entry(name.to_owned())
            .or_default();
        let before = granted.len();
        for permission in names {
            if !granted.contains(permission) {
                granted.push(permission.clone());
            }
        }
        granted.sort();
        granted.len() != before
    }

    /// Moves the account lists the shipped forge extensions kept in their
    /// own settings before the Accounts page into [`Self::forge_hosts`].
    /// Returns whether anything moved.
    pub fn move_forge_hosts(&mut self) -> bool {
        const OLD: [(&str, &str, &str); 6] = [
            ("github", "hosts", "github.github"),
            ("gitlab", "hosts", "gitlab.gitlab"),
            ("forgejo", "hosts", "forgejo.forgejo"),
            ("gitea", "hosts", "gitea.gitea"),
            ("bitbucket", "accounts", "bitbucket.bitbucket"),
            ("azure-devops", "accounts", "azure-devops.azure-devops"),
        ];
        let mut moved = false;
        for (extension, key, forge) in OLD {
            let Some(own) = self.extension_settings.get_mut(extension) else {
                continue;
            };
            let Some(value) = own.remove(key) else {
                continue;
            };
            if own.is_empty() {
                self.extension_settings.remove(extension);
            }
            moved = true;
            let Ok(hosts) = serde_json::from_value::<Vec<String>>(value) else {
                continue;
            };
            self.forge_hosts.entry(forge.to_owned()).or_insert(hosts);
        }
        moved
    }

    /// Sets setting `key` of extension `name`, or clears it with `None`.
    /// Returns whether anything changed.
    pub fn set_extension_setting(
        &mut self,
        name: &str,
        key: &str,
        value: Option<serde_json::Value>,
    ) -> bool {
        let Some(value) = value else {
            let Some(own) = self.extension_settings.get_mut(name) else {
                return false;
            };
            let removed = own.remove(key).is_some();
            if own.is_empty() {
                self.extension_settings.remove(name);
            }
            return removed;
        };
        let own = self.extension_settings.entry(name.to_owned()).or_default();
        own.insert(key.to_owned(), value.clone()) != Some(value)
    }

    /// Takes every permission from extension `name`. Returns whether
    /// anything changed.
    pub fn revoke_extension(&mut self, name: &str) -> bool {
        self.extension_permissions.remove(name).is_some()
    }

    /// Turns an extension on or off. `default_on` is as for
    /// [`Self::extension_enabled`]. Returns whether anything changed.
    pub fn set_extension_enabled(&mut self, name: &str, enabled: bool, default_on: bool) -> bool {
        if self.extension_enabled(name, default_on) == enabled {
            return false;
        }
        self.disabled_extensions.retain(|n| n != name);
        self.enabled_extensions.retain(|n| n != name);
        let list = match (enabled, default_on) {
            (false, _) => &mut self.disabled_extensions,
            (true, false) => &mut self.enabled_extensions,
            (true, true) => return true,
        };
        list.push(name.to_owned());
        list.sort();
        true
    }
}

/// Reads and writes [`Settings`] at a fixed path.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the settings, or the defaults when the file does not exist.
    pub fn load(&self) -> io::Result<Settings> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
                .map(|mut settings| {
                    settings.font_size = Settings::clamp_font_size(settings.font_size);
                    settings.move_forge_hosts();
                    settings
                })
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, settings: &Settings) -> io::Result<()> {
        write_json(&self.path, settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_account_lists_move_out_of_extension_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"extension_settings": {
                "forgejo": {"hosts": ["codeberg.org", "git.example.org"]},
                "bitbucket": {"accounts": ["bitbucket.org/acme"], "other": 1}
            }}"#,
        )
        .unwrap();
        let settings = SettingsStore::new(&path).load().unwrap();
        assert_eq!(
            settings.forge_hosts["forgejo.forgejo"],
            ["codeberg.org", "git.example.org"]
        );
        assert_eq!(
            settings.forge_hosts["bitbucket.bitbucket"],
            ["bitbucket.org/acme"]
        );
        assert!(!settings.extension_settings.contains_key("forgejo"));
        assert_eq!(
            settings.extension_settings["bitbucket"]
                .keys()
                .collect::<Vec<_>>(),
            ["other"]
        );
    }

    #[test]
    fn settings_round_trip_and_default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path().join("settings.json"));
        assert_eq!(store.load().unwrap(), Settings::default());
        let mut settings = Settings {
            font_size: 16.5,
            font_family: Some("Iosevka".into()),
            color_scheme: "nord".into(),
            language: "de".into(),
            ..Settings::default()
        };
        settings.keymap = "tmux".into();
        settings.extension_filter = ExtensionFilter::Disabled;
        settings
            .keybindings
            .insert("ctrl+b |".into(), Some("split_side_by_side".into()));
        settings.keybindings.insert("ctrl+b %".into(), None);
        let sandbox = &mut settings.sandbox;
        assert!(sandbox.set_guard_mode("git.push", None, GuardMode::Block));
        assert!(!sandbox.set_guard_mode("git.push", None, GuardMode::Block));
        assert!(!sandbox.set_guard_mode("npm.publish", None, GuardMode::Ask));
        assert_eq!(sandbox.guard_mode("npm.publish", None), GuardMode::Ask);
        // A forge guard follows its kind until it gets a mode of its own.
        let merge = Some(GuardKind::Merge);
        sandbox.change_requests.merge = GuardMode::Block;
        assert_eq!(sandbox.guard_mode("github.merge", merge), GuardMode::Block);
        assert!(sandbox.set_guard_mode("github.merge", merge, GuardMode::Ask));
        assert_eq!(sandbox.guard_mode("github.merge", merge), GuardMode::Ask);
        assert!(sandbox.set_guard_mode("github.merge", merge, GuardMode::Block));
        assert!(!sandbox.guards.contains_key("github.merge"));
        assert!(settings.set_extension_enabled("zeta", false, true));
        assert!(settings.set_extension_enabled("alpha", false, true));
        assert!(!settings.set_extension_enabled("alpha", false, true));
        assert_eq!(settings.disabled_extensions, ["alpha", "zeta"]);
        assert!(!settings.extension_enabled("quiet", false));
        assert!(settings.set_extension_enabled("quiet", true, false));
        assert!(settings.extension_enabled("quiet", false));
        assert_eq!(settings.enabled_extensions, ["quiet"]);
        assert!(settings.set_extension_enabled("quiet", false, false));
        assert!(settings.enabled_extensions.is_empty());
        assert!(!settings.extension_enabled("quiet", false));
        assert!(settings.grant_extension("npm", &["secrets".into(), "env".into()]));
        assert!(!settings.grant_extension("npm", &["env".into()]));
        assert_eq!(settings.extension_permissions["npm"], ["env", "secrets"]);
        let files = serde_json::json!([".gitconfig"]);
        assert!(settings.set_extension_setting("dotfiles", "files", Some(files.clone())));
        assert!(!settings.set_extension_setting("dotfiles", "files", Some(files)));
        store.save(&settings).unwrap();
        assert_eq!(store.load().unwrap(), settings);
        let saved = std::fs::read_to_string(dir.path().join("settings.json")).unwrap();
        assert!(saved.contains(r#""git.push": "block""#), "{saved}");
        assert!(saved.contains(r#""merge": "block""#), "{saved}");
        assert!(
            settings
                .sandbox
                .set_guard_mode("git.push", None, GuardMode::Ask)
        );
        assert!(settings.sandbox.guards.is_empty());
        assert!(settings.set_extension_enabled("alpha", true, true));
        assert!(settings.extension_enabled("alpha", true));
        assert!(!settings.extension_enabled("zeta", true));
    }

    #[test]
    fn partial_settings_files_fill_in_defaults_and_clamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"font_size": 300}"#).unwrap();
        let settings = SettingsStore::new(&path).load().unwrap();
        assert_eq!(settings.font_size, FONT_SIZE_RANGE.1);
        assert_eq!(settings.color_scheme, SYSTEM_SCHEME);
        assert_eq!(settings.keymap, DEFAULT_KEYMAP);
        assert!(settings.keybindings.is_empty());
        assert_eq!(Settings::clamp_font_size(13.26), 13.5);
        assert_eq!(Settings::clamp_font_size(f32::NAN), DEFAULT_FONT_SIZE);
    }

    #[test]
    fn missing_file_loads_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let state = store.load().unwrap();
        assert!(state.projects.is_empty());
        assert_eq!(state.version, STATE_VERSION);
    }

    /// The state holds control socket tokens, so only the user may read
    /// it, also when an older version wrote it readable for everyone.
    #[cfg(unix)]
    #[test]
    fn saved_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, "left by a crash").unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644)).unwrap();
        StateStore::new(&path).save(&AppState::default()).unwrap();
        let settings = dir.path().join("settings.json");
        SettingsStore::new(&settings)
            .save(&Settings::default())
            .unwrap();
        for file in [&path, &settings] {
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
        assert!(!temp.exists());
        StateStore::new(&path).load().unwrap();
    }

    #[test]
    fn state_round_trips_with_worktree_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("nested/state.json"));
        let mut state = AppState::default();
        let id = state.add_project(
            "app".into(),
            Environment::ssh("dev"),
            "/srv/app".into(),
            VcsKind::Git,
            None,
        );
        let project = state.project_mut(id).unwrap();
        project.active_worktree = Some("/srv/app-feature".into());
        let config = project.worktree_mut(Path::new("/srv/app-feature"));
        config.panels.push(PanelSpec::Browser {
            url: "http://localhost:3000".into(),
        });
        config.panels.push(PanelSpec::Terminal {
            title: None,
            command: None,
            cwd: None,
            session: Some("5e55".into()),
            control: Some(SessionControl {
                socket: "scriptschnell-term-0a1b".into(),
                token: "c0ffee".into(),
            }),
            agent: Some(AgentSession {
                agent: "claude".into(),
                session: Some("3f2a".into()),
                working: true,
            }),
            once: Vec::new(),
        });
        let mut layout = PaneLayout::pane(1);
        layout.split(0, SplitAxis::Stacked, 2);
        let tabs = vec![
            TabConfig {
                layout: PaneLayout::pane(0),
                focused: 0,
            },
            TabConfig { layout, focused: 1 },
        ];
        config.set_tabs(tabs.clone(), 1);
        state.active_project = Some(id);
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, state);
        let config = loaded.projects[0].worktree(Path::new("/srv/app-feature"));
        assert_eq!(config.unwrap().tabs(), (tabs, 1));
        let json = std::fs::read_to_string(dir.path().join("nested/state.json")).unwrap();
        assert!(json.contains("\"stacked\""), "{json}");
        assert!(!json.contains("active_panel"), "{json}");
        assert!(json.contains("\"session\": \"5e55\""), "{json}");
        assert!(json.contains("\"agent\": \"claude\""), "{json}");
        // Terminals saved before sessions existed load without one.
        let old: PanelSpec = serde_json::from_str(r#"{"kind":"terminal"}"#).unwrap();
        assert_eq!(old, PanelSpec::shell());
    }

    #[test]
    fn host_groups_are_valid_and_disjoint() {
        let mut seen = std::collections::BTreeSet::new();
        for group in HOST_GROUPS {
            for host in group.hosts {
                assert!(cmdterm_env::valid_host_pattern(host), "{host}");
                assert!(seen.insert(*host), "{host} is in two groups");
            }
        }
        let sandbox = SandboxSettings::default();
        assert!(sandbox.restrict_network && sandbox.intercept_tls);
        assert!(!sandbox.host_network);
        let old: SandboxSettings = serde_json::from_str(r#"{"network": true}"#).unwrap();
        assert!(!old.host_network);
        let on: SandboxSettings = serde_json::from_str(r#"{"host_network": true}"#).unwrap();
        assert!(on.host_network);
        assert!(!sandbox.windows_appcontainer && !old.windows_appcontainer);
        let container: SandboxSettings =
            serde_json::from_str(r#"{"windows_appcontainer": true}"#).unwrap();
        assert!(container.windows_appcontainer);
        assert_eq!(sandbox.engines, EngineAccess::Passthrough);
        let sidecar: SandboxSettings =
            serde_json::from_str(r#"{"engines": "sidecar", "sidecar": {"engine": "docker"}}"#)
                .unwrap();
        assert_eq!(sidecar.engines, EngineAccess::Sidecar);
        assert_eq!(sidecar.sidecar.engine, SidecarEngine::Docker);
        for group in HOST_GROUPS {
            assert_eq!(sandbox.group_allowed(group), group.default, "{}", group.id);
        }
    }

    #[test]
    fn allowing_a_group_adds_and_removes_its_hosts() {
        let mut sandbox = SandboxSettings::default();
        let jvm = HOST_GROUPS.iter().find(|g| g.id == "jvm").unwrap();
        let count = sandbox.allowed_hosts.len();
        assert!(sandbox.allow_group(jvm, true));
        assert!(!sandbox.allow_group(jvm, true));
        assert!(sandbox.group_allowed(jvm));
        assert!(sandbox.allow_group(jvm, false));
        assert_eq!(sandbox.allowed_hosts.len(), count);
        // A partly allowed group counts as off.
        sandbox.allowed_hosts.push("repo1.maven.org".to_owned());
        assert!(!sandbox.group_allowed(jvm));
    }

    #[test]
    fn terminals_run_in_a_sandbox_unless_the_host_is_chosen() {
        let config: WorktreeConfig = serde_json::from_str(r#"{"path":"/a"}"#).unwrap();
        assert_eq!(config.runtime, Runtime::Sandbox);
        assert_eq!(WorktreeConfig::new("/a".into()).runtime, Runtime::Sandbox);

        let mut config = config;
        config.runtime = Runtime::Host;
        let json = serde_json::to_string(&config).unwrap();
        let back: WorktreeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime, Runtime::Host, "{json}");
    }

    #[test]
    fn worktree_configs_from_before_splits_give_one_tab_per_panel() {
        let config: WorktreeConfig = serde_json::from_str(
            r#"{"path":"/a","panels":[{"kind":"terminal"},{"kind":"terminal"}],"active_panel":1}"#,
        )
        .unwrap();
        let (tabs, active) = config.tabs();
        let layouts: Vec<_> = tabs.iter().map(|tab| tab.layout.clone()).collect();
        assert_eq!(layouts, [PaneLayout::pane(0), PaneLayout::pane(1)]);
        assert_eq!(active, 1);
    }

    #[test]
    fn shared_panes_become_one_tab_and_hidden_panels_their_own() {
        let config: WorktreeConfig = serde_json::from_str(
            r#"{"path":"/a","panels":[{"kind":"terminal"},{"kind":"terminal"},{"kind":"terminal"}],
                "active_panel":1,
                "split":{"axis":"side_by_side","secondary":2}}"#,
        )
        .unwrap();
        let (tabs, active) = config.tabs();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].layout, PaneLayout::pane(0));
        assert_eq!(
            (tabs[1].layout.get(0), tabs[1].layout.get(1), active),
            (Some(1), Some(2), 1)
        );
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("split"), "{json}");
    }

    #[test]
    fn adding_the_same_project_twice_keeps_one() {
        let mut state = AppState::default();
        let first = state.add_project(
            "a".into(),
            Environment::Local,
            "/r".into(),
            VcsKind::Git,
            None,
        );
        let second = state.add_project(
            "b".into(),
            Environment::Local,
            "/r".into(),
            VcsKind::Git,
            None,
        );
        assert_eq!(first, second);
        assert_eq!(state.projects.len(), 1);
    }

    #[test]
    fn temporary_projects_are_left_out_of_the_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path().join("state.json"));
        let mut state = AppState::default();
        let kept = state.add_project(
            "kept".into(),
            Environment::Local,
            "/kept".into(),
            VcsKind::Git,
            None,
        );
        let local = state.add_project(
            "local".into(),
            Environment::Local,
            "/local".into(),
            VcsKind::Git,
            None,
        );
        state.project_mut(local).unwrap().temporary = true;
        let global = state.add_global_session("home".into(), Environment::Local, "/home/u".into());
        state.active_project = Some(global);
        store.save(&state).unwrap();
        let back = store.load().unwrap();
        let ids: Vec<u64> = back.projects.iter().map(|p| p.id).collect();
        assert_eq!(ids, [kept]);
        assert_eq!(back.active_project, Some(kept));

        state.project_mut(global).unwrap().temporary = false;
        store.save(&state).unwrap();
        let back = store.load().unwrap();
        let home = back.project(global).unwrap();
        assert!(home.is_global() && !home.temporary);
        assert_eq!(
            home.worktree(Path::new("/home/u")).unwrap().runtime,
            Runtime::Host
        );
        assert_eq!(back.active_project, Some(global));
    }

    #[test]
    fn a_global_session_and_a_repository_in_one_folder_are_two_projects() {
        let mut state = AppState::default();
        let repo = state.add_project(
            "dotfiles".into(),
            Environment::Local,
            "/home/u".into(),
            VcsKind::Git,
            None,
        );
        let global = state.add_global_session("home".into(), Environment::Local, "/home/u".into());
        assert_ne!(repo, global);
        let again = state.add_global_session("home".into(), Environment::Local, "/home/u".into());
        assert_eq!(global, again);
        assert!(!state.project(repo).unwrap().temporary);
        assert!(state.project(global).unwrap().temporary);
    }

    #[test]
    fn a_worktree_keeps_its_last_agent_titles() {
        let mut config = WorktreeConfig::new("/w".into());
        assert_eq!(config.agent_title(), None);
        assert!(config.note_agent_title("Fix login"));
        assert!(!config.note_agent_title("Fix login"));
        assert!(config.note_agent_title("Add search"));
        // An older title seen again moves to the end instead of repeating.
        assert!(config.note_agent_title("Fix login"));
        assert_eq!(config.agent_titles, ["Add search", "Fix login"]);
        for n in 0..MAX_AGENT_TITLES {
            config.note_agent_title(&format!("task {n}"));
        }
        assert_eq!(config.agent_titles.len(), MAX_AGENT_TITLES);
        assert_eq!(config.agent_titles[0], "task 0");
        let json = serde_json::to_string(&config).unwrap();
        let back: WorktreeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_title(), Some("task 7"));
    }

    #[test]
    fn a_worktree_keeps_its_agent_sessions() {
        let mut config = WorktreeConfig::new("/w".into());
        let session = |id: Option<&str>| AgentSession {
            agent: "claude".into(),
            session: id.map(str::to_owned),
            working: false,
        };
        assert!(!config.note_agent_session(&session(None), Some("Fix login"), 1));
        assert!(config.note_agent_session(&session(Some("a")), None, 1));
        assert!(!config.note_agent_session(&session(Some("a")), None, 2));
        assert!(config.note_agent_session(&session(Some("b")), Some("Add search"), 3));
        // A new title moves the session to the end.
        assert!(config.note_agent_session(&session(Some("a")), Some("Fix login"), 4));
        assert!(!config.note_agent_session(&session(Some("a")), Some("Fix login"), 5));
        let ids: Vec<_> = config
            .agent_sessions
            .iter()
            .map(|r| r.session.as_str())
            .collect();
        assert_eq!(ids, ["b", "a"]);
        assert_eq!(config.agent_sessions[1].title.as_deref(), Some("Fix login"));
        assert_eq!(config.agent_sessions[1].seen, 4);
        assert!(config.agent_sessions[1].is(&session(Some("a"))));
        assert_eq!(config.agent_sessions[1].resume(), session(Some("a")));
        for n in 0..MAX_AGENT_SESSIONS {
            config.note_agent_session(&session(Some(&format!("s{n}"))), None, 9);
        }
        assert_eq!(config.agent_sessions.len(), MAX_AGENT_SESSIONS);
        assert_eq!(config.agent_sessions[0].session, "s0");
        let json = serde_json::to_string(&config).unwrap();
        let back: WorktreeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_sessions, config.agent_sessions);
    }

    #[test]
    fn removing_the_active_project_selects_another() {
        let mut state = AppState::default();
        let a = state.add_project(
            "a".into(),
            Environment::Local,
            "/a".into(),
            VcsKind::Git,
            None,
        );
        let b = state.add_project(
            "b".into(),
            Environment::Local,
            "/b".into(),
            VcsKind::Git,
            None,
        );
        state.active_project = Some(a);
        state.remove_project(a);
        assert_eq!(state.active_project, Some(b));
    }
}
