//! The command palette: a modal overlay opened with Ctrl+Shift+P, the command
//! button at the right of the tab strip or the rail's `+` button. It lists
//! actions with their key bindings, filters them as the user types and runs
//! the highlighted one on Enter. Three actions ask for one line of text first
//! (input modes). The project input has an Open button beside it that fills
//! the input from the native folder chooser. The `+` on a rail worktree row
//! opens a fourth mode with two inputs: a name and an optional branch. The
//! `+` after the last tab opens a short list of the panels that can be opened
//! instead of the actions. Both lists offer to start the coding agents whose
//! integration is installed on the active project's machine. A repository URL in the project input asks where
//! to clone it: this machine, a VM, a remote machine or a VM on one, the
//! first and remote machines also with their devcontainer. The overlay is
//! its own region; typing rebuilds only the result list.

use std::path::{Path, PathBuf};

use crate::state::{Runtime, SplitAxis, WorktreeTitles};
use cmdterm_env::{Environment, RemoteTarget, VmTarget};
use schnellui::layout::EdgeInsets;
use schnellui::scene::{Color, Point, Size};
use schnellui::widgets::{
    Align, Button, ButtonAppearance, Column, Dialog, DialogPosition, Justify, ListRow, Pad, Row,
    Text, TextInput, Theme, ThemeProvider, View,
};

use super::rail::branch_name;
use super::settings::SettingsSection;
use super::{Closed, Fill, Named, tail_path, truncate};
use crate::agents::Agent;
use crate::i18n::Strings;
use crate::keymap;
use crate::workspace::{FolderTarget, Inner, Workspace, clone_url};

/// Accessible name of the palette's text input. English in every language:
/// `build_app` finds the input by it.
pub const PALETTE_INPUT: &str = "Command palette";
/// Accessible name of the optional branch input of [`InputMode::WorktreeFrom`].
pub const PALETTE_BRANCH_INPUT: &str = "Worktree branch";
/// Accessible name of the field of the new panel menu.
pub const PALETTE_PANEL_INPUT: &str = "Find a panel";
/// Accessible name of the VM name input of [`InputMode::Clone`].
pub const PALETTE_VM_INPUT: &str = "Clone VM name";
/// Folder a clone goes into until the user types another.
const CLONE_FOLDER: &str = "~";
/// Width of the palette surface.
pub const PALETTE_WIDTH: f32 = 560.0;
/// Room a list row's dot takes before its title: the dot and its gap.
const DOT_ROOM: f32 = 16.0;
/// Rows the list shows at once on a window of ordinary height.
pub const MAX_ROWS: usize = 16;
/// Fewest rows the list shows, on a window too short for [`MAX_ROWS`].
const FEWEST_ROWS: usize = 4;
/// Most rows the list shows at once, on a tall window.
const MOST_ROWS: usize = 24;
/// Window height the palette leaves for things other than its rows: the
/// space above it, the field, the group headings, the footer and a margin.
const CHROME_HEIGHT: f32 = 300.0;

thread_local! {
    /// Rows the list shows at once, set from the window height each time
    /// the palette mounts, so the list fills a tall window.
    static ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(MAX_ROWS) };
}

/// Rows a window `height` tall has room for: [`MAX_ROWS`] up to
/// [`MOST_ROWS`] as it grows, and fewer on a short window, so the footer
/// and the key hints stay inside it.
fn rows_for(height: f32) -> usize {
    (((height - CHROME_HEIGHT) / ROW_HEIGHT).max(0.0) as usize).clamp(FEWEST_ROWS, MOST_ROWS)
}
const ROW_HEIGHT: f32 = 28.0;
/// Space above the palette, in window coordinates.
const TOP: f32 = 80.0;
const PAD: f32 = 8.0;
const OPEN_WIDTH: f32 = 96.0;
/// Height of the palette's text fields and the button beside them.
const FIELD_HEIGHT: f32 = 36.0;
const GAP: f32 = 4.0;

/// The line of text an input mode asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Project,
    /// A project opened as a local session, never saved.
    TemporaryProject,
    /// Where a global session opens; empty is this machine's home.
    GlobalSession,
    Worktree,
    /// A worktree at another worktree's revision; the source is
    /// [`PaletteState::source`].
    WorktreeFrom,
    Url,
    /// A page to load in the debug browser.
    DebugUrl,
    /// Where to clone [`PaletteState::clone_url`]; the text is the folder.
    Clone,
    /// The base branch to review the worktree's changes against; empty
    /// detects it.
    ReviewBase,
}

/// Whether git takes `name` as a branch name; empty is fine, as the
/// confirm button waits for text anyway. The rules of
/// `git check-ref-format --branch` that a typed name breaks.
fn branch_name_ok(name: &str) -> bool {
    name.is_empty()
        || !(name.starts_with(['-', '/', '.'])
            || name.ends_with(['/', '.'])
            || name.ends_with(".lock")
            || name.contains("..")
            || name.contains("//")
            || name.contains("@{")
            || name.contains("/.")
            || name
                .contains(|c: char| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
}

impl InputMode {
    /// Every mode, for registering the key handler of each mode's field.
    pub const ALL: [InputMode; 9] = [
        InputMode::Project,
        InputMode::TemporaryProject,
        InputMode::GlobalSession,
        InputMode::Worktree,
        InputMode::WorktreeFrom,
        InputMode::Url,
        InputMode::DebugUrl,
        InputMode::Clone,
        InputMode::ReviewBase,
    ];

    /// Accessible name of the palette's field in this mode, so a screen
    /// reader hears what the line asks for instead of "Command palette".
    /// English in every language, as the key handlers find it by name.
    pub fn input_name(self) -> &'static str {
        match self {
            InputMode::Project => "Project folder",
            InputMode::TemporaryProject => "Temporary project folder",
            InputMode::GlobalSession => "Session folder",
            InputMode::Worktree | InputMode::WorktreeFrom => "Worktree name",
            InputMode::Url => "Page address",
            InputMode::DebugUrl => "Debug browser address",
            InputMode::Clone => "Clone folder",
            InputMode::ReviewBase => "Review base branch",
        }
    }

    /// Whether the input has the Open button of the native folder chooser.
    pub fn takes_folder(self) -> bool {
        matches!(self, InputMode::Project | InputMode::TemporaryProject)
    }

    /// The command that asked for the line, shown as the palette's title
    /// while it waits for it.
    pub fn title(self, tr: &Strings) -> &'static str {
        let ask = match self {
            InputMode::Project => tr.open_project_ask,
            InputMode::TemporaryProject => tr.open_temporary_ask,
            InputMode::GlobalSession => tr.global_session_ask,
            InputMode::Worktree | InputMode::WorktreeFrom => tr.new_worktree_ask,
            InputMode::Url => tr.open_url_ask,
            InputMode::DebugUrl => tr.debug_browser_url_ask,
            InputMode::Clone => tr.prompt_clone_folder,
            InputMode::ReviewBase => tr.explore_review_against,
        };
        ask.trim_end_matches('…')
    }

    /// What is wrong with `text` as this mode's line, said under the field
    /// while it is typed, or None when Enter may take it.
    pub fn problem(self, text: &str, tr: &Strings) -> Option<&'static str> {
        let text = text.trim();
        match self {
            InputMode::Worktree if !branch_name_ok(text) => Some(tr.bad_branch_name),
            InputMode::WorktreeFrom
                if text.contains(['/', '\\']) || text == "." || text == ".." =>
            {
                Some(tr.bad_folder_name)
            }
            _ => None,
        }
    }

    /// Whether the line must have text before Enter or the confirm button
    /// does anything; the others treat an empty line as a choice.
    pub fn needs_text(self) -> bool {
        !matches!(
            self,
            InputMode::GlobalSession | InputMode::ReviewBase | InputMode::Clone
        )
    }

    /// The label of the button that confirms the line, as Enter does.
    pub fn confirm(self, tr: &Strings) -> &'static str {
        match self {
            InputMode::Worktree | InputMode::WorktreeFrom => tr.palette_create,
            InputMode::Clone => tr.palette_clone,
            InputMode::ReviewBase => tr.explore_review,
            InputMode::Project
            | InputMode::TemporaryProject
            | InputMode::GlobalSession
            | InputMode::Url
            | InputMode::DebugUrl => tr.open,
        }
    }

    /// The prompt shown in the empty input.
    pub fn prompt(self, tr: &Strings) -> &'static str {
        match self {
            InputMode::Project | InputMode::TemporaryProject => tr.prompt_project,
            InputMode::GlobalSession => tr.prompt_global_session,
            InputMode::Worktree => tr.prompt_branch,
            InputMode::WorktreeFrom => tr.prompt_worktree,
            InputMode::Url | InputMode::DebugUrl => tr.prompt_url,
            InputMode::Clone => tr.prompt_clone_folder,
            InputMode::ReviewBase => tr.prompt_review_base,
        }
    }
}

/// What the palette is doing.
#[derive(Default)]
pub struct PaletteState {
    pub open: bool,
    /// `Some` while the palette asks for a line of text instead of listing actions.
    pub input: Option<InputMode>,
    /// The filter text, or the text of the input mode.
    pub query: String,
    /// Index into the filtered rows.
    pub highlight: usize,
    /// Project and worktree path that [`InputMode::WorktreeFrom`] starts from.
    pub source: Option<(u64, PathBuf)>,
    /// The branch input of [`InputMode::WorktreeFrom`]; empty means detached.
    pub branch: String,
    /// Lists the panels to open instead of every action. Set by the `+`
    /// after the last tab; kept while an input mode asks for a URL, so
    /// Escape returns to this list.
    pub new_panel: bool,
    /// Opened by a click on the `+`, so Escape gives focus back to it.
    pub from_add: bool,
    /// The repository URL [`InputMode::Clone`] clones.
    pub clone_url: String,
    /// Where [`InputMode::Clone`] can clone to; `highlight` picks one.
    pub clone_targets: Vec<CloneTarget>,
    /// The VM name input of [`InputMode::Clone`], for [`CloneTarget::MachineVm`].
    pub vm: String,
    /// A row whose action cannot be undone, after its first Enter or click:
    /// the second one runs it.
    pub armed: Option<usize>,
}

/// Where a cloned repository goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneTarget {
    /// This machine, with terminals on the host or in the devcontainer.
    Here { devcontainer: bool },
    /// A VM of this machine, by name.
    Vm(String),
    /// A trusted remote machine, by address.
    Machine { address: String, devcontainer: bool },
    /// A VM on a trusted remote machine; the name comes from [`PaletteState::vm`].
    MachineVm(String),
}

impl CloneTarget {
    pub fn label(&self, tr: &Strings) -> String {
        match self {
            CloneTarget::Here {
                devcontainer: false,
            } => tr.clone_here.to_owned(),
            CloneTarget::Here { devcontainer: true } => tr.clone_here_devcontainer.to_owned(),
            CloneTarget::Vm(name) => (tr.clone_in_vm)(name),
            CloneTarget::Machine {
                address,
                devcontainer: false,
            } => (tr.clone_on)(address),
            CloneTarget::Machine {
                address,
                devcontainer: true,
            } => (tr.clone_devcontainer_on)(address),
            CloneTarget::MachineVm(address) => (tr.clone_vm_on)(address),
        }
    }
}

/// What a palette row does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Switch {
        project: u64,
        path: PathBuf,
    },
    Ask(InputMode),
    NewShell,
    /// Opens a terminal with one of the shells of [`Settings::shells`].
    ///
    /// [`Settings::shells`]: crate::state::Settings::shells
    NewShellWith {
        name: String,
        argv: Vec<String>,
    },
    /// Starts a coding agent in a new terminal.
    StartAgent(Agent),
    Files,
    Explore,
    Review,
    /// The change request page of the explore panel.
    Change,
    Monitor,
    Tasks,
    DebugBrowser,
    Split(SplitAxis),
    ClosePane,
    NextPane,
    PreviousPane,
    GrowPane,
    ShrinkPane,
    NextTab,
    PreviousTab,
    MovePaneToTab,
    CloseTab,
    RemoveWorktree {
        project: u64,
        path: PathBuf,
    },
    ForgetProject(u64),
    /// Saves a local or global session from now on.
    KeepProject(u64),
    Settings,
    /// Opens the settings on the active project's page.
    ProjectSettings,
    /// Opens the settings on one page.
    SettingsPage(SettingsSection),
    /// Focuses the rail's search field.
    SearchWorktrees,
    /// Moves focus to the shown tab in the tab strip.
    FocusTabs,
    FocusBack,
    /// Hides the rail, or shows it again.
    ToggleSidebar,
    /// Changes what the rail names worktrees after.
    WorktreeTitles(WorktreeTitles),
    /// Switches the CPU mode of the terminals at a place.
    CpuMode(crate::cpu::Place, crate::state::CpuMode),
    /// Turns Jujutsu on or off for every project without its own choice.
    Jj(bool),
    /// Turns Jujutsu on or off for one project.
    ProjectJj {
        project: u64,
        on: bool,
    },
    Command(String),
    /// Runs an extension task now, by id.
    RunTask(String),
    SetRuntime(Runtime),
    RebuildDevcontainer,
    /// Forwards a port of the active worktree's sandbox to the host.
    #[cfg_attr(not(unix), allow(dead_code))]
    ExposePort(u16),
    /// Stops forwarding it.
    #[cfg_attr(not(unix), allow(dead_code))]
    UnexposePort(u16),
}

impl Action {
    /// Whether the action closes or removes something, so a typed query
    /// ranks it after safe actions that match as well.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Action::RemoveWorktree { .. }
                | Action::ForgetProject(_)
                | Action::ClosePane
                | Action::CloseTab
        )
    }

    /// Whether the action deletes something a user cannot get back from the
    /// app, so the palette asks for a second Enter first.
    pub fn needs_confirm(&self) -> bool {
        matches!(
            self,
            Action::RemoveWorktree { .. } | Action::ForgetProject(_)
        )
    }
}

/// One row of the palette.
#[derive(Clone, Debug)]
pub struct Entry {
    pub label: String,
    /// The heading the entry stands under while nothing is typed.
    pub group: Group,
    /// Lane color for the dot of worktree rows.
    pub lane: Option<Color>,
    pub action: Action,
}

/// The groups of the palette's list, in the order they come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// The new panel list's shells.
    Shells,
    Project,
    Agents,
    Panels,
    Layout,
    App,
    Worktrees,
    Runtime,
    Extensions,
}

impl Group {
    fn title(self, tr: &Strings) -> &'static str {
        match self {
            Group::Shells => tr.palette_group_shells,
            Group::Project => tr.palette_group_project,
            Group::Agents => tr.palette_group_agents,
            Group::Panels => tr.palette_group_panels,
            Group::Layout => tr.palette_group_layout,
            Group::App => tr.palette_group_app,
            Group::Worktrees => tr.palette_group_worktrees,
            Group::Runtime => tr.palette_group_runtime,
            Group::Extensions => tr.palette_group_extensions,
        }
    }
}

/// Puts the entries from `from` on into `group`.
fn group(out: &mut [Entry], from: usize, group: Group) {
    for entry in &mut out[from..] {
        entry.group = group;
    }
}

fn plain(label: &str, action: Action) -> Entry {
    Entry {
        group: Group::Project,
        label: label.to_owned(),
        lane: None,
        action,
    }
}

/// New shell entries for the shells of [`Settings::shells`] that resolve,
/// where a terminal starts them on this machine: in a local project that
/// does not run in a devcontainer.
///
/// [`Settings::shells`]: crate::state::Settings::shells
fn other_shells(inner: &Inner) -> Vec<Entry> {
    let here = inner
        .state
        .active()
        .is_some_and(|project| project.env.is_local())
        && inner.active_runtime() != Runtime::Devcontainer;
    if !here {
        return Vec::new();
    }
    (inner.settings.shells.iter())
        .filter_map(|choice| choice.resolve(&inner.shells))
        .map(|(name, argv)| {
            let argv = argv
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            plain(
                &(inner.tr.new_shell_named)(&name),
                Action::NewShellWith { name, argv },
            )
        })
        .collect()
}

/// Every action available right now, in display order. In the new panel
/// list only the panels that can be opened.
pub fn entries(inner: &Inner) -> Vec<Entry> {
    let scheme = inner.scheme;
    let tr = inner.tr;
    let agents = inner
        .state
        .active()
        .map(|project| inner.settings_ui.agents.configured(&project.env))
        .unwrap_or_default();
    let start_agents = agents
        .into_iter()
        .map(|agent| plain(&(tr.start_agent)(agent.name()), Action::StartAgent(agent)));
    if inner.palette.new_panel {
        // Headed like the palette's own list: shells, agents, then panels.
        let mut out = vec![plain(tr.new_shell, Action::NewShell)];
        out.extend(other_shells(inner));
        group(&mut out, 0, Group::Shells);
        let agents_from = out.len();
        out.extend(start_agents);
        group(&mut out, agents_from, Group::Agents);
        let panels_from = out.len();
        out.extend([
            plain(tr.browse_files, Action::Files),
            plain(tr.explore_open_history, Action::Explore),
            plain(tr.explore_open_review, Action::Review),
            plain(tr.explore_open_change, Action::Change),
            plain(tr.monitor_open, Action::Monitor),
            plain(tr.tasks_open, Action::Tasks),
            plain(tr.debug_browser_open, Action::DebugBrowser),
            plain(tr.open_url_ask, Action::Ask(InputMode::Url)),
            plain(tr.debug_browser_url_ask, Action::Ask(InputMode::DebugUrl)),
        ]);
        group(&mut out, panels_from, Group::Panels);
        return out;
    }
    let mut out = vec![
        plain(tr.open_project_ask, Action::Ask(InputMode::Project)),
        plain(
            tr.open_temporary_ask,
            Action::Ask(InputMode::TemporaryProject),
        ),
        plain(tr.global_session_ask, Action::Ask(InputMode::GlobalSession)),
        plain(tr.new_worktree_ask, Action::Ask(InputMode::Worktree)),
        plain(tr.open_url_ask, Action::Ask(InputMode::Url)),
    ];
    // Agents get their own group, so the agents a project uses are one
    // glance away from the project commands.
    let agents = out.len();
    out.extend(start_agents);
    group(&mut out, agents, Group::Agents);
    let panels = out.len();
    out.push(plain(tr.new_shell, Action::NewShell));
    out.extend(other_shells(inner));
    out.extend([
        plain(tr.browse_files, Action::Files),
        plain(tr.explore_open_history, Action::Explore),
        plain(tr.explore_open_review, Action::Review),
        plain(tr.explore_open_change, Action::Change),
        plain(tr.monitor_open, Action::Monitor),
        plain(tr.tasks_open, Action::Tasks),
        plain(tr.debug_browser_open, Action::DebugBrowser),
        plain(tr.debug_browser_url_ask, Action::Ask(InputMode::DebugUrl)),
        plain(
            tr.explore_review_against,
            Action::Ask(InputMode::ReviewBase),
        ),
    ]);
    group(&mut out, panels, Group::Panels);
    let layout = out.len();
    out.extend([
        plain(tr.split_side_by_side, Action::Split(SplitAxis::SideBySide)),
        plain(tr.split_stacked, Action::Split(SplitAxis::Stacked)),
        plain(tr.focus_next_pane, Action::NextPane),
        plain(tr.focus_previous_pane, Action::PreviousPane),
        plain(tr.grow_pane, Action::GrowPane),
        plain(tr.shrink_pane, Action::ShrinkPane),
        plain(tr.next_tab, Action::NextTab),
        plain(tr.previous_tab, Action::PreviousTab),
        plain(tr.close_pane, Action::ClosePane),
        plain(tr.move_pane_to_tab, Action::MovePaneToTab),
        plain(tr.close_tab, Action::CloseTab),
    ]);
    group(&mut out, layout, Group::Layout);
    let app = out.len();
    out.extend([
        plain(tr.settings, Action::Settings),
        plain(tr.project_settings, Action::ProjectSettings),
        plain(tr.search_worktrees, Action::SearchWorktrees),
        plain(tr.focus_tabs, Action::FocusTabs),
        plain(tr.focus_back, Action::FocusBack),
        plain(tr.toggle_sidebar, Action::ToggleSidebar),
    ]);
    // Every settings page, while the user types, so a page is found by
    // name without walking the settings' nav. The idle list stays short.
    if !inner.palette.query.trim().is_empty() {
        let no_shells = inner.shells.is_empty();
        out.extend(
            SettingsSection::ALL
                .into_iter()
                .filter(|page| !(no_shells && *page == SettingsSection::Shells))
                .map(|page| {
                    plain(
                        &(tr.settings_page)(page.title(tr)),
                        Action::SettingsPage(page),
                    )
                }),
        );
    }
    out.push(match inner.settings.worktree_titles {
        WorktreeTitles::Agent => plain(
            tr.title_worktrees_by_branch,
            Action::WorktreeTitles(WorktreeTitles::Branch),
        ),
        WorktreeTitles::Branch => plain(
            tr.title_worktrees_by_agent,
            Action::WorktreeTitles(WorktreeTitles::Agent),
        ),
    });
    // The modes each place is not in.
    for (place, label) in [
        (crate::cpu::Place::Local, tr.cpu_mode_local),
        (crate::cpu::Place::Remote, tr.cpu_mode_remote),
    ] {
        let modes = &inner.settings.cpu_mode;
        let current = modes.of(place);
        for mode in crate::state::CpuMode::ALL {
            if mode != current {
                let text = label(&modes.describe(mode, tr));
                out.push(plain(&text, Action::CpuMode(place, mode)));
            }
        }
    }
    let jj = inner.settings.jj;
    out.push(if jj {
        plain(tr.jj_off, Action::Jj(false))
    } else {
        plain(tr.jj_on, Action::Jj(true))
    });
    if let Some(project) = inner.state.active().filter(|project| !project.is_global()) {
        let on = !project.uses_jj(jj);
        let label = if on {
            tr.project_jj_on
        } else {
            tr.project_jj_off
        };
        out.push(plain(
            label,
            Action::ProjectJj {
                project: project.id,
                on,
            },
        ));
    }
    group(&mut out, app, Group::App);
    let worktrees = out.len();
    for project in &inner.state.projects {
        let Some(snapshot) = inner.snapshots.get(&project.id) else {
            out.push(Entry {
                group: Group::Worktrees,
                label: (tr.switch_to)(&project.name),
                lane: Some(scheme.lane(0)),
                action: Action::Switch {
                    project: project.id,
                    path: project.root.clone(),
                },
            });
            continue;
        };
        for (index, worktree) in snapshot.worktrees.iter().enumerate() {
            if worktree.bare {
                continue;
            }
            out.push(Entry {
                group: Group::Worktrees,
                label: (tr.switch_to_worktree)(&project.name, &branch_name(worktree)),
                lane: Some(scheme.lane(index)),
                action: Action::Switch {
                    project: project.id,
                    path: worktree.path.clone(),
                },
            });
        }
    }
    if let Some(project) = inner.state.active()
        && let Some(snapshot) = inner.snapshots.get(&project.id)
    {
        for worktree in snapshot.worktrees.iter().filter(|w| !w.is_main && !w.bare) {
            out.push(plain(
                &(tr.remove_worktree)(&branch_name(worktree)),
                Action::RemoveWorktree {
                    project: project.id,
                    path: worktree.path.clone(),
                },
            ));
        }
    }
    group(&mut out, worktrees, Group::Worktrees);
    let runtime = out.len();
    if inner.state.active().is_some() {
        let current = inner.active_runtime();
        for (runtime, label) in [
            (Runtime::Host, tr.run_on_host),
            (Runtime::Devcontainer, tr.run_in_devcontainer),
            (Runtime::Sandbox, tr.run_in_sandbox),
        ] {
            if runtime != current {
                out.push(plain(label, Action::SetRuntime(runtime)));
            }
        }
        if current == Runtime::Devcontainer {
            out.push(plain(tr.rebuild_devcontainer, Action::RebuildDevcontainer));
        }
    }
    #[cfg(unix)]
    if let Some((id, path)) = inner.active_key()
        && let Some(hub) = inner.runtimes.ports.get(id, &path)
    {
        for port in hub.ports() {
            out.push(match port.exposed {
                Some(_) => plain(
                    &(tr.stop_exposing_port)(port.port),
                    Action::UnexposePort(port.port),
                ),
                None => plain(&(tr.expose_port)(port.port), Action::ExposePort(port.port)),
            });
        }
    }
    if let Some(project) = inner.state.active() {
        if project.temporary {
            out.push(plain(
                &(tr.keep_session)(&project.name),
                Action::KeepProject(project.id),
            ));
        }
        let forget = if project.temporary {
            tr.close_session
        } else {
            tr.forget_project
        };
        out.push(plain(
            &forget(&project.name),
            Action::ForgetProject(project.id),
        ));
    }
    group(&mut out, runtime, Group::Runtime);
    let extensions = out.len();
    for command in inner.ext.commands() {
        out.push(plain(&command.title, Action::Command(command.name.clone())));
    }
    for task in inner.ext.tasks() {
        out.push(plain(
            &(tr.run_task)(&task.title),
            Action::RunTask(task.id.clone()),
        ));
    }
    group(&mut out, extensions, Group::Extensions);
    out
}

/// Indices into `labels` that match `query`, best first, at most `limit`.
///
/// A label matches when the query's characters appear in it in order,
/// ignoring case. Matches rank by how well the query fits: the label's start,
/// then the start of a word, then anywhere as one run, then scattered. Within
/// one fit, labels flagged in `demoted` (destructive actions) come after the
/// others, then earlier matches, then shorter labels, then original order. An
/// empty query matches everything in the original order.
#[cfg(test)]
pub fn filter(labels: &[&str], query: &str, limit: usize) -> Vec<usize> {
    filter_ranked(labels, &[], query, limit)
}

/// [`filter`] with `demoted[i]` set for labels that rank after the others
/// of the same fit. Missing flags count as not demoted.
pub fn filter_ranked(labels: &[&str], demoted: &[bool], query: &str, limit: usize) -> Vec<usize> {
    let wanted: Vec<char> = query.trim().to_lowercase().chars().collect();
    let mut hits: Vec<(u8, bool, usize, usize, usize)> = labels
        .iter()
        .enumerate()
        .filter_map(|(index, label)| {
            let (fit, start, len) = fit(label, &wanted)?;
            let demote = !wanted.is_empty() && demoted.get(index).copied().unwrap_or(false);
            Some((fit, demote, start, len, index))
        })
        .collect();
    hits.sort();
    hits.into_iter().take(limit).map(|(.., i)| i).collect()
}

/// How well `wanted` fits `label`: 0 at its start, 1 at the start of a
/// word, 2 as one run elsewhere, 3 scattered in order. With the start of
/// the match and the label length; `None` when it does not match.
fn fit(label: &str, wanted: &[char]) -> Option<(u8, usize, usize)> {
    let lower: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let len = lower.len();
    if wanted.is_empty() {
        return Some((0, 0, 0));
    }
    let mut run: Option<(u8, usize)> = None;
    if wanted.len() <= len {
        for start in 0..=len - wanted.len() {
            if lower[start..start + wanted.len()] != *wanted {
                continue;
            }
            let word = start == 0 || !lower[start - 1].is_alphanumeric();
            let rank = match (start, word) {
                (0, _) => 0,
                (_, true) => 1,
                _ => 2,
            };
            if run.is_none_or(|(best, _)| rank < best) {
                run = Some((rank, start));
            }
            if rank <= 1 {
                break;
            }
        }
    }
    if let Some((rank, start)) = run {
        return Some((rank, start, len));
    }
    let (start, len) = subsequence(label, wanted)?;
    Some((3, start, len))
}

/// Start of the leftmost tight in-order match of `wanted` and the label
/// length. Tight means the letters span at most three times the query
/// (and at least four more than it), or a letter starts a word: "pane"
/// finds "pa_ne", "sps" finds "Split pane side", and neither finds one
/// letter inside each of four words of a long label.
fn subsequence(label: &str, wanted: &[char]) -> Option<(usize, usize)> {
    let lower: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let len = lower.len();
    if wanted.is_empty() {
        return Some((0, 0));
    }
    let widest = (wanted.len() * 3).max(wanted.len() + 4);
    for start in 0..len {
        if lower[start] != wanted[0] {
            continue;
        }
        let mut next = 1;
        let mut at = start + 1;
        while next < wanted.len() && at < len {
            let word = !lower[at - 1].is_alphanumeric();
            if lower[at] == wanted[next] && (at - start < widest || word) {
                next += 1;
            }
            at += 1;
        }
        if next == wanted.len() {
            return Some((start, len));
        }
    }
    None
}

/// The keymap action a palette action runs the same way, for its key hint.
fn bound(action: &Action) -> Option<keymap::Action> {
    Some(match action {
        Action::NewShell => keymap::Action::NewTerminal,
        Action::Files => keymap::Action::Files,
        Action::Explore => keymap::Action::Explore,
        Action::Review => keymap::Action::Review,
        Action::Monitor => keymap::Action::Monitor,
        Action::Tasks => keymap::Action::Tasks,
        Action::DebugBrowser => keymap::Action::DebugBrowser,
        Action::Split(axis) => keymap::Action::Split(*axis),
        Action::ClosePane => keymap::Action::ClosePane,
        Action::NextPane => keymap::Action::NextPane,
        Action::PreviousPane => keymap::Action::PreviousPane,
        Action::GrowPane => keymap::Action::GrowPane,
        Action::ShrinkPane => keymap::Action::ShrinkPane,
        Action::NextTab => keymap::Action::NextTab,
        Action::PreviousTab => keymap::Action::PreviousTab,
        Action::MovePaneToTab => keymap::Action::MovePaneToTab,
        Action::CloseTab => keymap::Action::CloseTab,
        Action::Settings => keymap::Action::Settings,
        Action::SearchWorktrees => keymap::Action::SearchWorktrees,
        Action::FocusTabs => keymap::Action::FocusTabs,
        Action::FocusBack => keymap::Action::FocusBack,
        Action::ToggleSidebar => keymap::Action::ToggleSidebar,
        Action::Command(name) => keymap::Action::Command(name.clone()),
        _ => return None,
    })
}

/// The entries that match the filter. The list shows [`MAX_ROWS`] of them
/// at a time, scrolled to keep the highlight in view.
pub fn visible(inner: &Inner) -> Vec<Entry> {
    let all = entries(inner);
    let labels: Vec<&str> = all.iter().map(|e| e.label.as_str()).collect();
    let demoted: Vec<bool> = all.iter().map(|e| e.action.is_destructive()).collect();
    filter_ranked(&labels, &demoted, &inner.palette.query, usize::MAX)
        .into_iter()
        .map(|i| all[i].clone())
        .collect()
}

impl Workspace {
    /// Opens the palette, as a list or straight in an input mode.
    pub fn open_palette(&self, input: Option<InputMode>) {
        self.show_palette(input, false);
    }

    /// Opens the palette listing the panels that can be opened: the `+`
    /// after the last tab.
    pub fn open_new_panel(&self) {
        if self.borrow().active_key().is_some() {
            self.show_palette(None, true);
        }
    }

    /// The new panel list, opened from the tab strip's `+`: Escape gives
    /// focus back to the `+`, as a menu does to its button.
    pub fn open_new_panel_from_add(&self) {
        self.open_new_panel();
        let mut inner = self.borrow_mut();
        if inner.palette.open {
            inner.palette.from_add = true;
        }
    }

    /// Closes the palette on Escape or a click outside it. Focus goes back
    /// to the `+` that opened it, or else to the panel.
    pub fn dismiss_palette(&self) {
        let from_add = self.borrow().palette.from_add;
        self.close_palette();
        if from_add {
            self.borrow_mut().dirty.new_panel = true;
        }
    }

    /// The new panel list with its first agent highlighted, for the Agents
    /// tab's start button: Enter starts the agent a project uses first.
    pub fn open_agent_list(&self) {
        if self.borrow().active_key().is_none() {
            return;
        }
        self.show_palette(None, true);
        let mut inner = self.borrow_mut();
        if let Some(index) = visible(&inner)
            .iter()
            .position(|entry| matches!(entry.action, Action::StartAgent(_)))
        {
            inner.palette.highlight = index;
        }
    }

    fn show_palette(&self, input: Option<InputMode>, new_panel: bool) {
        self.probe_agents();
        let mut inner = self.borrow_mut();
        inner.palette = PaletteState {
            open: true,
            input,
            new_panel,
            ..PaletteState::default()
        };
        inner.dirty.palette = true;
    }

    /// Checks which coding agents the active project's machine has an
    /// integration for, once per machine, so the lists can offer to start
    /// them. The rows appear when the check is done.
    fn probe_agents(&self) {
        if cfg!(test) {
            return;
        }
        let Some(env) = self.borrow().state.active().map(|p| p.env.clone()) else {
            return;
        };
        let setup = self.borrow().settings_ui.agents.clone();
        setup.probe(&env, self.redraw_handle());
    }

    /// Opens the palette asking for the name and optional branch of a new
    /// worktree at the revision of the worktree `path` of `project`.
    pub fn open_new_worktree_from(&self, project: u64, path: &Path) {
        let mut inner = self.borrow_mut();
        inner.palette = PaletteState {
            open: true,
            input: Some(InputMode::WorktreeFrom),
            source: Some((project, path.to_path_buf())),
            ..PaletteState::default()
        };
        inner.dirty.palette = true;
    }

    /// Replaces the branch text of [`InputMode::WorktreeFrom`].
    pub fn set_palette_branch(&self, value: &str) {
        let mut inner = self.borrow_mut();
        if inner.palette.branch != value {
            inner.palette.branch.clear();
            inner.palette.branch.push_str(value);
        }
    }

    pub fn close_palette(&self) {
        let mut inner = self.borrow_mut();
        if inner.palette.open {
            inner.palette = PaletteState::default();
            inner.dirty.palette = true;
        }
    }

    pub fn palette_is_open(&self) -> bool {
        self.borrow().palette.open
    }

    /// Replaces the filter text (or the input mode's text).
    pub fn set_palette_query(&self, value: &str) {
        let mut inner = self.borrow_mut();
        if inner.palette.query == value {
            return;
        }
        let was_empty = inner.palette.query.trim().is_empty();
        let tr = inner.tr;
        let was_wrong = inner
            .palette
            .input
            .and_then(|mode| mode.problem(&inner.palette.query, tr));
        inner.palette.query.clear();
        inner.palette.query.push_str(value);
        inner.palette.highlight = 0;
        inner.palette.armed = None;
        match inner.palette.input {
            None => inner.dirty.palette_list = true,
            // The confirm button turns on with the first character and off
            // with the last; other keystrokes leave the overlay alone.
            Some(mode) if mode.needs_text() && was_empty != value.trim().is_empty() => {
                inner.dirty.palette = true;
            }
            // A problem shows under the field the moment it starts or ends.
            Some(mode) if was_wrong != mode.problem(value, tr) => {
                inner.dirty.palette = true;
            }
            Some(_) => {}
        }
    }

    /// Moves the highlight by `delta` rows, wrapping around.
    pub fn move_palette_highlight(&self, delta: isize) {
        let count = {
            let inner = self.borrow();
            if inner.palette.input == Some(InputMode::Clone) {
                inner.palette.clone_targets.len()
            } else {
                visible(&inner).len()
            }
        };
        if count == 0 {
            return;
        }
        let highlight = self.borrow().palette.highlight as isize + delta;
        // A step wraps around the ends; a page stops at them.
        let index = if delta.abs() == 1 {
            highlight.rem_euclid(count as isize)
        } else {
            highlight.clamp(0, count as isize - 1)
        };
        self.set_palette_highlight(index as usize);
    }

    /// Highlights row `index`. In [`InputMode::Clone`] the VM name input
    /// shows only for a VM on a remote machine, so crossing that row
    /// rebuilds the whole palette.
    fn set_palette_highlight(&self, index: usize) {
        let mut inner = self.borrow_mut();
        let palette = &mut inner.palette;
        let asks_vm = |palette: &PaletteState| {
            matches!(
                palette.clone_targets.get(palette.highlight),
                Some(CloneTarget::MachineVm(_))
            )
        };
        let before = asks_vm(palette);
        palette.highlight = index;
        if palette.armed != Some(index) {
            palette.armed = None;
        }
        if asks_vm(palette) != before {
            inner.dirty.palette = true;
        } else {
            inner.dirty.palette_list = true;
        }
    }

    /// Replaces the VM name text of [`InputMode::Clone`].
    pub fn set_palette_vm(&self, value: &str) {
        let mut inner = self.borrow_mut();
        if inner.palette.vm != value {
            inner.palette.vm.clear();
            inner.palette.vm.push_str(value);
        }
    }

    /// Asks where to clone `url`.
    fn ask_clone_target(&self, url: &str) {
        let targets = self.clone_targets();
        let mut inner = self.borrow_mut();
        inner.palette = PaletteState {
            open: true,
            input: Some(InputMode::Clone),
            query: CLONE_FOLDER.to_owned(),
            clone_url: url.to_owned(),
            clone_targets: targets,
            ..PaletteState::default()
        };
        inner.dirty.palette = true;
    }

    /// Clones into the folder typed in [`InputMode::Clone`] on the
    /// highlighted target.
    fn palette_clone(&self) {
        let (url, folder, target, vm) = {
            let palette = &self.borrow().palette;
            (
                palette.clone_url.clone(),
                palette.query.trim().to_owned(),
                palette.clone_targets.get(palette.highlight).cloned(),
                palette.vm.trim().to_owned(),
            )
        };
        let Some(target) = target else { return };
        if folder.is_empty() {
            return;
        }
        let (env, devcontainer) = match target {
            CloneTarget::Here { devcontainer } => (Environment::Local, devcontainer),
            CloneTarget::Vm(name) => (Environment::Vm(VmTarget { name }), false),
            CloneTarget::Machine {
                address,
                devcontainer,
            } => (
                Environment::Remote(RemoteTarget::new(&address)),
                devcontainer,
            ),
            CloneTarget::MachineVm(address) => {
                if !cmdterm_remote::vm::valid_name(&vm) {
                    self.notify(self.tr().bad_vm_name);
                    return;
                }
                let mut target = RemoteTarget::new(&address);
                target.vm = Some(vm);
                (Environment::Remote(target), false)
            }
        };
        self.close_palette();
        self.clone_project(&url, env, PathBuf::from(folder), devcontainer);
    }

    /// Enter: runs the highlighted row, or submits the input mode's text.
    pub fn palette_enter(&self) {
        let (mode, query, highlight, source, branch) = {
            let inner = self.borrow();
            (
                inner.palette.input,
                inner.palette.query.trim().to_owned(),
                inner.palette.highlight,
                inner.palette.source.clone(),
                inner.palette.branch.clone(),
            )
        };
        match mode {
            Some(InputMode::Clone) => self.palette_clone(),
            Some(InputMode::Project) if clone_url(&query).is_some() => {
                self.ask_clone_target(&query);
            }
            Some(InputMode::ReviewBase) => {
                self.close_palette();
                self.review_against(&query);
            }
            Some(InputMode::GlobalSession) => {
                self.close_palette();
                self.open_global_session(&query);
            }
            Some(mode) => {
                if query.is_empty() || mode.problem(&query, self.tr()).is_some() {
                    return;
                }
                self.close_palette();
                match mode {
                    InputMode::Project => self.add_project(&query),
                    InputMode::TemporaryProject => self.add_temporary_project(&query),
                    InputMode::Worktree => self.add_worktree(&query),
                    InputMode::WorktreeFrom => {
                        if let Some((project, path)) = source {
                            self.add_worktree_from(project, &path, &query, &branch);
                        }
                    }
                    InputMode::Url => self.open_browser(&query),
                    InputMode::DebugUrl => self.open_in_debug_browser(&query),
                    InputMode::Clone | InputMode::ReviewBase | InputMode::GlobalSession => {}
                }
            }
            None => self.palette_run(highlight),
        }
    }

    /// Runs the row at `index` of the filtered list.
    /// In [`InputMode::Clone`] it picks the place to clone to instead.
    pub fn palette_run(&self, index: usize) {
        if self.borrow().palette.input == Some(InputMode::Clone) {
            self.set_palette_highlight(index);
            return;
        }
        let Some(entry) = visible(&self.borrow()).into_iter().nth(index) else {
            return;
        };
        if let Action::Ask(mode) = entry.action {
            let new_panel = self.borrow().palette.new_panel;
            self.show_palette(Some(mode), new_panel);
            return;
        }
        if entry.action.needs_confirm() && self.borrow().palette.armed != Some(index) {
            let mut inner = self.borrow_mut();
            inner.palette.highlight = index;
            inner.palette.armed = Some(index);
            inner.dirty.palette_list = true;
            return;
        }
        // The new panel list opens a new panel; the palette's own list shows
        // the one the worktree has.
        let new_panel = self.borrow().palette.new_panel;
        self.close_palette();
        match entry.action {
            Action::Switch { project, path } => {
                self.select_project(project);
                self.activate_worktree(project, &path);
            }
            Action::Ask(_) => {}
            Action::NewShell => self.open_terminal(None, None),
            Action::NewShellWith { name, argv } => self.open_terminal(Some(argv), Some(name)),
            Action::StartAgent(agent) => self.open_terminal(
                Some(vec![agent.program().to_owned()]),
                Some(agent.name().to_owned()),
            ),
            Action::Files if new_panel => self.open(crate::state::PanelSpec::Files { file: None }),
            Action::Files => self.open_files(),
            Action::Explore => self.open_explore(false),
            Action::Review => self.open_explore(true),
            Action::Change => self.open_change(),
            Action::Monitor => self.open_monitor(),
            Action::Tasks => self.open_tasks(),
            Action::DebugBrowser => self.open_debug_browser(),
            Action::Split(axis) => self.split_pane(axis),
            Action::ClosePane => self.close_pane(),
            Action::NextPane => self.focus_next_pane(),
            Action::PreviousPane => self.cycle_pane(-1),
            Action::GrowPane => self.resize_pane(crate::keymap::PANE_STEP),
            Action::ShrinkPane => self.resize_pane(-crate::keymap::PANE_STEP),
            Action::NextTab => self.cycle_tab(1),
            Action::PreviousTab => self.cycle_tab(-1),
            Action::MovePaneToTab => self.move_pane_to_tab(),
            Action::CloseTab => {
                if let Some(index) = self.with_active(|live| live.map(|live| live.active)) {
                    self.close_tab(index);
                }
            }
            Action::RemoveWorktree { project, path } => self.remove_worktree(project, &path),
            Action::ForgetProject(id) => self.remove_project(id),
            Action::KeepProject(id) => self.keep_project(id),
            Action::Settings => self.open_settings(),
            Action::ProjectSettings => self.open_project_settings(),
            Action::SettingsPage(page) => {
                self.show_settings_section(page);
                self.open_settings();
            }
            Action::SearchWorktrees => self.focus_rail_search(),
            Action::FocusTabs => self.focus_tabs(),
            Action::FocusBack => self.focus_chrome_back(),
            Action::ToggleSidebar => self.toggle_sidebar(),
            Action::WorktreeTitles(titles) => self.set_worktree_titles(titles),
            Action::CpuMode(place, mode) => self.set_cpu_mode(place, mode),
            Action::Jj(on) => self.set_jj(on),
            Action::ProjectJj { project, on } => self.set_project_jj(project, on),
            Action::Command(name) => self.run_command(&name),
            Action::RunTask(id) => self.run_task_now(&id),
            Action::SetRuntime(runtime) => self.set_runtime(runtime),
            Action::RebuildDevcontainer => self.rebuild_devcontainer(),
            Action::ExposePort(port) => self.expose_port(port, true),
            Action::UnexposePort(port) => self.expose_port(port, false),
        }
    }

    /// Starts or stops forwarding `port` of the active worktree's sandbox to
    /// this machine, and says where it opened.
    pub(crate) fn expose_port(&self, port: u16, on: bool) {
        #[cfg(unix)]
        {
            let tr = self.tr();
            let hub = {
                let inner = self.borrow();
                inner
                    .active_key()
                    .and_then(|(id, path)| inner.runtimes.ports.get(id, &path))
            };
            let Some(hub) = hub else { return };
            if !on {
                hub.unexpose(port);
            } else {
                match hub.expose(port, None) {
                    Ok(host) => self.notify((tr.port_exposed)(port, host)),
                    Err(error) => self.notify((tr.port_expose_failed)(port, &error.to_string())),
                }
            }
        }
        #[cfg(not(unix))]
        let _ = (port, on);
    }

    /// Opens the native folder chooser. The chosen folder replaces the
    /// project input's text on the next poll.
    pub fn choose_project_folder(&self) {
        self.choose_folder(self.tr().open_project, FolderTarget::Palette);
    }

    /// Opens the native folder chooser titled `title`. The chosen folder
    /// replaces the text of `target` on the next poll.
    pub(crate) fn choose_folder(&self, title: &str, target: FolderTarget) {
        let (sender, wake) = {
            let inner = self.borrow();
            (inner.picked_folders.0.clone(), self.redraw_handle())
        };
        let shown = schnellui::platform::pick_folder(title, move |folder| {
            if let Some(folder) = folder
                && sender.send((target, folder)).is_ok()
                && let Some(signal) = wake.get()
            {
                signal.request_redraw();
            }
        });
        if !shown {
            self.notify(self.tr().no_folder_chooser);
        }
    }

    /// Puts a folder from the chooser into its input. The palette's project
    /// input takes it only while it is still open. Called from `poll`;
    /// allocates nothing while no folder arrived.
    pub(crate) fn apply_picked_folder(&self) {
        let picked = self.borrow().picked_folders.1.try_recv();
        let Ok((target, folder)) = picked else { return };
        let mut inner = self.borrow_mut();
        match target {
            FolderTarget::Palette => {
                let mode = inner.palette.input;
                if inner.palette.open && mode.is_some_and(InputMode::takes_folder) {
                    inner.palette.query = folder.to_string_lossy().into_owned();
                    inner.dirty.palette = true;
                }
            }
            FolderTarget::ProjectSandbox => {
                inner.settings_ui.machines.form.project_path =
                    folder.to_string_lossy().into_owned();
                inner.dirty.settings = true;
            }
        }
    }

    /// Escape: leaves an input mode for the list, else closes the palette.
    /// The rail opens [`InputMode::WorktreeFrom`], so Escape closes it.
    /// [`InputMode::Clone`] goes back to the project input with the URL.
    pub fn palette_escape(&self) {
        let (input, new_panel, from_add, url) = {
            let palette = &self.borrow().palette;
            (
                palette.input,
                palette.new_panel,
                palette.from_add,
                palette.clone_url.clone(),
            )
        };
        if input == Some(InputMode::Clone) {
            self.show_palette(Some(InputMode::Project), false);
            self.borrow_mut().palette.query = url;
        } else if input.is_some() && input != Some(InputMode::WorktreeFrom) {
            self.show_palette(None, new_panel);
            self.borrow_mut().palette.from_add = from_add;
        } else {
            self.dismiss_palette();
        }
    }
}

/// The overlay: a dialog while open, an empty slot while closed.
pub fn palette(ws: &Workspace, viewport: Size) -> Box<dyn View> {
    let viewport_width = viewport.width;
    ROWS.with(|rows| rows.set(rows_for(viewport.height)));
    let inner = ws.borrow();
    if !inner.palette.open {
        return Box::new(Closed);
    }
    let query = inner.palette.query.clone();
    let empty = query.trim().is_empty();
    let mode = inner.palette.input;
    let new_panel = inner.palette.new_panel;
    let regions = inner.regions;
    let scheme = inner.scheme;
    let tr = inner.tr;
    let problem = mode.and_then(|mode| mode.problem(&query, tr));
    // Where a worktree or a review starts, named beside the title.
    let place = match (mode, inner.state.active()) {
        (Some(InputMode::Worktree | InputMode::ReviewBase), Some(project)) => {
            match inner.active_branch() {
                Some(branch) => Some(format!("{} · {branch}", project.name)),
                None => Some(project.name.clone()),
            }
        }
        _ => None,
    };
    drop(inner);

    let typed = ws.clone();
    let list_prompt = if new_panel {
        tr.prompt_panel
    } else {
        tr.prompt_command
    };
    let prompt = mode.map_or(list_prompt, |mode| mode.prompt(tr));
    let project = mode.is_some_and(InputMode::takes_folder);
    let input_width = if project {
        PALETTE_WIDTH - PAD * 2.0 - OPEN_WIDTH - GAP
    } else {
        PALETTE_WIDTH - PAD * 2.0
    };
    // Every field is one line with a plain hint, tall enough that the text
    // has room inside the focus ring. An input mode names what it asks for
    // in a title above the field, so the question stays in view while the
    // user types.
    let field = TextInput::new(query).hint(prompt).height(FIELD_HEIGHT);
    // The hint is dimmer than typed text, so the empty field does not
    // look filled in.
    let input = ThemeProvider::new(
        scheme.field_theme(),
        Named(
            match mode {
                Some(mode) => mode.input_name(),
                None if new_panel => PALETTE_PANEL_INPUT,
                None => PALETTE_INPUT,
            }
            .into(),
            field
                .width(input_width)
                .on_input(move |value| typed.set_palette_query(value)),
        )
        .with_ref(regions.palette_input),
    );

    let dismiss = ws.clone();
    // Room under the field, so the first group heading does not sit on it.
    let mut body = Column::new().gap(GAP * 2.0);
    if mode == Some(InputMode::WorktreeFrom) {
        body = worktree_from_body(ws, body, input);
    } else if mode == Some(InputMode::Clone) {
        body = clone_body(ws, body, input);
    } else if project {
        let choose = ws.clone();
        body = body.child(
            Row::new().gap(GAP).align(Align::Center).child(input).child(
                Button::new(tr.choose_folder)
                    .display(tr.open_folder)
                    .appearance(ButtonAppearance::Outline)
                    .width(OPEN_WIDTH)
                    .height(FIELD_HEIGHT)
                    .text_size(13.5)
                    .tooltip(tr.choose_a_folder)
                    .on_click(move || choose.choose_project_folder()),
            ),
        );
    } else {
        body = body.child(input);
    }
    if let Some(mode) = mode
        && mode != InputMode::Clone
    {
        body = Column::new()
            .gap(GAP)
            .child(
                Pad::insets(EdgeInsets {
                    left: 4.0,
                    top: 2.0,
                    bottom: 2.0,
                    ..EdgeInsets::default()
                })
                .child({
                    let title = Row::new()
                        .gap(8.0)
                        .align(Align::Center)
                        .child(Text::new(mode.title(tr)).size(14.0).bold());
                    match place {
                        Some(place) => title.child(ThemeProvider::new(
                            scheme.muted_theme(),
                            Text::new(place).size(13.0),
                        )),
                        None => title,
                    }
                }),
            )
            .child(body);
        // A name git or the file system would refuse says why under the
        // field, in the danger color, before Enter is pressed.
        if let Some(problem) = problem {
            body = body.child(
                Pad::insets(EdgeInsets {
                    left: 4.0,
                    ..EdgeInsets::default()
                })
                .child(ThemeProvider::new(
                    Theme {
                        text: scheme.lanes[3],
                        ..scheme.theme()
                    },
                    Text::new(problem).size(12.0),
                )),
            );
        }
        body = body.child({
            // The keys on the left, and the button that does what
            // Enter does on the right, for whoever reaches for the
            // pointer.
            let (confirm, cancel) = (ws.clone(), ws.clone());
            Row::new()
                .width(PALETTE_WIDTH - PAD * 2.0)
                .justify(Justify::SpaceBetween)
                .align(Align::Center)
                .child(
                    Pad::insets(EdgeInsets {
                        left: 4.0,
                        ..EdgeInsets::default()
                    })
                    .child(ThemeProvider::new(
                        scheme.muted_theme(),
                        Text::new(tr.palette_input_keys_hint).size(12.0),
                    )),
                )
                .child(
                    Row::new()
                        .gap(8.0)
                        .child(
                            Button::new(tr.cancel)
                                .appearance(ButtonAppearance::Outline)
                                .width(OPEN_WIDTH - 16.0)
                                .height(28.0)
                                .text_size(13.0)
                                .on_click(move || cancel.close_palette()),
                        )
                        .child(
                            Button::new(mode.confirm(tr))
                                .appearance(ButtonAppearance::Solid)
                                // Enter does nothing on an empty line where one
                                // is needed, so neither does the button.
                                .disabled((empty && mode.needs_text()) || problem.is_some())
                                .width(OPEN_WIDTH)
                                .height(28.0)
                                .text_size(13.0)
                                .on_click(move || confirm.palette_enter()),
                        ),
                )
        });
    }
    if mode.is_none() {
        body = body.child(list(ws).with_ref(regions.palette_list));
    }
    Box::new(
        Dialog::new(if new_panel {
            tr.new_panel
        } else {
            tr.command_palette
        })
        .modal()
        .undecorated()
        // Centered on the window, like the settings, so every overlay
        // shares one axis.
        .position(DialogPosition::At(Point {
            x: ((viewport_width - PALETTE_WIDTH) / 2.0).max(PAD),
            y: TOP,
        }))
        // The point is in window coordinates.
        .viewport_inset(0.0)
        .width(PALETTE_WIDTH)
        .backdrop(scheme.backdrop)
        .surface(scheme.rail)
        .padding(PAD)
        .on_dismiss(move || dismiss.dismiss_palette())
        .child(body),
    )
}

/// The two inputs of [`InputMode::WorktreeFrom`] under a line naming the
/// source worktree, and a hint where the new one goes.
fn worktree_from_body(ws: &Workspace, body: Column, name_input: impl View + 'static) -> Column {
    let inner = ws.borrow();
    let scheme = inner.scheme;
    let tr = inner.tr;
    let width = PALETTE_WIDTH - PAD * 2.0;
    let (source, repo) = match &inner.palette.source {
        Some((project, path)) => {
            let snapshot = inner.snapshots.get(project);
            let source = snapshot
                .and_then(|s| s.worktrees.iter().find(|w| &w.path == path))
                .map_or_else(|| tail_path(path, 48), branch_name);
            let repo = snapshot
                .and_then(|s| s.repo.root.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            (source, repo)
        }
        None => (String::new(), String::new()),
    };
    let branch = inner.palette.branch.clone();
    drop(inner);
    let muted = |text: String| {
        Pad::insets(EdgeInsets::symmetric(4.0, 2.0)).child(ThemeProvider::new(
            scheme.muted_theme(),
            Text::new(text).size(12.0).ellipsis(),
        ))
    };
    let typed = ws.clone();
    body.child(muted((tr.worktree_source)(&truncate(&source, 56))))
        .child(name_input)
        // The same field as the name above it: height, frame and hint.
        .child(ThemeProvider::new(
            scheme.field_theme(),
            Named(
                PALETTE_BRANCH_INPUT.into(),
                TextInput::new(branch)
                    .hint(tr.branch_optional)
                    .height(FIELD_HEIGHT)
                    .width(width)
                    .on_input(move |value| typed.set_palette_branch(value)),
            ),
        ))
        .child(muted((tr.creates_next_to)(&repo)))
}

/// The URL being cloned, the folder input, the places to clone to and,
/// for a VM on a remote machine, the VM name input.
fn clone_body(ws: &Workspace, body: Column, folder_input: impl View + 'static) -> Column {
    let inner = ws.borrow();
    let scheme = inner.scheme;
    let tr = inner.tr;
    let palette = &inner.palette;
    let url = truncate(&palette.clone_url, 64);
    let asks_vm = matches!(
        palette.clone_targets.get(palette.highlight),
        Some(CloneTarget::MachineVm(_))
    );
    let vm = palette.vm.clone();
    let regions = inner.regions;
    drop(inner);
    let mut body = body
        .child(
            Pad::insets(EdgeInsets::symmetric(4.0, 2.0)).child(ThemeProvider::new(
                scheme.muted_theme(),
                Text::new((tr.clone_from)(&url)).size(12.0).ellipsis(),
            )),
        )
        .child(folder_input)
        .child(list(ws).with_ref(regions.palette_list));
    if asks_vm {
        let typed = ws.clone();
        body = body.child(ThemeProvider::new(
            scheme.field_theme(),
            Named(
                PALETTE_VM_INPUT.into(),
                TextInput::new(vm)
                    .hint(tr.clone_vm_name)
                    .height(FIELD_HEIGHT)
                    .width(PALETTE_WIDTH - PAD * 2.0)
                    .on_input(move |value| typed.set_palette_vm(value)),
            ),
        ));
    }
    body
}

/// The filtered rows with the highlight, or in [`InputMode::Clone`] the
/// places to clone to. Clicking a place picks it; Enter clones.
pub fn list(ws: &Workspace) -> impl View + use<> {
    let lists_panels = ws.borrow().palette.new_panel;
    let (rows, keys, highlight, armed, headed, scheme, tr) = {
        let inner = ws.borrow();
        let rows = if inner.palette.input == Some(InputMode::Clone) {
            // `palette_run` picks the row instead of running the action.
            inner
                .palette
                .clone_targets
                .iter()
                .map(|target| plain(&target.label(inner.tr), Action::Ask(InputMode::Clone)))
                .collect()
        } else {
            visible(&inner)
        };
        let keys: Vec<Option<String>> = rows
            .iter()
            .map(|entry: &Entry| {
                let action = bound(&entry.action)?;
                Some(inner.keymap.keys_for(&action)?.to_string())
            })
            .collect();
        // Headings stand over the groups while the list is in its own
        // order: nothing typed, and more than one group.
        let headed = inner.palette.query.trim().is_empty()
            && inner.palette.input.is_none()
            && rows.windows(2).any(|pair| pair[0].group != pair[1].group);
        (
            rows,
            keys,
            inner.palette.highlight,
            inner.palette.armed,
            headed,
            inner.scheme,
            inner.tr,
        )
    };
    let mut column = Column::new().width(PALETTE_WIDTH - PAD * 2.0);
    if rows.is_empty() {
        column = column.child(
            Pad::insets(EdgeInsets::symmetric(12.0, 6.0)).child(ThemeProvider::new(
                scheme.muted_theme(),
                Text::new(if lists_panels {
                    tr.no_matching_panel
                } else {
                    tr.no_matching_command
                })
                .size(13.0),
            )),
        );
    }
    let shown = ROWS.with(std::cell::Cell::get);
    // A window of `shown` rows that follows the highlight, so the arrow
    // keys reach every command and not only the first screen of them.
    let total = rows.len();
    let first = (highlight + 1).saturating_sub(shown);
    let dotted = rows
        .iter()
        .skip(first)
        .take(shown)
        .any(|entry| entry.lane.is_some());
    let mut last = None;
    for (index, (entry, keys)) in rows.into_iter().zip(keys).enumerate() {
        if index < first || index >= first + shown {
            continue;
        }
        if headed && last != Some(entry.group) {
            last = Some(entry.group);
            column = column.child(
                Pad::insets(EdgeInsets {
                    // More room above than below, so a heading sits with
                    // the group it names.
                    top: if index == 0 { 2.0 } else { 12.0 },
                    right: 12.0,
                    bottom: 4.0,
                    // In line with the labels, past the dots' room.
                    left: if dotted { 12.0 + DOT_ROOM } else { 12.0 },
                })
                // Small, muted capitals, like the settings nav's groups, so
                // a heading never reads as one more command.
                .child(ThemeProvider::new(
                    scheme.muted_theme(),
                    Text::new(entry.group.title(tr).to_uppercase()).size(11.5),
                )),
            );
        }
        let run = ws.clone();
        // An armed row says that the next Enter runs it.
        let label = if armed == Some(index) {
            (tr.palette_confirm)(&truncate(&entry.label, 40))
        } else {
            truncate(&entry.label, 64)
        };
        let mut row = ListRow::new(label)
            .width(PALETTE_WIDTH - PAD * 2.0)
            .height(ROW_HEIGHT)
            .selected(index == highlight)
            .selected_background(scheme.selected())
            .on_click(move || run.palette_run(index));
        // Rows without a lane keep the dot's room, so every label starts
        // at the same edge when worktrees share the list with commands.
        match entry.lane {
            Some(lane) => row = row.dot(lane),
            None if dotted => row = row.dot(Color::rgba(0, 0, 0, 0)),
            None => {}
        }
        // The highlighted row carries the accent bar the rail and the
        // settings nav use for the shown row.
        if index == highlight {
            row = row.accent_bar(scheme.lanes[0]);
        }
        // Key hints sit on the keycap pills of the keyboard settings.
        if let Some(keys) = keys {
            let pill = if index == highlight {
                scheme.keycap_on_selected()
            } else {
                scheme.keycap()
            };
            row = row.trailing(keys).trailing_pill(pill);
        }
        // Destructive rows read in the danger color before Enter runs them.
        let text = if entry.action.is_destructive() {
            scheme.lanes[3]
        } else {
            scheme.ink
        };
        // The pointer lays the light hover wash on its row. The chosen row
        // keeps its stronger tint and its bar, so it stays the one Enter runs.
        let theme = Theme {
            text,
            on_accent: scheme.ink,
            ..scheme.theme()
        };
        column = column.child(ThemeProvider::new(theme, row));
    }
    let below = total.saturating_sub(first + shown);
    // A rule sets the footer apart from the last command, in every list,
    // and the count of hidden rows sits under it.
    let mut footer =
        Column::new()
            .gap(6.0)
            .child(Fill::new(PALETTE_WIDTH - PAD * 2.0, 1.0, scheme.line));
    // Where the window sits in a list longer than it, so a scrolled list
    // says what is above as well as what is below.
    if total > shown {
        let to = (first + shown).min(total);
        footer = footer.child(
            Pad::insets(EdgeInsets {
                left: 12.0,
                ..EdgeInsets::default()
            })
            .child(ThemeProvider::new(
                scheme.muted_theme(),
                Text::new((tr.palette_more)(first + 1, to, total, below)).size(12.0),
            )),
        );
    }
    // The key that moves focus between the window's areas, which nothing
    // else on screen names; the palette is where keys are looked up.
    let areas = ws
        .borrow()
        .keymap
        .keys_for(&crate::keymap::Action::FocusTabs)
        .map(|keys| (tr.palette_areas_hint)(&keys.to_string()));
    // The keys, in the list's region, so a list without rows does not
    // offer to choose or run one.
    let keys = if total == 0 {
        tr.palette_empty_keys_hint.to_owned()
    } else {
        match &areas {
            Some(areas) => format!("{} · {areas}", tr.palette_keys_hint),
            None => tr.palette_keys_hint.to_owned(),
        }
    };
    let footer = footer.child(
        Pad::insets(EdgeInsets {
            left: 12.0,
            ..EdgeInsets::default()
        })
        .child(ThemeProvider::new(
            scheme.muted_theme(),
            Text::new(keys).size(12.0),
        )),
    );
    column.child(
        Pad::insets(EdgeInsets {
            top: 6.0,
            ..EdgeInsets::default()
        })
        .child(footer),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_git_refuses_are_caught_while_typed() {
        for good in ["", "topic", "feature/topic", "fix-1.2", "a_b"] {
            assert!(branch_name_ok(good), "{good}");
        }
        for bad in [
            "two words",
            "-x",
            "a..b",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "a/",
            "a.lock",
            "a//b",
            "a@{1}",
            "a/.b",
            ".a",
        ] {
            assert!(!branch_name_ok(bad), "{bad}");
        }
    }

    fn matches(labels: &[&str], query: &str) -> Vec<&'static str> {
        let owned: Vec<&'static str> = labels
            .iter()
            .map(|l| &*Box::leak(l.to_string().into_boxed_str()))
            .collect();
        filter(&owned, query, MAX_ROWS)
            .into_iter()
            .map(|i| owned[i])
            .collect()
    }

    #[test]
    fn empty_query_keeps_order() {
        assert_eq!(matches(&["b", "a", "c"], ""), ["b", "a", "c"]);
    }

    #[test]
    fn matching_ignores_case_and_needs_order() {
        let labels = ["New shell", "Close panel"];
        assert_eq!(matches(&labels, "NEW"), ["New shell"]);
        assert_eq!(matches(&labels, "nsh"), ["New shell"]);
        assert!(matches(&labels, "hsn").is_empty());
        assert!(matches(&labels, "xyz").is_empty());
    }

    #[test]
    fn earlier_matches_rank_first_then_shorter_labels() {
        let labels = [
            "Remove worktree feature",
            "Switch to app / main",
            "Split stacked",
            "Split side by side",
        ];
        // Starts at 0 in both split rows, shorter label first.
        assert_eq!(
            matches(&labels, "spl"),
            ["Split stacked", "Split side by side"]
        );
        let ranked = matches(
            &["Remove worktree feature", "Split stacked", "Switch to app"],
            "t",
        );
        assert_eq!(
            ranked,
            ["Switch to app", "Split stacked", "Remove worktree feature"]
        );
    }

    #[test]
    fn word_starts_beat_scattered_matches() {
        let labels = ["Show tasks", "New shell", "Explore history"];
        assert_eq!(matches(&labels, "sh"), ["Show tasks", "New shell"]);
        assert_eq!(matches(&labels, "hist"), ["Explore history"]);
        // A run inside a word beats letters spread over the label.
        assert_eq!(
            matches(&["Split stacked", "Close tab"], "ta"),
            ["Close tab", "Split stacked"]
        );
    }

    #[test]
    fn destructive_actions_rank_after_safe_ones_of_the_same_fit() {
        let labels = [
            "Remove worktree feature/ui-polish",
            "Switch to repo / feature/ui-polish",
        ];
        assert_eq!(filter_ranked(&labels, &[true, false], "feat", 10), [1, 0]);
        // Nothing typed keeps the display order.
        assert_eq!(filter_ranked(&labels, &[true, false], "", 10), [0, 1]);
    }

    #[test]
    fn the_list_grows_with_the_window() {
        assert_eq!(rows_for(600.0), 10);
        assert_eq!(rows_for(748.0), MAX_ROWS);
        assert_eq!(rows_for(200.0), FEWEST_ROWS);
        assert_eq!(rows_for(800.0), 17);
        assert_eq!(rows_for(1400.0), MOST_ROWS);
    }

    #[test]
    fn results_are_capped() {
        let labels: Vec<String> = (0..30).map(|i| format!("row {i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        assert_eq!(filter(&refs, "", MAX_ROWS).len(), MAX_ROWS);
        assert_eq!(filter(&refs, "row", 5).len(), 5);
    }

    #[test]
    fn non_ascii_labels_match() {
        assert_eq!(matches(&["Open project…"], "pro…"), ["Open project…"]);
    }

    #[test]
    fn scattered_letters_across_a_long_label_do_not_match() {
        let labels = [
            "CPU for terminals on this machine",
            "Split pane side by side",
        ];
        assert_eq!(matches(&labels, "pane"), ["Split pane side by side"]);
        assert_eq!(
            matches(&["Split pane side by side"], "sps"),
            ["Split pane side by side"]
        );
    }
}
