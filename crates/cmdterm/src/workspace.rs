//! The live application model: saved state plus running panels, VCS
//! snapshots, extension host, and the dirty flags that decide which part of
//! the UI rebuilds.
//!
//! Everything here lives on the UI thread. Worker threads (PTY readers, the
//! VCS worker) only bump revisions and call the redraw signal; the next
//! [`Workspace::poll`] picks up what they produced.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use crate::ext::{
    Context as ExtContext, Event, ExtError, ExtensionHost, NotifyLevel, Permission, Request,
};
use crate::state::{
    AgentControl, AppState, PaneLayout, PanelSpec, Runtime, Settings, SettingsStore, SplitAxis,
    StateStore, TabConfig,
};
use crate::term::{Attention, TermSize, TerminalSession};
use crate::vcs::Repository;
use cmdterm_env::Environment;
use schnellui::RedrawSignal;
use schnellui::scene::{ComponentRef, Size};
use schnellui::text::TextShaper;
use schnellui::widgets::{DockPosition, TerminalCellMetrics};

use crate::browser::BrowserPanel;
use crate::debug_browser::DebugBrowserPanel;
use crate::explore::{ExplorePanel, Mode as ExploreMode};
use crate::files::FilesPanel;
use crate::i18n::{Lang, Strings};
use crate::keymap::Keymap;
use crate::monitor::MonitorPanel;
use crate::taskview::TasksPanel;
use crate::terminal_panel::{GridStyle, TerminalPanel};
use crate::ui::{PaletteState, Scheme, SettingsState, resolve_scheme};
use crate::vcs_worker::{Job, RemoteOp, Reply, Snapshot, VcsWorker};

/// Notices kept in memory; the status bar shows the newest.
const MAX_NOTICES: usize = 4;
/// How long a notice stays in the status bar.
pub const NOTICE_TTL: Duration = Duration::from_secs(8);

/// A terminal whose program fails this soon after it started failed to
/// start. Its pane closes like any other, and a notice says why.
const FAILED_START: Duration = Duration::from_secs(3);

/// A fetch, pull or push that runs in a worktree, see
/// [`Workspace::run_remote`].
struct RemoteRun {
    project: u64,
    path: PathBuf,
    /// What the notices name: the project for a fetch, else the branch.
    label: String,
}

/// A message for the user, shown in the status bar until it expires or a
/// newer one replaces it.
#[derive(Debug)]
pub struct Notice {
    pub text: String,
    pub at: Instant,
}

impl Notice {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            at: Instant::now(),
        }
    }
}

pub enum LivePanel {
    Terminal(Rc<TerminalPanel>),
    Browser(BrowserPanel),
    Files(Rc<FilesPanel>),
    Explore(Rc<ExplorePanel>),
    Monitor(Rc<MonitorPanel>),
    DebugBrowser(DebugBrowserPanel),
    Tasks(Rc<TasksPanel>),
}

impl LivePanel {
    pub fn title(&self) -> String {
        match self {
            Self::Terminal(terminal) => {
                let mut title = terminal.title();
                let attention = terminal.session.attention();
                if attention.wants_user() {
                    title.insert_str(0, "● ");
                }
                if let Some(code) = terminal.session.exit_code() {
                    match code {
                        Some(code) => title.push_str(&format!(" (exit {code})")),
                        None => title.push_str(" (killed)"),
                    }
                }
                title
            }
            Self::Browser(browser) => browser.title(),
            Self::Files(files) => files.title(),
            Self::Explore(explore) => explore.title(),
            Self::Monitor(monitor) => monitor.title(),
            Self::DebugBrowser(debug) => debug.title(),
            Self::Tasks(tasks) => tasks.title(),
        }
    }

    /// Ends a terminal's remote session; see [`TerminalPanel::end_session`].
    fn end_session(&self) {
        if let Self::Terminal(terminal) = self {
            terminal.end_session();
        }
    }

    pub(crate) fn spec(&self) -> PanelSpec {
        match self {
            Self::Terminal(terminal) => PanelSpec::Terminal {
                title: terminal.fixed_title.clone(),
                command: terminal.command.clone(),
                cwd: terminal.cwd.clone(),
                session: terminal.remote_session.clone(),
                control: terminal.session_control.clone(),
                agent: terminal.agent_session.borrow().clone(),
                once: Vec::new(),
            },
            Self::Browser(browser) => PanelSpec::Browser { url: browser.url() },
            Self::Files(files) => match files.view_source() {
                Some((view, key)) => PanelSpec::View {
                    view: view.to_owned(),
                    key: key.to_owned(),
                },
                None => PanelSpec::Files { file: files.file() },
            },
            Self::Explore(explore) => PanelSpec::Explore {
                review: explore.mode() == ExploreMode::Review,
                change: explore.mode() == ExploreMode::Change,
                base: explore.base(),
                committed: !explore.uncommitted(),
            },
            Self::Monitor(_) => PanelSpec::Monitor,
            Self::DebugBrowser(_) => PanelSpec::DebugBrowser,
            Self::Tasks(_) => PanelSpec::Tasks,
        }
    }
}

/// Panel id no panel has. The only pane of a worktree without panels shows it.
const NO_PANEL: u64 = 0;

/// Most panes one tab shows. Each pane position has its own accessible
/// names and focused-input handlers, registered once per mount.
pub const MAX_PANES: usize = 8;

/// One tab: its own tree of panes, each showing a panel no other pane shows.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveTab {
    pub layout: PaneLayout<u64>,
    /// Position of the focused pane in `layout`.
    pub focused: usize,
}

impl LiveTab {
    fn new(panel: u64) -> Self {
        Self {
            layout: PaneLayout::pane(panel),
            focused: 0,
        }
    }

    /// The panel in the focused pane.
    pub fn focused_panel(&self) -> Option<u64> {
        self.layout.get(self.focused).filter(|id| *id != NO_PANEL)
    }
}

/// An agent running in a terminal of a worktree, for its row in the Agents
/// tab.
#[derive(Clone, Debug)]
pub struct LiveAgent {
    pub panel: u64,
    pub attention: Attention,
    pub message: String,
    /// The agent session's title; empty until the agent set one.
    pub title: String,
    /// The terminal's tab label.
    pub label: String,
}

/// The running panels of one worktree. They keep running while another
/// worktree is shown.
pub struct LiveWorktree {
    /// Every panel of every tab, in the order they were opened.
    pub panels: Vec<(u64, LivePanel)>,
    /// The tabs in strip order. Never empty: a worktree without panels has
    /// one tab whose pane shows [`NO_PANEL`].
    pub tabs: Vec<LiveTab>,
    /// Index of the shown tab.
    pub active: usize,
}

impl LiveWorktree {
    /// Ends the remote sessions of the terminals, before the worktree's
    /// panels go away for good.
    pub fn end_sessions(&self) {
        for (_, panel) in &self.panels {
            panel.end_session();
        }
    }

    /// The most urgent attention state of this worktree's terminals, with the
    /// message of the terminal that has it.
    pub fn attention(&self) -> (Attention, String) {
        match self.most_urgent().map(|(_, terminal)| terminal) {
            Some(terminal) => (
                terminal.session.attention(),
                terminal.session.attention_message(),
            ),
            None => (Attention::None, String::new()),
        }
    }

    /// The id and panel of the terminal with the most urgent attention
    /// state, the one [`Self::attention`] reports.
    fn most_urgent(&self) -> Option<(u64, &Rc<TerminalPanel>)> {
        self.panels
            .iter()
            .filter_map(|(id, panel)| match panel {
                LivePanel::Terminal(terminal) => Some((*id, terminal)),
                LivePanel::Browser(_)
                | LivePanel::Files(_)
                | LivePanel::Explore(_)
                | LivePanel::Monitor(_)
                | LivePanel::DebugBrowser(_)
                | LivePanel::Tasks(_) => None,
            })
            .max_by_key(|(_, terminal)| terminal.session.attention())
    }

    /// The terminals agents run in, in the order they were opened: those
    /// with an attention state or an agent session.
    pub fn agents(&self) -> Vec<LiveAgent> {
        self.panels
            .iter()
            .filter_map(|(id, panel)| match panel {
                LivePanel::Terminal(terminal) => {
                    let attention = terminal.session.attention();
                    let session = terminal.agent_session.borrow().is_some();
                    (attention != Attention::None || session).then(|| LiveAgent {
                        panel: *id,
                        attention,
                        message: terminal.session.attention_message(),
                        title: terminal.agent_title(),
                        label: terminal.title(),
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// The id of the terminal that has `agent`'s session open.
    fn session_panel(&self, agent: &crate::state::AgentSession) -> Option<u64> {
        self.panels.iter().find_map(|(id, panel)| match panel {
            LivePanel::Terminal(terminal)
                if terminal
                    .agent_session
                    .borrow()
                    .as_ref()
                    .is_some_and(|a| a.agent == agent.agent && a.session == agent.session) =>
            {
                Some(*id)
            }
            _ => None,
        })
    }

    /// Shows the tab that holds panel `id` and focuses its pane. False if
    /// no tab holds it or it already is shown and focused.
    fn reveal(&mut self, id: u64) -> bool {
        let Some(index) = self.tab_of(id) else {
            return false;
        };
        let Some(pane) = self.tabs[index].layout.position(id) else {
            return false;
        };
        if self.active == index && self.tabs[index].focused == pane {
            return false;
        }
        self.active = index;
        self.tabs[index].focused = pane;
        true
    }
}

impl Default for LiveWorktree {
    fn default() -> Self {
        Self {
            panels: Vec::new(),
            tabs: vec![LiveTab::new(NO_PANEL)],
            active: 0,
        }
    }
}

impl LiveWorktree {
    /// The panel with id `id`.
    pub fn panel(&self, id: u64) -> Option<&(u64, LivePanel)> {
        self.panels.iter().find(|(panel, _)| *panel == id)
    }

    /// Index into `panels` of the panel with id `id`.
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.panels.iter().position(|(panel, _)| *panel == id)
    }

    /// The shown tab.
    pub fn tab(&self) -> &LiveTab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut LiveTab {
        &mut self.tabs[self.active]
    }

    /// The panes of the shown tab.
    pub fn layout(&self) -> &PaneLayout<u64> {
        &self.tab().layout
    }

    /// Position of the focused pane in the shown tab.
    pub fn focused(&self) -> usize {
        self.tab().focused
    }

    /// The panel shown in `pane` of the shown tab.
    pub fn pane_panel(&self, pane: usize) -> Option<&(u64, LivePanel)> {
        self.panel(self.layout().get(pane)?)
    }

    /// The panel shown in the focused pane.
    pub fn focused_panel(&self) -> Option<&(u64, LivePanel)> {
        self.pane_panel(self.focused())
    }

    /// The panel that names `tab`: a pane whose agent set a title of its
    /// own, the focused one first, else the focused pane.
    pub fn title_panel(&self, tab: &LiveTab) -> Option<&(u64, LivePanel)> {
        let focused = tab.focused_panel()?;
        let titled = |id: u64| {
            self.panel(id).filter(|(_, panel)| {
                matches!(panel, LivePanel::Terminal(terminal) if terminal.agent_titled())
            })
        };
        if tab.layout.len() > 1 {
            let agent = titled(focused).or_else(|| {
                (0..tab.layout.len())
                    .filter_map(|pane| tab.layout.get(pane))
                    .filter(|&id| id != NO_PANEL && id != focused)
                    .find_map(titled)
            });
            if agent.is_some() {
                return agent;
            }
        }
        self.panel(focused)
    }

    /// Index of the tab that holds panel `id`.
    pub fn tab_of(&self, id: u64) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.layout.position(id).is_some())
    }

    /// Removes panel `id` and its pane; the sibling takes the space and a tab
    /// whose last pane goes away goes away too. False if no panel has `id`.
    fn remove_panel(&mut self, id: u64) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        self.panels.remove(index).1.end_session();
        if let Some(tab) = self.tab_of(id) {
            let tab = &mut self.tabs[tab];
            if let Some(pane) = tab.layout.position(id)
                && tab.layout.remove(pane).is_some()
                && tab.focused >= pane
            {
                tab.focused = tab.focused.saturating_sub(1);
            }
        }
        // A lone pane cannot be removed; normalize drops its tab.
        self.normalize();
        true
    }

    /// Whether the shown tab is the placeholder of a worktree without panels.
    fn is_blank(&self) -> bool {
        self.layout().get(0) == Some(NO_PANEL)
    }

    /// Restores the invariants: every running panel sits in exactly one pane
    /// of one tab, a tab has 1 to [`MAX_PANES`] panes, and the shown tab and
    /// focused panes exist. A panel no tab holds gets a tab of its own.
    fn normalize(&mut self) {
        let mut seen = Vec::with_capacity(self.panels.len());
        let panels = &self.panels;
        let mut tabs = Vec::with_capacity(self.tabs.len());
        for (index, tab) in self.tabs.iter().enumerate() {
            let mut count = 0;
            let layout = tab.layout.filter_map(&mut |id| {
                let keep = count < MAX_PANES
                    && !seen.contains(&id)
                    && panels.iter().any(|(panel, _)| *panel == id);
                if keep {
                    seen.push(id);
                    count += 1;
                }
                keep.then_some(id)
            });
            match layout {
                Some(layout) => {
                    let focused = tab.focused.min(layout.len() - 1);
                    tabs.push(LiveTab { layout, focused });
                }
                None if index < self.active => self.active -= 1,
                None => {}
            }
        }
        for (id, _) in panels {
            if !seen.contains(id) {
                tabs.push(LiveTab::new(*id));
            }
        }
        if tabs.is_empty() {
            tabs.push(LiveTab::new(NO_PANEL));
        }
        self.active = self.active.min(tabs.len() - 1);
        self.tabs = tabs;
    }
}

/// Content size of every pane of `layout` in pane order, when the panes share
/// `full`. A lone pane fills it; split panes each lose a border on every side.
pub fn pane_sizes<T: Copy + PartialEq>(layout: &PaneLayout<T>, full: Size, out: &mut Vec<Size>) {
    fn framed<T: Copy + PartialEq>(layout: &PaneLayout<T>, outer: Size, out: &mut Vec<Size>) {
        use crate::ui::{PANE_FRAME, PANE_PAD, PANE_TITLE};
        match layout {
            PaneLayout::Pane { .. } => out.push(Size {
                width: (outer.width - PANE_FRAME - PANE_PAD * 2.0).max(1.0),
                height: (outer.height - PANE_FRAME - PANE_TITLE - PANE_PAD).max(1.0),
            }),
            PaneLayout::Split {
                axis,
                first,
                second,
                share,
            } => {
                let spans = (first.span(*axis), second.span(*axis));
                let (a, b) = split_sizes(outer, *axis, spans, *share);
                framed(first, a, out);
                framed(second, b, out);
            }
        }
    }
    out.clear();
    match layout {
        PaneLayout::Pane { .. } => out.push(full),
        PaneLayout::Split { .. } => framed(layout, full, out),
    }
}

/// The least room a dragged split leaves each pane on a side while the
/// window has it, in logical pixels.
const SHARED_PANE_LEAST: f32 = 160.0;

/// Outer sizes of the two halves of a split of `outer` along `axis`, less the
/// separator between them. Each half gets room in proportion to the panes it
/// lays out along `axis`, so a row of three panes shares the width evenly.
/// A resized split, with a `share` other than 0, gives the first half that
/// many thousandths of the room instead.
/// The first half is cut to whole pixels and the second takes the rest.
pub fn split_sizes(
    outer: Size,
    axis: SplitAxis,
    (first_span, second_span): (usize, usize),
    share: u16,
) -> (Size, Size) {
    use crate::ui::PANE_SEPARATOR;
    let total = match axis {
        SplitAxis::SideBySide => outer.width,
        SplitAxis::Stacked => outer.height,
    };
    let count = (first_span + second_span) as f32;
    let each = (total - PANE_SEPARATOR * (count - 1.0)) / count;
    let even = |span: usize| each * span as f32 + PANE_SEPARATOR * (span as f32 - 1.0);
    let first = if share > 0 {
        // A window that shrank after a line was dragged keeps each side
        // usable: no side falls under what an even split would give it,
        // or under SHARED_PANE_LEAST per pane, whichever is less.
        let least = |span: usize| even(span).min(SHARED_PANE_LEAST * span as f32);
        let wanted = (total - PANE_SEPARATOR) * f32::from(share) / 1000.0;
        let most = total - PANE_SEPARATOR - least(second_span);
        wanted.min(most).max(least(first_span)).floor().max(1.0)
    } else {
        even(first_span).floor().max(1.0)
    };
    let second = (total - PANE_SEPARATOR - first).max(1.0);
    match axis {
        SplitAxis::SideBySide => (
            Size {
                width: first,
                height: outer.height,
            },
            Size {
                width: second,
                height: outer.height,
            },
        ),
        SplitAxis::Stacked => (
            Size {
                width: outer.width,
                height: first,
            },
            Size {
                width: outer.width,
                height: second,
            },
        ),
    }
}

/// Whole cells that fit in `size`.
pub fn grid_for(size: schnellui::scene::Size, metrics: TerminalCellMetrics) -> (u16, u16) {
    (
        (size.width / metrics.width).floor().clamp(2.0, 1000.0) as u16,
        (size.height / metrics.height).floor().clamp(1.0, 1000.0) as u16,
    )
}

/// The grid of an explore, monitor or tasks panel in a pane of `grid`
/// cells: the rows below the bar above it and its rule,
/// [`crate::ui::EXPLORE_GRID_TOP`], go to the grid.
fn explore_grid((cols, rows): (u16, u16), metrics: TerminalCellMetrics) -> (u16, u16) {
    let bar = (crate::ui::EXPLORE_GRID_TOP / metrics.height).ceil() as u16;
    (cols, rows.saturating_sub(bar).max(1))
}

/// `layout` with pane `id` dropped on pane `target` at `position`: swapped
/// with it at the middle, else split off at that edge. Returns the new
/// layout and the moved pane's place, or `None` when nothing moves.
fn docked(
    layout: &PaneLayout<u64>,
    id: u64,
    target: usize,
    position: DockPosition,
) -> Option<(PaneLayout<u64>, usize)> {
    let mut layout = layout.clone();
    let from = layout.position(id)?;
    let shown = layout.get(target)?;
    if from == target {
        return None;
    }
    let split = match position {
        DockPosition::Center => None,
        DockPosition::Left => Some((SplitAxis::SideBySide, true)),
        DockPosition::Right => Some((SplitAxis::SideBySide, false)),
        DockPosition::Top => Some((SplitAxis::Stacked, true)),
        DockPosition::Bottom => Some((SplitAxis::Stacked, false)),
    };
    match split {
        None => {
            layout.set(from, shown);
            layout.set(target, id);
            Some((layout, target))
        }
        Some((axis, before)) => {
            // Two panes that stay split along the same axis keep their
            // divider where the user left it.
            let kept = match &layout {
                PaneLayout::Split {
                    axis: was, share, ..
                } if layout.len() == 2 && *was == axis => Some(*share),
                _ => None,
            };
            layout.remove(from)?;
            let target = layout.position(shown).unwrap_or(0);
            if !layout.split_with(target, axis, id, before) {
                return None;
            }
            if let Some(share) = kept.filter(|share| *share != 0) {
                layout.set_share(0, share);
            }
            let focused = layout.position(id).unwrap_or(0);
            Some((layout, focused))
        }
    }
}

/// Whether two layouts split the same panes the same way, whatever their
/// dividers' places.
fn same_shape(a: &PaneLayout<u64>, b: &PaneLayout<u64>) -> bool {
    match (a, b) {
        (PaneLayout::Pane { panel: a }, PaneLayout::Pane { panel: b }) => a == b,
        (
            PaneLayout::Split {
                axis: a_axis,
                first: a_first,
                second: a_second,
                ..
            },
            PaneLayout::Split {
                axis: b_axis,
                first: b_first,
                second: b_second,
                ..
            },
        ) => a_axis == b_axis && same_shape(a_first, b_first) && same_shape(a_second, b_second),
        _ => false,
    }
}

/// Where `focus_tabs` last sent focus in the chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChromeStep {
    Tabs,
    Status,
    Rail,
}

/// What a pointer drag in the tab strip or a pane title carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dragged {
    /// The tab holding this panel.
    Tab(u64),
    /// The pane showing this panel.
    Pane(u64),
}

/// What the rail lists under its tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RailTab {
    /// Every project with its worktrees.
    #[default]
    Projects,
    /// The worktrees where agents ran: finished ones first, working and
    /// merged ones collapsed.
    Agents,
}

/// Which UI regions must rebuild after this poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dirty {
    pub sidebar: bool,
    /// The tab bar and the active panel.
    pub main: bool,
    /// Only the tab bar, e.g. after a program changed its title.
    pub tabs: bool,
    /// Only the status bar, e.g. after a notice appeared or expired.
    pub status: bool,
    /// The whole command palette overlay: opened, closed or switched mode.
    pub palette: bool,
    /// Only the palette's result list, e.g. after the filter text changed.
    pub palette_list: bool,
    /// The settings overlay: opened, closed, or its content changed.
    pub settings: bool,
    /// The approval overlay: a sandbox request was asked or answered.
    pub approval: bool,
    /// The notification overlay: opened, closed, or its list changed.
    pub notifications: bool,
    /// The setup overlay: opened, closed, or its stage or content changed.
    pub setup: bool,
    /// The rail's search field: focused, or cleared. The list under it
    /// follows `sidebar`.
    pub search: bool,
    /// Focus goes back to the shown panel, as Escape on the rail, the tab
    /// strip or the status bar asks. The status bar rebuilds to carry it.
    pub refocus: bool,
    /// Focus goes from the panel to the shown tab in the tab strip, where
    /// Tab reaches the rest of the chrome. The strip rebuilds to carry it.
    pub focus_tabs: bool,
    /// Focus goes from the tab strip to the status bar, on the second
    /// `focus_tabs` press. The status bar rebuilds to carry it.
    pub focus_status: bool,
    /// Escape closed the notification overlay, so focus goes back to the
    /// bell it opened from.
    pub bell: bool,
    /// Escape closed the new panel list the `+` opened, so focus goes back
    /// to the `+`.
    pub new_panel: bool,
    /// Rebuild everything, e.g. after the terminal font metrics or the color
    /// scheme changed.
    pub full: bool,
}

impl Dirty {
    pub fn any(self) -> bool {
        self.sidebar
            || self.main
            || self.tabs
            || self.status
            || self.palette
            || self.palette_list
            || self.settings
            || self.approval
            || self.notifications
            || self.setup
            || self.search
            || self.refocus
            || self.focus_tabs
            || self.focus_status
            || self.bell
            || self.new_panel
            || self.full
    }
}

/// Stable references to the regions that rebuild independently.
#[derive(Clone, Copy)]
pub struct Regions {
    /// The whole rail. Rebuilt only by a full remount.
    pub sidebar: ComponentRef,
    /// The rail's scrolling list, rebuilt for `Dirty::sidebar`.
    pub sidebar_list: ComponentRef,
    /// The rail's footer with the settings and the bell, also rebuilt for
    /// `Dirty::sidebar`.
    pub sidebar_footer: ComponentRef,
    /// The rail's Settings button, which takes focus back from the
    /// settings it opened.
    pub settings_button: ComponentRef,
    /// The rail's search field.
    pub rail_search: ComponentRef,
    /// The rail's Projects and Agents tabs, rebuilt for `Dirty::sidebar`.
    pub rail_tabs: ComponentRef,
    pub tabs: ComponentRef,
    /// The shown tab in the strip, which takes focus for `Dirty::focus_tabs`.
    pub active_tab: ComponentRef,
    pub status: ComponentRef,
    /// The command palette overlay. Empty while the palette is closed.
    pub palette: ComponentRef,
    /// The palette's result list, rebuilt alone while the user types.
    pub palette_list: ComponentRef,
    /// The palette's text input, which takes focus when the overlay opens.
    pub palette_input: ComponentRef,
    /// The settings overlay. Empty while closed.
    pub settings: ComponentRef,
    /// The selected section row, which takes focus when the settings open.
    pub settings_focus: ComponentRef,
    /// The approval overlay. Empty while no sandbox request waits.
    pub approval: ComponentRef,
    /// The approval overlay's Block button, which takes focus.
    pub approval_focus: ComponentRef,
    /// The notification overlay. Empty while closed.
    pub notifications: ComponentRef,
    /// The notification overlay's first button, which takes focus.
    pub notifications_focus: ComponentRef,
    /// The rail's bell, which takes focus back when Escape closes the
    /// notification overlay.
    pub bell: ComponentRef,
    /// The tab strip's `+`, which takes focus back when Escape closes the
    /// new panel list it opened.
    pub new_panel: ComponentRef,
    /// The host setup overlay. Empty while closed.
    pub setup: ComponentRef,
    /// The host setup overlay's Done button, which takes focus.
    pub setup_focus: ComponentRef,
    pub panel: ComponentRef,
    pub focus: ComponentRef,
    /// The title strip of each split pane, in layout order, rebuilt alone
    /// for `Dirty::tabs` when a panel's title changes.
    pub pane_titles: [ComponentRef; MAX_PANES],
}

/// The input a folder from the native folder chooser goes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderTarget {
    /// The command palette's project input.
    Palette,
    /// The folder input on the settings' Project page.
    ProjectSandbox,
}

/// A folder from the native folder chooser and the input it goes into.
pub type PickedFolder = (FolderTarget, PathBuf);

pub struct Inner {
    pub state: AppState,
    store: StateStore,
    pub snapshots: HashMap<u64, Snapshot>,
    pub errors: HashMap<u64, String>,
    /// Running panels per project, then per worktree path. Nested so lookups
    /// borrow a `&Path` instead of building a key.
    pub(crate) live: HashMap<u64, HashMap<PathBuf, LiveWorktree>>,
    next_panel: u64,
    pub dirty: Dirty,
    pub notices: VecDeque<Notice>,
    /// Raw project inputs the worker is still discovering.
    pub pending_projects: Vec<String>,
    /// Fetches, pulls and pushes the worker runs, one per worktree at most.
    remote_runs: Vec<RemoteRun>,
    /// The ones of `pending_projects` that open as local sessions.
    temporary_inputs: Vec<String>,
    pub ext: ExtensionHost,
    requests: Vec<Request>,
    vcs: VcsWorker,
    pub(crate) wake: Arc<OnceLock<RedrawSignal>>,
    pub metrics: TerminalCellMetrics,
    /// Measures terminal fonts and lists installed families. Cold use only.
    pub shaper: TextShaper,
    pub settings: Settings,
    settings_store: SettingsStore,
    /// The color scheme every view draws with, resolved from the settings.
    pub scheme: &'static Scheme,
    /// The strings every view shows, in the language the settings pick.
    pub tr: &'static Strings,
    /// Whether the desktop prefers dark. Written by the preference watcher
    /// thread, read once per poll.
    system_dark: Arc<AtomicBool>,
    /// Settings overlay state: whether it is open and which section.
    pub settings_ui: SettingsState,
    /// The shells this machine has, as [`cmdterm_env::installed_shells`]
    /// found them. Empty on systems other than Windows.
    pub shells: Vec<cmdterm_env::ShellOption>,
    /// The keybindings in effect and the key sequence typed so far.
    pub keymap: Keymap,
    /// The extensions' commands revision the keymap was built from.
    pub keymap_revision: u64,
    /// The user's own extensions, in the configuration directory.
    pub extensions_dir: PathBuf,
    /// The shipped extensions, in the data directory. The same as
    /// `extensions_dir` with `--config-dir`.
    pub shipped_dir: PathBuf,
    /// Where the app keeps its files.
    pub dirs: cmdterm_env::AppDirs,
    /// Columns and rows available to the active panel.
    pub grid: (u16, u16),
    /// Command palette state: whether it is open, its mode, filter and highlight.
    pub palette: PaletteState,
    /// Folders chosen in the native folder chooser, sent from its thread
    /// with the input they go into.
    pub picked_folders: (mpsc::Sender<PickedFolder>, mpsc::Receiver<PickedFolder>),
    pub regions: Regions,
    /// Logical size of the active panel's content area.
    pub panel_size: schnellui::scene::Size,
    /// Content size of each pane of the shown worktree, in pane order.
    /// Refreshed with the terminal sizes after every layout change.
    pub pane_sizes: Vec<Size>,
    /// What is being dragged: the tab or the pane of a panel.
    dragging: Option<Dragged>,
    /// What the drag in progress carries and where it may land, shown in
    /// the status bar in place of a notice until it ends.
    pub drag_hint: Option<String>,
    /// Where the last `focus_tabs` sent focus, while it stays in the chrome.
    chrome_step: Option<ChromeStep>,
    /// Set while a browser panel is shown, so the ticker keeps Servo spinning.
    #[cfg(feature = "browser")]
    browser_visible: Arc<std::sync::atomic::AtomicBool>,
    /// Devcontainer phases and background jobs for worktree runtimes.
    pub(crate) runtimes: crate::runtime::Runtimes,
    /// Sum of every terminal's attention revision at the last poll. A change
    /// means some program started or stopped waiting for the user.
    attention_revision: u64,
    /// ssh executable given to newly added remote projects, such as `tsh ssh`.
    /// Starts from `SCRIPTSCHNELL_TERM_SSH_PROGRAM`, or the older
    /// `CMDTERM_SSH_PROGRAM`.
    ssh_program: Option<String>,
    /// This machine served to others from inside the app.
    pub(crate) serving: Option<crate::machines::Serving>,
    /// Asks fused remote machines for their projects.
    pub(crate) fusion: crate::fusion::Fusion,
    /// Recent notices and update check results, behind the rail's bell.
    pub notifications: crate::notifications::Notifications,
    /// Whether the notification overlay is open.
    pub notifications_open: bool,
    /// The setup overlay, while it is open.
    pub setup: Option<crate::ui::Setup>,
    /// When the next extension task is due.
    pub(crate) tasks: crate::tasks::Schedule,
    /// The control socket, started with the first terminal that gets it.
    /// `Some(None)` when it could not start.
    control: Option<Option<crate::control::Server>>,
    /// The control socket's revision the approval overlay last showed.
    control_seen: u64,
    /// Allowed launches waiting for their new worktree, by project.
    pub(crate) launches: Vec<(u64, crate::control::Launch)>,
    /// Projects whose launches pass without a question until the app quits,
    /// from the launch dialog's "Allow in this session".
    pub(crate) launches_allowed: Vec<u64>,
    /// Per SSH host, whether it forwards the control socket: `None` while
    /// the check runs. See [`Inner::forwards_to`].
    forwarding: Arc<Mutex<HashMap<Environment, Option<bool>>>>,
    /// The debug browser's status and title revisions the UI last showed.
    debug_status_seen: u64,
    debug_title_seen: u64,
    /// The extension tools revision last published.
    ext_tools_seen: u64,
    /// The extension agents revision last handed to `agents::set_custom`.
    ext_agents_seen: u64,
    /// The text in the rail's search field. While it has words the rail
    /// lists the worktrees that match instead of every project.
    pub rail_query: String,
    /// The highlighted result, which Enter opens.
    pub rail_highlight: usize,
    /// Where focus goes after [`Dirty::search`] rebuilds the field: into
    /// it, or back to the panel.
    pub rail_search_focus: bool,
    /// The rail is hidden, so the panel takes the whole width.
    pub rail_hidden: bool,
    /// The window got too narrow for the rail and hid it; widening the
    /// window shows it again, unless the user toggled it meanwhile.
    pub rail_auto_hidden: bool,
    /// The first tab the tab strip's window showed last, so choosing a
    /// tab it already shows does not slide the strip under the pointer.
    pub tab_window: std::cell::Cell<Option<usize>>,
    /// What the rail lists: projects or agents.
    pub rail_tab: RailTab,
    /// Whether the Agents tab shows its working worktrees, its merged ones
    /// and its earlier sessions.
    pub agents_expanded: [bool; 3],
    /// Forge accounts and what forges reported about agent branches.
    pub forges: crate::forges::Forges,
    /// Per-project environment variables, backed by the system keyring when
    /// available.
    pub(crate) project_env: crate::project_env::ProjectEnvStore,
    /// Set when a frozen agent finished resuming, see
    /// [`TerminalPanel::thaw`].
    pub thaw_signal: Arc<AtomicBool>,
    /// Set every half minute, when idle agents in hidden terminals may be
    /// frozen, and every few seconds while memory runs low.
    pub freeze_check: Arc<AtomicBool>,
    /// This machine's [`crate::memory::Pressure`], which the freeze thread
    /// samples.
    pub memory_pressure: Arc<std::sync::atomic::AtomicU8>,
}

#[derive(Clone)]
pub struct Workspace(Rc<RefCell<Inner>>);

impl Workspace {
    /// [`Self::with_dirs`] with everything in the parent of `extensions_dir`,
    /// the layout of `--config-dir`.
    #[cfg(test)]
    pub fn new(store: StateStore, settings_store: SettingsStore, extensions_dir: &Path) -> Self {
        let dir = extensions_dir.parent().unwrap_or(extensions_dir);
        Self::with_dirs(store, settings_store, &cmdterm_env::AppDirs::under(dir))
    }

    /// A workspace with the saved state and settings. Extensions load from
    /// the shipped ones in the data directory and the user's own in the
    /// configuration directory; the ones named in the settings'
    /// `disabled_extensions` are listed but not loaded.
    pub fn with_dirs(
        store: StateStore,
        settings_store: SettingsStore,
        dirs: &cmdterm_env::AppDirs,
    ) -> Self {
        let extensions_dir = dirs.config.join("extensions");
        let shipped_dir = dirs.data.join("extensions");
        let wake: Arc<OnceLock<RedrawSignal>> = Arc::new(OnceLock::new());
        let worker_wake = wake.clone();
        let vcs = VcsWorker::spawn(move || {
            if let Some(signal) = worker_wake.get() {
                signal.request_redraw();
            }
        });
        let runtimes = crate::runtime::Runtimes::new(dirs, wake.clone());
        let mut notices = VecDeque::new();
        let state = store.load();
        let settings = settings_store.load();
        let tr = Lang::resolve(settings.as_ref().map_or("", |s| s.language.as_str())).strings();
        let state = match state {
            Ok(state) => state,
            Err(error) => {
                notices.push_back(Notice::new((tr.state_not_loaded)(&error.to_string())));
                AppState::default()
            }
        };
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => {
                notices.push_back(Notice::new((tr.settings_not_loaded)(&error.to_string())));
                Settings::default()
            }
        };
        let mut shaper = TextShaper::new();
        let metrics = measure(&mut shaper, &settings);
        let system_dark = Arc::new(AtomicBool::new(true));
        let scheme = resolve_scheme(&settings.color_scheme, true);
        let mut ext = extension_host(dirs, &settings);
        let errors = ext.load_dirs_except(
            &extension_dirs(&shipped_dir, &extensions_dir),
            &settings.disabled_extensions,
            &granted_permissions(&settings),
            &setting_values(&settings),
        );
        for error in worth_a_notice(&ext, errors) {
            notices.push_back(Notice::new(error.to_string()));
        }
        // Before any panel opens, so saved sessions of extension agents
        // resume.
        for error in crate::agents::set_custom(ext.owner(), ext.agents()) {
            notices.push_back(Notice::new(error.to_string()));
        }
        let (keymap, errors) =
            Keymap::build(&settings.keymap, ext.commands(), &settings.keybindings);
        notices.extend(errors.into_iter().map(Notice::new));
        let keymap_revision = ext.commands_revision();
        let forges = crate::forges::Forges::new(wake.clone(), dirs.cache.join("change-requests"));
        let project_env = crate::project_env::ProjectEnvStore::new(
            Arc::new(crate::project_env::OsSecretBackend::new(
                !cfg!(test) && !dirs.is_single(),
            )),
            wake.clone(),
        );
        let mut inner = Inner {
            state,
            store,
            snapshots: HashMap::new(),
            errors: HashMap::new(),
            live: HashMap::new(),
            attention_revision: 0,
            next_panel: 0,
            dirty: Dirty::default(),
            notices,
            pending_projects: Vec::new(),
            remote_runs: Vec::new(),
            temporary_inputs: Vec::new(),
            ext,
            requests: Vec::new(),
            vcs,
            wake,
            metrics,
            shaper,
            settings,
            settings_store,
            scheme,
            tr,
            system_dark,
            settings_ui: SettingsState::default(),
            shells: cmdterm_env::installed_shells().to_vec(),
            keymap,
            keymap_revision,
            extensions_dir,
            shipped_dir,
            dirs: dirs.clone(),
            runtimes,
            grid: (80, 24),
            palette: PaletteState::default(),
            picked_folders: mpsc::channel(),
            regions: Regions {
                sidebar: ComponentRef::new(),
                sidebar_list: ComponentRef::new(),
                sidebar_footer: ComponentRef::new(),
                settings_button: ComponentRef::new(),
                rail_search: ComponentRef::new(),
                rail_tabs: ComponentRef::new(),
                tabs: ComponentRef::new(),
                active_tab: ComponentRef::new(),
                status: ComponentRef::new(),
                palette: ComponentRef::new(),
                palette_list: ComponentRef::new(),
                palette_input: ComponentRef::new(),
                settings: ComponentRef::new(),
                settings_focus: ComponentRef::new(),
                approval: ComponentRef::new(),
                approval_focus: ComponentRef::new(),
                notifications: ComponentRef::new(),
                notifications_focus: ComponentRef::new(),
                bell: ComponentRef::new(),
                new_panel: ComponentRef::new(),
                setup: ComponentRef::new(),
                setup_focus: ComponentRef::new(),
                panel: ComponentRef::new(),
                focus: ComponentRef::new(),
                pane_titles: std::array::from_fn(|_| ComponentRef::new()),
            },
            panel_size: schnellui::scene::Size {
                width: 640.0,
                height: 480.0,
            },
            pane_sizes: Vec::new(),
            dragging: None,
            drag_hint: None,
            chrome_step: None,
            #[cfg(feature = "browser")]
            browser_visible: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ssh_program: ["SCRIPTSCHNELL_TERM_SSH_PROGRAM", "CMDTERM_SSH_PROGRAM"]
                .into_iter()
                .find_map(|var| std::env::var(var).ok().filter(|p| !p.is_empty())),
            serving: None,
            fusion: crate::fusion::Fusion::default(),
            notifications: crate::notifications::Notifications::default(),
            notifications_open: false,
            setup: None,
            tasks: crate::tasks::Schedule::default(),
            control: None,
            control_seen: 0,
            launches: Vec::new(),
            launches_allowed: Vec::new(),
            forwarding: Arc::default(),
            debug_status_seen: 0,
            debug_title_seen: 0,
            ext_tools_seen: 0,
            ext_agents_seen: 0,
            rail_query: String::new(),
            rail_highlight: 0,
            rail_search_focus: false,
            rail_hidden: false,
            rail_auto_hidden: false,
            tab_window: std::cell::Cell::new(None),
            rail_tab: RailTab::Projects,
            agents_expanded: [false; 3],
            forges,
            project_env,
            thaw_signal: Arc::new(AtomicBool::new(false)),
            freeze_check: Arc::new(AtomicBool::new(false)),
            memory_pressure: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        };
        // Tests share one hub and run in parallel; each configures it
        // itself, so no other test's workspace starts a real browser.
        #[cfg(not(test))]
        crate::debug_browser::configure(&inner.settings, &inner.dirs);
        let debug_wake = inner.wake.clone();
        crate::devtools::hub().set_wake(move || {
            if let Some(signal) = debug_wake.get() {
                signal.request_redraw();
            }
        });
        #[cfg(feature = "browser")]
        {
            let tick_wake = inner.wake.clone();
            crate::browser::engine::spawn_ticker(inner.browser_visible.clone(), move || {
                if let Some(signal) = tick_wake.get() {
                    signal.request_redraw();
                }
            });
        }
        if !inner.notices.is_empty() {
            arm_notice_expiry(inner.wake.clone());
        }
        for notice in &inner.notices {
            inner.notifications.push(
                crate::notifications::Topic::Notice(notice.text.clone()),
                true,
            );
        }
        // The Seatbelt probe runs `sandbox-exec` on its own thread, so the
        // first sandboxed terminal does not wait for it. Tests probe on use.
        if !cfg!(test) {
            cmdterm_env::warm_seatbelt_probe();
        }
        // Tests never ask npm or mise.
        if inner.settings.check_updates && !cfg!(test) {
            inner.notifications.schedule(inner.wake.clone());
        }
        // The hooks of integrations an older version installed call the
        // current helper from now on. Tests never touch the real home.
        if !cfg!(test) {
            let _ = std::thread::Builder::new()
                .name("cmdterm-agent-scripts".into())
                .spawn(|| crate::agents::refresh_scripts(&Environment::Local));
        }
        // Tests freeze agents themselves.
        if !cfg!(test) {
            let (check, wake) = (inner.freeze_check.clone(), inner.wake.clone());
            let pressure = inner.memory_pressure.clone();
            let _ = std::thread::Builder::new()
                .name("cmdterm-agent-freeze".into())
                .spawn(move || {
                    use crate::memory::{Monitor, Pressure, SAMPLE_EVERY};
                    let mut monitor = Monitor::default();
                    let mut last_check = std::time::Instant::now();
                    loop {
                        std::thread::sleep(SAMPLE_EVERY);
                        let now = std::time::Instant::now();
                        let level = monitor.sample(now);
                        pressure.store(level as u8, Ordering::Release);
                        // Low memory checks at every sample, so idle agents
                        // freeze before the machine starts swapping.
                        let due = level > Pressure::Normal
                            || now.duration_since(last_check) >= Duration::from_secs(30);
                        if !due {
                            continue;
                        }
                        last_check = now;
                        check.store(true, Ordering::Release);
                        if let Some(signal) = wake.get() {
                            signal.request_redraw();
                        }
                    }
                });
        }
        // Before the first terminals start.
        inner.apply_default_shell();
        // A local project saved before its git directory was recorded gets
        // it now, before any sandbox starts and could replace its `.git`.
        // Another machine's project waits for its first poll: its sandbox
        // pins `.git` as a directory meanwhile, see `runtime.rs`.
        let mut recorded = false;
        for project in &mut inner.state.projects {
            if project.git_dir.is_none() && !project.is_global() && project.env.is_local() {
                project.git_dir = crate::vcs::local_common_dir(&project.root);
                recorded |= project.git_dir.is_some();
            }
        }
        let workspace = Self(Rc::new(RefCell::new(inner)));
        if recorded {
            workspace.save();
        }
        workspace.emit(&Event::AppStarted);
        workspace.warn_unscoped_host_network();
        let active = workspace.0.borrow().state.active_project;
        {
            let mut inner = workspace.0.borrow_mut();
            let inner = &mut *inner;
            inner.project_env.start_initial_loads(&inner.state);
            if let Some(id) = active {
                inner
                    .project_env
                    .wait_for_project(id, crate::project_env::STARTUP_WAIT);
            }
            for project in &inner.state.projects {
                if Some(project.id) != active && !project.is_global() {
                    inner.vcs.send(Job::Refresh {
                        project: project.id,
                        env: project.env.clone(),
                        root: project.root.clone(),
                        jj: project.uses_jj(inner.settings.jj),
                        known: project.known_repo(),
                    });
                }
            }
        }
        if let Some(id) = active {
            workspace.select_project(id);
        }
        workspace.resume_working_agents();
        workspace
    }

    /// Starts the saved panels of every worktree but the shown one whose
    /// agents were in the middle of a turn when the app closed, so they go
    /// on in the background instead of waiting until their worktree shows.
    fn resume_working_agents(&self) {
        {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            let shown = inner.active_key();
            let working = |config: &crate::state::WorktreeConfig| {
                config.panels.iter().any(|spec| {
                    matches!(spec, PanelSpec::Terminal { agent: Some(agent), .. }
                        if agent.working && agent.session.is_some())
                })
            };
            let due: Vec<(u64, crate::state::WorktreeConfig)> = (inner.state.projects.iter())
                .flat_map(|project| {
                    (project.worktrees.iter())
                        .filter(|config| working(config))
                        .map(|config| (project.id, config.clone()))
                })
                .filter(|(id, config)| shown.as_ref() != Some(&(*id, config.path.clone())))
                .collect();
            if due.is_empty() {
                return;
            }
            // Their terminals get the project's variables from the keyring
            // too, as the shown project's do.
            for (id, _) in &due {
                (inner.project_env).wait_for_project(*id, crate::project_env::STARTUP_WAIT);
            }
            for (id, config) in &due {
                inner.start_live(*id, &config.path, config);
            }
        }
        self.batch_hidden_sessions();
    }

    /// Wakes the UI thread from a worker thread.
    pub fn redraw_handle(&self) -> Arc<OnceLock<RedrawSignal>> {
        self.0.borrow().wake.clone()
    }

    /// Installs the window's redraw signal. Worker threads use it from now on.
    pub fn set_redraw_signal(&self, signal: RedrawSignal) {
        let _ = self.0.borrow().wake.set(signal);
    }

    pub fn borrow(&self) -> std::cell::Ref<'_, Inner> {
        self.0.borrow()
    }

    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, Inner> {
        self.0.borrow_mut()
    }

    pub fn regions(&self) -> Regions {
        self.0.borrow().regions
    }

    pub fn take_dirty(&self) -> Dirty {
        std::mem::take(&mut self.0.borrow_mut().dirty)
    }

    pub fn mark(&self, dirty: Dirty) {
        let mut inner = self.0.borrow_mut();
        inner.dirty.sidebar |= dirty.sidebar;
        inner.dirty.main |= dirty.main;
        inner.dirty.tabs |= dirty.tabs;
        inner.dirty.status |= dirty.status;
        inner.dirty.palette |= dirty.palette;
        inner.dirty.palette_list |= dirty.palette_list;
        inner.dirty.settings |= dirty.settings;
        inner.dirty.notifications |= dirty.notifications;
        inner.dirty.full |= dirty.full;
    }

    #[cfg(test)]
    pub fn push_internal_notice(&self, text: &str) {
        self.0.borrow_mut().push_notice(text.into());
    }

    /// The strings of the language in effect.
    pub fn tr(&self) -> &'static Strings {
        self.0.borrow().tr
    }

    pub fn notify(&self, message: impl Into<String>) {
        self.0.borrow_mut().push_notice(message.into());
    }

    /// Shows what the sandboxes of this process had to go without, such
    /// as Seatbelt rule groups that `sandbox-exec` turned down. Cold: it
    /// runs once per warning.
    fn report_sandbox_warnings(&self) {
        let mut inner = self.0.borrow_mut();
        for warning in cmdterm_env::take_sandbox_warnings() {
            let text = match warning {
                cmdterm_env::SandboxWarning::SeatbeltRulesDropped(groups) => {
                    (inner.tr.sandbox_rules_dropped)(&groups.join(", "))
                }
                cmdterm_env::SandboxWarning::ShellReplaced { shell, with } => {
                    (inner.tr.sandbox_shell_replaced)(&shell, with)
                }
                cmdterm_env::SandboxWarning::ClaudeWithoutGitBash => {
                    inner.tr.sandbox_claude_without_bash.to_owned()
                }
            };
            inner.push_notice(text);
        }
    }

    /// Drops the notices once the newest one is older than [`NOTICE_TTL`].
    pub fn expire_notices(&self, now: Instant) {
        self.0.borrow_mut().expire_notices(now);
    }

    // --- projects and worktrees ---------------------------------------------

    /// Parses `path`, `~/path`, `host:/path`, `ssh://host/path`, a VPN
    /// location such as `tailscale://box/path` or a location in an
    /// extension's environment such as `coder://ws/path`, and asks the
    /// worker to discover the repository there.
    pub fn add_project(&self, input: &str) {
        if let Some(location) = crate::vpn::VpnLocation::parse(input) {
            let mut inner = self.0.borrow_mut();
            let scan = crate::vpn::ScanConfig {
                nebula_config: inner.settings.vpn.nebula_config.clone().map(PathBuf::from),
            };
            let ssh_program = inner.ssh_program.clone();
            inner.pending_projects.push(input.to_owned());
            inner.dirty.sidebar = true;
            inner.vcs.send(Job::DiscoverVpn {
                input: input.to_owned(),
                location: Box::new(location),
                scan,
                ssh_program,
                jj: inner.settings.jj,
            });
            return;
        }
        let Some((mut env, path)) = parse_location(input) else {
            self.notify((self.tr().not_a_path)(input));
            return;
        };
        if let (Environment::Ssh(target), Some(program)) = (&mut env, &self.0.borrow().ssh_program)
        {
            target.program = Some(program.clone());
        }
        {
            let mut inner = self.0.borrow_mut();
            inner.pending_projects.push(input.to_owned());
            inner.dirty.sidebar = true;
            let jj = inner.settings.jj;
            inner.vcs.send(Job::Discover {
                input: input.to_owned(),
                env,
                path,
                jj,
            });
        }
    }

    /// Clones `url` into a new directory under `parent` in `env` and adds
    /// it. With `devcontainer`, its terminals run in the checkout's
    /// devcontainer.
    pub fn clone_project(&self, url: &str, env: Environment, parent: PathBuf, devcontainer: bool) {
        let mut inner = self.0.borrow_mut();
        inner.pending_projects.push(url.to_owned());
        inner.dirty.sidebar = true;
        inner.vcs.send(Job::Clone {
            input: url.to_owned(),
            env,
            url: url.to_owned(),
            parent,
            devcontainer,
            jj: inner.settings.jj,
        });
    }

    /// [`Self::add_project`] as a local session: the project shows like any
    /// other until the app quits, but is never saved.
    pub fn add_temporary_project(&self, input: &str) {
        self.0.borrow_mut().temporary_inputs.push(input.to_owned());
        self.add_project(input);
        let mut inner = self.0.borrow_mut();
        // Not queued: the input did not parse.
        if !inner.pending_projects.iter().any(|p| p == input) {
            inner.temporary_inputs.retain(|p| p != input);
        }
    }

    /// Opens a global session in `input`, a location as
    /// [`Self::add_project`] takes it but without a repository. Empty is
    /// this machine's home directory, and `host:` the home on `host`.
    pub fn open_global_session(&self, input: &str) {
        let input = input.trim();
        let location = match input {
            "" => "~".to_owned(),
            host if host.ends_with(':') && !host.contains('/') => format!("{host}~"),
            input => input.to_owned(),
        };
        let Some((mut env, path)) = parse_location(&location) else {
            self.notify((self.tr().not_a_path)(input));
            return;
        };
        let mut inner = self.0.borrow_mut();
        if let (Environment::Ssh(target), Some(program)) = (&mut env, &inner.ssh_program) {
            target.program = Some(program.clone());
        }
        inner.pending_projects.push(location.clone());
        inner.dirty.sidebar = true;
        inner.vcs.send(Job::OpenGlobal {
            input: location,
            env,
            path,
        });
    }

    /// Keeps a local or global session: it is saved from now on and comes
    /// back on the next start.
    pub fn keep_project(&self, id: u64) {
        {
            let mut inner = self.0.borrow_mut();
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            project.temporary = false;
            inner.dirty.sidebar = true;
        }
        self.save();
    }

    /// Sets the ssh executable for remote projects added from now on.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn set_ssh_program(&self, program: Option<String>) {
        self.0.borrow_mut().ssh_program = program;
    }

    pub fn remove_project(&self, id: u64) {
        self.hide_fused(id);
        self.drop_project(id);
    }

    /// Takes a project out of the sidebar and ends its terminals.
    pub(crate) fn drop_project(&self, id: u64) {
        {
            let mut inner = self.0.borrow_mut();
            if let Some(project) = inner.state.project(id).cloned() {
                inner.project_env.forget_project(&project);
            }
            inner.state.remove_project(id);
            inner.runtimes.forget_project(id);
            for live in inner
                .live
                .remove(&id)
                .into_iter()
                .flat_map(|w| w.into_values())
            {
                live.end_sessions();
            }
            inner.snapshots.remove(&id);
        }
        if let Some(next) = self.0.borrow().state.active_project {
            self.select_project(next);
        }
        self.mark(Dirty {
            sidebar: true,
            main: true,
            ..Dirty::default()
        });
        self.save();
    }

    pub fn select_project(&self, id: u64) {
        let target = {
            let mut inner = self.0.borrow_mut();
            let Some(project) = inner.state.project(id) else {
                return;
            };
            let job = watch(project, inner.settings.jj);
            let worktree = project.current_worktree().to_path_buf();
            inner.vcs.send(job);
            inner.state.active_project = Some(id);
            worktree
        };
        self.activate_worktree(id, &target);
    }

    /// Shows `path` of project `id`: saves the panels of the worktree being
    /// left, and starts the saved panels of `path` if they are not running.
    /// Switching to another project's worktree moves the VCS watch to it.
    pub fn activate_worktree(&self, id: u64, path: &Path) {
        let event = {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            let switching = inner.state.active_project != Some(id);
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            if switching {
                inner.vcs.send(watch(project, inner.settings.jj));
                inner.settings_ui.machines.form.project_env_value.clear();
                inner.settings_ui.machines.form.project_env_error = None;
            }
            project.active_worktree = Some(path.to_path_buf());
            let env = project.env.clone();
            let name = project.name.clone();
            let project_id = project_storage_id(&name, &env, &project.root);
            let fresh = project.worktree(path).is_none();
            let chosen = project.runtime;
            let config = project.worktree_mut(path);
            if fresh {
                config.runtime =
                    new_worktree_runtime(chosen, inner.settings.default_runtime, &env, path);
            }
            let config = config.clone();
            inner.state.active_project = Some(id);
            inner.start_live(id, path, &config);
            inner.dirty.sidebar = true;
            inner.dirty.main = true;
            let branch = inner.snapshots.get(&id).and_then(|snapshot| {
                snapshot
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.path == path)
                    .and_then(|worktree| worktree.branch.clone())
            });
            inner.ext.set_context(ExtContext {
                project: Some(name.clone()),
                project_id: Some(project_id.clone()),
                worktree: Some(path.to_path_buf()),
                branch: branch.clone(),
                env: env.label().to_owned(),
                runtime: crate::runtime::runtime_name(config.runtime).to_owned(),
            });
            Event::WorktreeActivated {
                project: name,
                project_id,
                path: path.to_path_buf(),
                branch,
                env: env.label().to_owned(),
            }
        };
        self.resize_active();
        self.emit(&event);
        self.announce_project_env(id, path);
        self.save();
    }

    /// Creates a worktree for `branch` next to the main worktree, named
    /// `<repo>-<branch>`, creating the branch if it does not exist.
    pub fn add_worktree(&self, branch: &str) {
        let Some(id) = self.0.borrow().state.active_project else {
            return;
        };
        let branch = branch.trim();
        if !branch.is_empty() {
            self.add_worktree_in(id, Some(branch), None, None);
        }
    }

    /// Creates a worktree named `<repo>-NAME` next to the main worktree of
    /// project `id`. `branch` is checked out, created at `from` when
    /// missing; without it, the worktree is detached at `from`. `from`
    /// defaults to the main worktree's revision, `name` to the branch with
    /// `-` for `/`, else to `from`. Returns false when the project's
    /// repository is not loaded yet.
    pub fn add_worktree_in(
        &self,
        id: u64,
        branch: Option<&str>,
        from: Option<&str>,
        name: Option<&str>,
    ) -> bool {
        let inner = self.0.borrow();
        let Some(snapshot) = inner.snapshots.get(&id) else {
            drop(inner);
            self.notify(self.tr().repository_not_loaded);
            return false;
        };
        let repo = snapshot.repo.clone();
        let name = match (name, branch) {
            (Some(name), _) => name.to_owned(),
            (None, Some(branch)) => branch.replace('/', "-"),
            (None, None) => from.map_or_else(|| "detached".to_owned(), folder_name),
        };
        let path = sibling_path(&repo.root, &name);
        inner.vcs.send(Job::AddWorktree {
            project: id,
            repo,
            path,
            branch: branch.map(str::to_owned),
            from: from.map(str::to_owned),
        });
        true
    }

    /// Creates a worktree named `<repo>-<name>` next to the main worktree,
    /// at the revision checked out in `source`. A non-empty `branch` is
    /// checked out (created when missing); an empty one leaves it detached.
    pub fn add_worktree_from(&self, id: u64, source: &Path, name: &str, branch: &str) {
        let name = name.trim();
        let branch = branch.trim();
        if name.is_empty() {
            return;
        }
        if name.contains('/') || name == "." || name == ".." {
            self.notify((self.tr().bad_worktree_name)(name));
            return;
        }
        let inner = self.0.borrow();
        let Some(snapshot) = inner.snapshots.get(&id) else {
            drop(inner);
            self.notify(self.tr().repository_not_loaded);
            return;
        };
        let repo = snapshot.repo.clone();
        let path = sibling_path(&repo.root, name);
        inner.vcs.send(Job::AddWorktreeFrom {
            project: id,
            repo,
            source: source.to_path_buf(),
            path,
            branch: (!branch.is_empty()).then(|| branch.to_owned()),
        });
    }

    /// Removes worktree `path` of project `id` and ends its terminals. A
    /// project showing it switches to its main worktree first.
    pub fn remove_worktree(&self, id: u64, path: &Path) {
        let root = {
            let inner = self.0.borrow();
            let Some(project) = inner.state.project(id) else {
                return;
            };
            if project.root == path {
                return;
            }
            (project.current_worktree() == path).then(|| project.root.clone())
        };
        if let Some(root) = root {
            if self.0.borrow().state.active_project == Some(id) {
                self.activate_worktree(id, &root);
            } else if let Some(project) = self.0.borrow_mut().state.project_mut(id) {
                project.active_worktree = None;
            }
        }
        let mut inner = self.0.borrow_mut();
        let Some(snapshot) = inner.snapshots.get(&id) else {
            return;
        };
        let repo = snapshot.repo.clone();
        if let Some(live) = inner.live.get_mut(&id).and_then(|w| w.remove(path)) {
            live.end_sessions();
        }
        inner.vcs.send(Job::RemoveWorktree {
            project: id,
            repo,
            path: path.to_path_buf(),
        });
    }

    /// Fetches, pulls or pushes in worktree `path` of project `id` on the
    /// worker. A notice says when it starts and another how it ended, then
    /// the project is read again. While one runs in a worktree, another
    /// one there is refused with a notice.
    pub fn run_remote(&self, id: u64, path: &Path, op: RemoteOp) {
        let mut inner = self.0.borrow_mut();
        let tr = inner.tr;
        let Some(snapshot) = inner.snapshots.get(&id) else {
            inner.push_notice(tr.repository_not_loaded.to_owned());
            return;
        };
        let repo = snapshot.repo.clone();
        let label = match op {
            RemoteOp::Fetch => inner.state.project(id).map(|p| p.name.clone()),
            RemoteOp::Pull | RemoteOp::Push(_) => (snapshot.worktrees.iter())
                .find(|worktree| worktree.path == path)
                .map(crate::ui::branch_name),
        }
        .unwrap_or_else(|| crate::search::folder_name(path));
        if (inner.remote_runs.iter()).any(|run| run.project == id && run.path == path) {
            inner.push_notice((tr.vcs_busy)(&label));
            return;
        }
        let started = match &op {
            RemoteOp::Fetch => (tr.vcs_fetching)(&label),
            RemoteOp::Pull => (tr.vcs_pulling)(&label),
            RemoteOp::Push(_) => (tr.vcs_pushing)(&label),
        };
        inner.push_notice(started);
        inner.remote_runs.push(RemoteRun {
            project: id,
            path: path.to_path_buf(),
            label,
        });
        inner.vcs.send(Job::Remote {
            project: id,
            repo,
            path: path.to_path_buf(),
            op,
        });
    }

    // --- panels ---------------------------------------------------------------

    pub fn open_terminal(&self, command: Option<Vec<String>>, title: Option<String>) {
        // Start where the active shell says it is (OSC 7).
        let cwd = self.active_terminal().and_then(|t| t.current_dir());
        self.open(PanelSpec::Terminal {
            title,
            command,
            cwd,
            session: None,
            control: None,
            agent: None,
            once: Vec::new(),
        });
    }

    /// Opens a terminal that runs `command` with `once` after it, and
    /// returns it. The layout saves `command` alone, so a restart runs it
    /// without `once`, or resumes the agent it started.
    pub fn open_terminal_once(
        &self,
        command: Vec<String>,
        once: Vec<String>,
        title: String,
    ) -> Option<Rc<TerminalPanel>> {
        let cwd = self.active_terminal().and_then(|t| t.current_dir());
        let id = self.push_panel(
            PanelSpec::Terminal {
                title: Some(title),
                command: Some(command),
                cwd,
                session: None,
                control: None,
                agent: None,
                once,
            },
            Place::NewTab,
        )?;
        self.resize_active();
        self.save();
        self.with_active(|live| match live?.panel(id)? {
            (_, LivePanel::Terminal(terminal)) => Some(terminal.clone()),
            _ => None,
        })
    }

    /// Opens a terminal in `cwd` (interpreted on the project's machine).
    pub fn open_terminal_in(
        &self,
        cwd: Option<PathBuf>,
        command: Option<Vec<String>>,
        title: Option<String>,
    ) {
        self.open(PanelSpec::Terminal {
            title,
            command,
            cwd,
            session: None,
            control: None,
            agent: None,
            once: Vec::new(),
        });
    }

    /// Stores `snapshot` as if the VCS worker had sent it.
    #[cfg(test)]
    pub fn apply_snapshot_for_test(&self, project: u64, snapshot: crate::vcs_worker::Snapshot) {
        self.apply_reply(crate::vcs_worker::Reply::Snapshot {
            project,
            snapshot: Box::new(snapshot),
        });
    }

    /// Opens the file explorer of the active worktree.
    /// Shows the worktree's file panel, the shown tab's first, or opens one
    /// when it has none, so the key never piles up file tabs.
    pub fn open_files(&self) {
        let found = self.with_active(|live| {
            let live = live?;
            let files = || {
                live.panels
                    .iter()
                    .filter(|(_, panel)| matches!(panel, LivePanel::Files(_)))
                    .map(|(id, _)| *id)
            };
            let shown = live.tabs.get(live.active)?;
            let id = files()
                .find(|id| shown.layout.position(*id).is_some())
                .or_else(|| files().next())?;
            live.tabs
                .iter()
                .enumerate()
                .find_map(|(index, tab)| tab.layout.position(id).map(|pane| (index, pane)))
        });
        let Some((tab, pane)) = found else {
            self.open(PanelSpec::Files { file: None });
            return;
        };
        self.select_tab(tab);
        let mut inner = self.0.borrow_mut();
        if let Some(live) = inner.active_live_mut() {
            live.tab_mut().focused = pane;
        }
        inner.dirty.main = true;
    }

    /// The file panel shown in `pane`, if that pane shows one.
    pub fn pane_files(&self, pane: usize) -> Option<Rc<FilesPanel>> {
        self.with_active(|live| match live?.pane_panel(pane)? {
            (_, LivePanel::Files(files)) => Some(files.clone()),
            _ => None,
        })
    }

    /// Opens the history of the active worktree, or with `review` the
    /// review of its changes against the detected base branch.
    pub fn open_explore(&self, review: bool) {
        // History and review are modes of one panel: an explore panel the
        // worktree has already turns to the mode asked for, so the keys
        // never pile up tabs that show the same switcher.
        let mode = if review {
            ExploreMode::Review
        } else {
            ExploreMode::History
        };
        if let Some(explore) = self.reveal_explore(|explore| explore.mode() == mode) {
            if explore.mode() != mode {
                explore.switch_mode(mode);
            }
            if explore.take_changed() {
                self.save();
            }
            return;
        }
        self.open(PanelSpec::Explore {
            review,
            change: false,
            base: None,
            committed: false,
        });
    }

    /// Shows the explore panel of the active worktree that `prefer` picks,
    /// else one in the shown tab, else any, with its tab shown and its pane
    /// focused. `None` when the worktree has none.
    fn reveal_explore(&self, prefer: impl Fn(&ExplorePanel) -> bool) -> Option<Rc<ExplorePanel>> {
        let (tab, pane, explore) = self.with_active(|live| {
            let live = live?;
            let explores = || {
                live.panels.iter().filter_map(|(id, panel)| match panel {
                    LivePanel::Explore(explore) => Some((*id, explore.clone())),
                    _ => None,
                })
            };
            let shown = live.tabs.get(live.active)?;
            let (id, explore) = explores()
                .find(|(id, explore)| prefer(explore) && shown.layout.position(*id).is_some())
                .or_else(|| explores().find(|(id, _)| shown.layout.position(*id).is_some()))
                .or_else(|| explores().find(|(_, explore)| prefer(explore)))
                .or_else(|| explores().next())?;
            let (tab, pane) = live
                .tabs
                .iter()
                .enumerate()
                .find_map(|(index, tab)| tab.layout.position(id).map(|pane| (index, pane)))?;
            Some((tab, pane, explore))
        })?;
        self.select_tab(tab);
        let mut inner = self.0.borrow_mut();
        if let Some(live) = inner.active_live_mut() {
            live.tab_mut().focused = pane;
        }
        inner.dirty.main = true;
        drop(inner);
        Some(explore)
    }

    /// Shows the change request of the active worktree: on its explore
    /// panel that shows it already, else on the first explore panel of the
    /// shown tab, else on a new one.
    pub fn open_change(&self) {
        let found = self.with_active(|live| {
            let live = live?;
            let explores = || {
                live.panels.iter().filter_map(|(id, panel)| match panel {
                    LivePanel::Explore(explore) => Some((*id, explore.clone())),
                    _ => None,
                })
            };
            let tab = live.tabs.get(live.active)?;
            let (id, explore) = explores()
                .find(|(_, explore)| explore.mode() == ExploreMode::Change)
                .or_else(|| explores().find(|(id, _)| tab.layout.position(*id).is_some()))?;
            let (tab, pane) = live
                .tabs
                .iter()
                .enumerate()
                .find_map(|(index, tab)| tab.layout.position(id).map(|pane| (index, pane)))?;
            Some((tab, pane, explore))
        });
        match found {
            Some((tab, pane, explore)) => {
                self.select_tab(tab);
                let mut inner = self.0.borrow_mut();
                if let Some(live) = inner.active_live_mut() {
                    live.tab_mut().focused = pane;
                }
                inner.dirty.main = true;
                drop(inner);
                explore.show_change();
                if explore.take_changed() {
                    self.save();
                }
            }
            None => self.open(PanelSpec::Explore {
                review: false,
                change: true,
                base: None,
                committed: false,
            }),
        }
    }

    /// The explore panel shown in `pane`, if that pane shows one.
    pub fn pane_explore(&self, pane: usize) -> Option<Rc<ExplorePanel>> {
        self.with_active(|live| match live?.pane_panel(pane)? {
            (_, LivePanel::Explore(explore)) => Some(explore.clone()),
            _ => None,
        })
    }

    /// Opens the monitor of the machine the active worktree's project lives
    /// on.
    pub fn open_monitor(&self) {
        self.open(PanelSpec::Monitor);
    }

    /// Opens the tasks panel: every panel of every worktree, the extension
    /// tasks and the app's threads.
    pub fn open_tasks(&self) {
        self.open(PanelSpec::Tasks);
    }

    /// The tasks panel shown in `pane`, if that pane shows one.
    pub fn pane_tasks(&self, pane: usize) -> Option<Rc<TasksPanel>> {
        self.with_active(|live| match live?.pane_panel(pane)? {
            (_, LivePanel::Tasks(tasks)) => Some(tasks.clone()),
            _ => None,
        })
    }

    /// Hands a shown tasks panel a new snapshot when what it lists changed.
    /// Looks at most once a second, and allocates nothing when nothing
    /// changed.
    fn poll_tasks_panel(&self) {
        let inner = self.0.borrow();
        let Some(live) = inner.active_live() else {
            return;
        };
        let Some(panel) = (0..MAX_PANES).find_map(|pane| match live.pane_panel(pane) {
            Some((_, LivePanel::Tasks(tasks))) => Some(tasks),
            _ => None,
        }) else {
            return;
        };
        let now = Instant::now();
        if !panel.look_due(now) {
            return;
        }
        if !panel.looked_at(now, crate::taskview::fingerprint(&inner, now)) {
            return;
        }
        let panel = panel.clone();
        let snapshot = crate::taskview::snapshot(&inner, now);
        drop(inner);
        panel.set_snapshot(snapshot);
    }

    /// Opens a panel on the app's debug browser.
    pub fn open_debug_browser(&self) {
        self.open(PanelSpec::DebugBrowser);
    }

    /// Opens a debug browser panel unless the shown tab has one, and loads
    /// `url` in the tab it shows.
    pub fn open_in_debug_browser(&self, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        let shown = self.with_active(|live| {
            live.is_some_and(|live| {
                (0..MAX_PANES).any(|pane| {
                    matches!(live.pane_panel(pane), Some((_, LivePanel::DebugBrowser(_))))
                })
            })
        });
        if !shown {
            self.open_debug_browser();
        }
        let schemeless = [
            "about:",
            "data:",
            "file:",
            "blob:",
            "view-source:",
            "chrome:",
        ]
        .iter()
        .any(|scheme| url.starts_with(scheme));
        let url = if schemeless {
            url.to_owned()
        } else {
            normalize_url(url)
        };
        crate::devtools::hub().open(url);
    }

    /// Routes focused input to the debug browser panel in `pane`.
    pub fn debug_browser_input(
        &self,
        pane: usize,
        event: schnellui::FocusedInputEvent,
    ) -> schnellui::FocusedInputResult {
        let size = {
            let inner = self.0.borrow();
            let shows = matches!(
                inner.active_live().and_then(|live| live.pane_panel(pane)),
                Some((_, LivePanel::DebugBrowser(_)))
            );
            if !shows {
                return schnellui::FocusedInputResult::Ignored;
            }
            inner
                .pane_sizes
                .get(pane)
                .copied()
                .unwrap_or(inner.panel_size)
        };
        crate::debug_browser::input(event, size)
    }

    /// Tells the debug browser the size of the pane that shows it, and
    /// marks the panel and tab titles when its status or title changed.
    /// Every frame: compares atomics and allocates nothing.
    fn poll_debug_browser(&self) {
        let hub = crate::devtools::hub();
        let mut inner = self.0.borrow_mut();
        let shown = inner.active_live().and_then(|live| {
            (0..MAX_PANES).find(|pane| {
                matches!(
                    live.pane_panel(*pane),
                    Some((_, LivePanel::DebugBrowser(_)))
                )
            })
        });
        let size = shown.map(|pane| {
            inner
                .pane_sizes
                .get(pane)
                .copied()
                .unwrap_or(inner.panel_size)
        });
        hub.show(size.map(|size| (size.width as u32, size.height as u32)));
        if shown.is_none() {
            return;
        }
        let status = hub.status_revision();
        if status != inner.debug_status_seen {
            inner.debug_status_seen = status;
            inner.dirty.main = true;
            inner.dirty.tabs = true;
        }
        let title = hub.title_revision();
        if title != inner.debug_title_seen {
            inner.debug_title_seen = title;
            inner.dirty.tabs = true;
        }
    }

    /// The monitor panel shown in `pane`, if that pane shows one.
    pub fn pane_monitor(&self, pane: usize) -> Option<Rc<MonitorPanel>> {
        self.with_active(|live| match live?.pane_panel(pane)? {
            (_, LivePanel::Monitor(monitor)) => Some(monitor.clone()),
            _ => None,
        })
    }

    /// Reviews against `base` (the detected default when empty) in the
    /// explore panel of the focused pane, or in a new one.
    pub fn review_against(&self, base: &str) {
        let base = (!base.is_empty()).then(|| base.to_owned());
        match self.pane_explore(self.focused_pane()) {
            Some(explore) => {
                explore.set_base(base);
                explore.take_changed();
                self.save();
            }
            None => self.open(PanelSpec::Explore {
                review: true,
                change: false,
                base,
                committed: false,
            }),
        }
    }

    pub fn open_browser(&self, url: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        self.open(PanelSpec::Browser {
            url: normalize_url(url),
        });
    }

    pub(crate) fn open(&self, spec: PanelSpec) {
        if self.push_panel(spec, Place::NewTab).is_some() {
            self.resize_active();
            self.save();
        }
    }

    /// Starts `spec` in the active worktree and puts it at `place`. Returns
    /// its id.
    fn push_panel(&self, spec: PanelSpec, place: Place) -> Option<u64> {
        let mut inner = self.0.borrow_mut();
        let inner = &mut *inner;
        let (id, path) = inner.active_key()?;
        let vars = inner.worktree_env_vars(id, &path);
        let env = inner.terminal_env(id, &path);
        let panel = inner.open_panel(&env, &path, &spec, &vars)?;
        let panel_id = panel.0;
        let live = inner.live.entry(id).or_default().entry(path).or_default();
        live.panels.push(panel);
        if live.is_blank() {
            // A worktree without panels fills its empty pane.
            *live.tab_mut() = LiveTab::new(panel_id);
        } else {
            match place {
                Place::NewTab => {
                    live.tabs.push(LiveTab::new(panel_id));
                    live.active = live.tabs.len() - 1;
                }
                Place::Split(axis) => {
                    let tab = live.tab_mut();
                    if tab.layout.split(tab.focused, axis, panel_id) {
                        tab.focused += 1;
                    }
                }
            }
        }
        live.normalize();
        inner.dirty.main = true;
        Some(panel_id)
    }

    /// Shows tab `index`.
    pub fn select_tab(&self, index: usize) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        if index == live.active || index >= live.tabs.len() {
            return;
        }
        live.active = index;
        inner.dirty.main = true;
        inner.mark_agent_focus();
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Stops panel `panel_id`. Its pane goes away and its sibling takes the
    /// space; a tab whose last pane closes goes away too.
    pub fn close_panel(&self, panel_id: u64) {
        let mut inner = self.0.borrow_mut();
        if inner
            .active_live_mut()
            .is_some_and(|live| live.remove_panel(panel_id))
        {
            inner.dirty.main = true;
        }
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Stops every panel of tab `index` and removes the tab.
    pub fn close_tab(&self, index: usize) {
        self.close_tabs(|tab| tab == index);
    }

    /// Stops the panels of every tab but `index` and removes those tabs,
    /// so tab `index` is left, shown.
    pub fn close_other_tabs(&self, index: usize) {
        self.select_tab(index);
        self.close_tabs(|tab| tab != index);
    }

    /// Stops every panel of the tabs `close` picks by index and removes
    /// the tabs.
    fn close_tabs(&self, close: impl Fn(usize) -> bool) {
        let ids: Vec<u64> = self.with_active(|live| {
            let mut ids = Vec::new();
            for (index, tab) in live
                .into_iter()
                .flat_map(|live| live.tabs.iter().enumerate())
            {
                if close(index) {
                    tab.layout.for_each(&mut |id| ids.push(id));
                }
            }
            ids
        });
        let mut inner = self.0.borrow_mut();
        if let Some(live) = inner.active_live_mut() {
            live.panels.retain(|(id, panel)| {
                let close = ids.contains(id);
                if close {
                    panel.end_session();
                }
                !close
            });
            live.normalize();
            inner.dirty.main = true;
        }
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Splits the focused pane of the shown tab along `axis` with a new
    /// shell, which takes focus.
    pub fn split_pane(&self, axis: SplitAxis) {
        let Some(panes) = self.with_active(|live| Some(live?.layout().len())) else {
            return;
        };
        if panes >= MAX_PANES {
            self.notify((self.tr().max_panes)(MAX_PANES));
            return;
        }
        let cwd = self.active_terminal().and_then(|t| t.current_dir());
        let spec = PanelSpec::Terminal {
            title: None,
            command: None,
            cwd,
            session: None,
            control: None,
            agent: None,
            once: Vec::new(),
        };
        if self.push_panel(spec, Place::Split(axis)).is_some() {
            self.resize_active();
            self.save();
        }
    }

    /// Moves the focused pane out of a split tab into a tab of its own,
    /// placed after the shown one.
    pub fn move_pane_to_tab(&self) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        let tab = live.tab_mut();
        let Some(panel) = tab.layout.remove(tab.focused) else {
            return;
        };
        tab.focused = tab.focused.min(tab.layout.len() - 1);
        live.active += 1;
        live.tabs.insert(live.active, LiveTab::new(panel));
        live.normalize();
        inner.dirty.main = true;
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Records that the tab of panel `id` is being dragged.
    pub fn start_tab_drag(&self, id: u64) {
        self.start_drag(Dragged::Tab(id));
    }

    /// Records that the pane of panel `id` is being dragged by its title.
    pub fn start_pane_drag(&self, id: u64) {
        self.start_drag(Dragged::Pane(id));
    }

    fn start_drag(&self, dragged: Dragged) {
        let mut inner = self.0.borrow_mut();
        let tr = inner.tr;
        // The shown tab cannot split its own panes, so its hint offers only
        // the reorder.
        let shown_tab = match dragged {
            Dragged::Tab(id) => inner.active_live().is_some_and(|live| {
                live.tabs.get(live.active).is_some_and(|tab| {
                    (0..tab.layout.len()).any(|pane| tab.layout.get(pane) == Some(id))
                })
            }),
            Dragged::Pane(_) => false,
        };
        inner.drag_hint = Some(
            match dragged {
                Dragged::Tab(_) if shown_tab => tr.drag_shown_tab_hint,
                Dragged::Tab(_) => tr.drag_tab_hint,
                Dragged::Pane(_) => tr.drag_pane_hint,
            }
            .to_string(),
        );
        inner.dragging = Some(dragged);
        inner.dirty.status = true;
    }

    /// Whether pane `pane` of the shown tab takes the drag in progress: a
    /// tab dropped on its own panes, or a pane on itself, changes nothing,
    /// so those show no preview.
    pub fn dock_accepts(&self, pane: usize, position: DockPosition) -> bool {
        let inner = self.0.borrow();
        let Some(live) = inner.active_live() else {
            return false;
        };
        match inner.dragging {
            // A tab joins at an edge; its middle has nothing to swap with.
            Some(Dragged::Tab(id)) => {
                position != DockPosition::Center
                    && live.tab_of(id).is_some_and(|tab| tab != live.active)
            }
            // A move that leaves the panes as they are is no drop: the
            // right edge of the left of two panes, say.
            Some(Dragged::Pane(id)) => {
                live.layout().get(pane).is_some_and(|shown| shown != id)
                    && docked(live.layout(), id, pane, position)
                        .is_some_and(|(moved, _)| !same_shape(&moved, live.layout()))
            }
            None => false,
        }
    }

    /// Whether a pane is being dragged, which the new tab button takes.
    pub fn dragging_pane(&self) -> bool {
        matches!(self.0.borrow().dragging, Some(Dragged::Pane(_)))
    }

    /// Drops the dragged pane on the new tab button: it moves into a tab of
    /// its own, as Ctrl+Shift+M moves the focused one.
    pub fn drop_pane_on_new_tab(&self) {
        let dragged = self.0.borrow_mut().dragging.take();
        let Some(Dragged::Pane(id)) = dragged else {
            return;
        };
        let pane = {
            let inner = self.borrow();
            inner
                .active_live()
                .filter(|live| live.layout().len() > 1)
                .and_then(|live| live.layout().position(id))
        };
        if let Some(pane) = pane {
            self.focus_pane(pane);
            self.move_pane_to_tab();
        }
    }

    /// Ends a tab or pane drag, dropped or not, and gives the keys back to
    /// the focused pane: a press on a tab took focus, and a reorder or a
    /// cancel would leave it on nothing.
    pub fn end_tab_drag(&self) {
        let mut inner = self.0.borrow_mut();
        inner.dragging = None;
        if inner.drag_hint.take().is_some() {
            inner.dirty.status = true;
            // Titles that changed during the drag waited for it to end.
            inner.dirty.tabs = true;
        }
        inner.dirty.refocus = true;
    }

    /// Whether a tab or pane is being dragged. The tab strip and the pane
    /// title strips hold the dragged one, so they are not rebuilt meanwhile.
    pub fn is_dragging(&self) -> bool {
        self.0.borrow().drag_hint.is_some()
    }

    /// Drops the dragged tab on pane `pane` of the shown tab. An edge moves
    /// the dragged tab's panes into a split on that side and removes the
    /// dragged tab. The middle takes no tab (see [`Workspace::dock_accepts`]).
    pub fn dock_dragged(&self, pane: usize, position: DockPosition) {
        let dragged = self.0.borrow_mut().dragging.take();
        let id = match dragged {
            Some(Dragged::Tab(id)) => id,
            Some(Dragged::Pane(id)) => return self.dock_pane(id, pane, position),
            None => return,
        };
        let (axis, before) = match position {
            DockPosition::Center => (None, false),
            DockPosition::Left => (Some(SplitAxis::SideBySide), true),
            DockPosition::Right => (Some(SplitAxis::SideBySide), false),
            DockPosition::Top => (Some(SplitAxis::Stacked), true),
            DockPosition::Bottom => (Some(SplitAxis::Stacked), false),
        };
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        let Some(source) = live.tab_of(id) else {
            return;
        };
        let Some(axis) = axis.filter(|_| source != live.active) else {
            drop(inner);
            self.select_tab(source);
            return;
        };
        if pane >= live.layout().len() {
            return;
        }
        if live.layout().len() + live.tabs[source].layout.len() > MAX_PANES {
            drop(inner);
            self.notify((self.tr().max_panes)(MAX_PANES));
            return;
        }
        let moved = live.tabs.remove(source);
        if source < live.active {
            live.active -= 1;
        }
        let tab = live.tab_mut();
        if tab.layout.split_tree(pane, axis, moved.layout, before) {
            tab.focused = if before { pane } else { pane + 1 } + moved.focused;
            // The joined panes share the row or column evenly.
            tab.layout.even_out(axis);
        }
        live.normalize();
        inner.dirty.main = true;
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Drops the dragged pane on tab `index`: it leaves the shown tab and
    /// joins that one beside its focused pane, which is then shown.
    pub fn drop_pane_on_tab(&self, index: usize) {
        let dragged = self.0.borrow_mut().dragging.take();
        let Some(Dragged::Pane(id)) = dragged else {
            return;
        };
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        if index == live.active || index >= live.tabs.len() || live.layout().len() < 2 {
            return;
        }
        if live.tabs[index].layout.len() >= MAX_PANES {
            drop(inner);
            self.notify((self.tr().max_panes)(MAX_PANES));
            return;
        }
        let tab = live.tab_mut();
        let Some(from) = tab.layout.position(id) else {
            return;
        };
        if tab.layout.remove(from).is_none() {
            return;
        }
        tab.focused = tab.focused.min(tab.layout.len() - 1);
        let target = &mut live.tabs[index];
        if target
            .layout
            .split_with(target.focused, SplitAxis::SideBySide, id, false)
        {
            target.focused += 1;
            target.layout.even_out(SplitAxis::SideBySide);
        }
        live.active = index;
        live.normalize();
        inner.dirty.main = true;
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Drops the pane of panel `id` on pane `target` of the shown tab. An
    /// edge moves it into a split on that side of the target, the center
    /// swaps the two panes' panels.
    fn dock_pane(&self, id: u64, target: usize, position: DockPosition) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        let tab = live.tab_mut();
        let Some((layout, focused)) = docked(&tab.layout, id, target, position) else {
            return;
        };
        tab.layout = layout;
        tab.focused = focused;
        live.normalize();
        inner.dirty.main = true;
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Moves the tab at `from` to `to` in the tab strip.
    pub fn reorder_tab(&self, from: usize, to: usize) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        if from == to || from >= live.tabs.len() || to >= live.tabs.len() {
            return;
        }
        let shown = live.active;
        let tab = live.tabs.remove(from);
        live.tabs.insert(to, tab);
        live.active = if shown == from {
            to
        } else if from < shown && shown <= to {
            shown - 1
        } else if to <= shown && shown < from {
            shown + 1
        } else {
            shown
        };
        inner.dirty.tabs = true;
        drop(inner);
        self.save();
    }

    /// Stops the panel in the focused pane, which closes the pane.
    pub fn close_pane(&self) {
        if let Some(id) = self.pane_panel_id(self.focused_pane()) {
            self.close_panel(id);
        }
    }

    /// Records which pane has input focus. Rebuilds the panel only when the
    /// focus actually moved, so the lane border, tabs and status follow it.
    pub fn focus_pane(&self, pane: usize) {
        let mut inner = self.0.borrow_mut();
        // Focus reached a panel, so the next `focus_tabs` starts over at
        // the tab strip.
        inner.chrome_step = None;
        if let Some(live) = inner.active_live_mut()
            && live.focused() != pane
            && pane < live.layout().len()
        {
            live.tab_mut().focused = pane;
            inner.dirty.main = true;
            inner.mark_agent_focus();
        }
    }

    /// Focuses the pane that shows `explore`, so a mode picked from its
    /// bar's menu leaves the keyboard in that pane and not in another.
    pub fn focus_explore_pane(&self, explore: &Rc<crate::explore::ExplorePanel>) {
        let pane = {
            let inner = self.0.borrow();
            let Some(live) = inner.active_live() else {
                return;
            };
            (0..live.layout().len()).find(|&pane| {
                live.layout()
                    .get(pane)
                    .and_then(|id| live.panel(id))
                    .is_some_and(|(_, panel)| {
                        matches!(panel, LivePanel::Explore(shown) if Rc::ptr_eq(shown, explore))
                    })
            })
        };
        if let Some(pane) = pane {
            self.focus_pane(pane);
            self.0.borrow_mut().dirty.main = true;
        }
    }

    /// Moves focus to the next pane, wrapping after the last.
    pub fn focus_next_pane(&self) {
        self.cycle_pane(1);
    }

    /// Moves focus `delta` panes on, wrapping at either end.
    pub fn cycle_pane(&self, delta: isize) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        let tab = live.tab_mut();
        let count = tab.layout.len();
        if count < 2 {
            return;
        }
        tab.focused = wrap(tab.focused, delta, count);
        inner.dirty.main = true;
        inner.mark_agent_focus();
    }

    /// Shows the tab `delta` places from the shown one, wrapping at either
    /// end of the strip.
    pub fn cycle_tab(&self, delta: isize) {
        let next = self.with_active(|live| {
            let live = live?;
            let count = live.tabs.len();
            (count > 1).then(|| wrap(live.active, delta, count))
        });
        if let Some(index) = next {
            self.select_tab(index);
        }
    }

    /// Position of the focused pane.
    pub fn focused_pane(&self) -> usize {
        self.with_active(|live| live.map_or(0, LiveWorktree::focused))
    }

    /// Id of the panel in `pane`.
    pub fn pane_panel_id(&self, pane: usize) -> Option<u64> {
        self.with_active(|live| live?.pane_panel(pane).map(|p| p.0))
    }

    /// The panels shown in the main area, if a worktree is active.
    pub fn with_active<R>(&self, f: impl FnOnce(Option<&LiveWorktree>) -> R) -> R {
        let inner = self.0.borrow();
        f(inner.active_live())
    }

    /// The terminal in the focused pane.
    pub fn active_terminal(&self) -> Option<Rc<TerminalPanel>> {
        self.pane_terminal(self.focused_pane())
    }

    /// The terminal shown in `pane`, if that pane shows one.
    pub fn pane_terminal(&self, pane: usize) -> Option<Rc<TerminalPanel>> {
        self.with_active(|live| match live?.pane_panel(pane)? {
            (_, LivePanel::Terminal(terminal)) => Some(terminal.clone()),
            _ => None,
        })
    }

    /// Sets the area available to panels, in logical pixels and in terminal
    /// cells, and resizes the visible terminals.
    pub fn set_panel_area(&self, size: schnellui::scene::Size, cols: u16, rows: u16) {
        let changed = {
            let mut inner = self.0.borrow_mut();
            let changed = inner.panel_size != size || inner.grid != (cols, rows);
            inner.panel_size = size;
            inner.grid = (cols, rows);
            changed
        };
        if changed {
            self.resize_active();
        }
    }

    /// Fits every terminal of the shown worktree to its pane, in every tab,
    /// and records the pane sizes of the shown tab.
    fn resize_active(&self) {
        let mut inner = self.0.borrow_mut();
        let inner = &mut *inner;
        let mut sizes = std::mem::take(&mut inner.pane_sizes);
        match inner.active_live() {
            Some(live) => pane_sizes(live.layout(), inner.panel_size, &mut sizes),
            None => sizes.clear(),
        }
        inner.pane_sizes = sizes;
        let metrics = inner.metrics;
        let Some(live) = inner.active_live() else {
            return;
        };
        let mut sizes = Vec::new();
        for tab in &live.tabs {
            let grids = inner.grids_for(&tab.layout, &mut sizes);
            let mut pane = 0;
            tab.layout.for_each(&mut |id| {
                match live.panel(id) {
                    Some((_, LivePanel::Terminal(terminal))) => {
                        let (cols, rows) = grids[pane];
                        terminal.resize(cols, rows, metrics);
                    }
                    Some((_, LivePanel::Files(files))) => {
                        let (cols, rows) = grids[pane];
                        files.resize(cols, rows);
                    }
                    Some((_, LivePanel::Explore(explore))) => {
                        let (cols, rows) = explore_grid(grids[pane], metrics);
                        explore.resize(cols, rows);
                    }
                    Some((_, LivePanel::Monitor(monitor))) => {
                        let (cols, rows) = explore_grid(grids[pane], metrics);
                        monitor.resize(cols, rows);
                    }
                    Some((_, LivePanel::Tasks(tasks))) => {
                        let (cols, rows) = explore_grid(grids[pane], metrics);
                        tasks.resize(cols, rows);
                    }
                    _ => {}
                }
                pane += 1;
            });
        }
    }

    // --- extensions -----------------------------------------------------------

    pub fn run_command(&self, name: &str) {
        let result = self.0.borrow_mut().ext.run_command(name);
        if let Err(error) = result {
            self.notify(error.to_string());
        }
    }

    pub(crate) fn emit(&self, event: &Event) {
        let errors = self.0.borrow_mut().ext.emit(event);
        for error in errors {
            self.notify(error.to_string());
        }
    }

    // --- settings -------------------------------------------------------------

    /// Starts following the desktop's light or dark preference. The watcher
    /// thread only stores the answer and wakes the window; [`Self::poll`]
    /// switches the scheme. Headless tests skip this and stay dark.
    pub fn follow_system_appearance(&self) {
        use schnellui::platform::{ColorScheme, PlatformPreferences, PreferenceChange};
        let (dark, wake) = {
            let inner = self.0.borrow();
            (inner.system_dark.clone(), inner.wake.clone())
        };
        if let Some(scheme) = PlatformPreferences::detect().color_scheme {
            dark.store(scheme == ColorScheme::Dark, Ordering::Release);
        }
        schnellui::platform::watch_preferences(move |change| {
            if let PreferenceChange::ColorScheme(scheme) = change {
                // No preference counts as dark, the app's own default.
                let next = scheme != Some(ColorScheme::Light);
                if dark.swap(next, Ordering::AcqRel) != next
                    && let Some(signal) = wake.get()
                {
                    signal.request_redraw();
                }
            }
            true
        });
        self.apply_scheme();
    }

    /// Overrides the desktop preference, for tests.
    #[cfg(test)]
    pub fn set_system_dark(&self, dark: bool) {
        self.0.borrow().system_dark.store(dark, Ordering::Release);
    }

    /// Sets the terminal font size for this run without saving it, as the
    /// `--font-size` flag does.
    pub fn override_font_size(&self, size: f32) {
        self.0.borrow_mut().settings.font_size = Settings::clamp_font_size(size);
        self.apply_font();
    }

    pub fn set_font_size(&self, size: f32) {
        let size = Settings::clamp_font_size(size);
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.font_size == size {
                return;
            }
            inner.settings.font_size = size;
        }
        self.apply_font();
        self.save_settings();
    }

    /// Changes the font size by `delta` pixels.
    pub fn step_font_size(&self, delta: f32) {
        let size = self.0.borrow().settings.font_size;
        self.set_font_size(size + delta);
    }

    /// Picks an installed family for terminals; `None` is the bundled face.
    pub fn set_font_family(&self, family: Option<String>) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.font_family == family {
                return;
            }
            inner.settings.font_family = family;
        }
        self.apply_font();
        self.save_settings();
    }

    /// Picks a scheme by id, or [`crate::state::SYSTEM_SCHEME`].
    pub fn set_color_scheme(&self, id: &str) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.color_scheme == id {
                return;
            }
            inner.settings.color_scheme = id.to_owned();
            // The selected row moves even when the resolved scheme stays.
            inner.dirty.settings = true;
        }
        self.apply_scheme();
        self.save_settings();
    }

    /// Picks the UI language by id, or [`crate::state::SYSTEM_LANGUAGE`].
    /// Every region shows text, so a change remounts the app.
    pub fn set_language(&self, id: &str) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.language == id {
                return;
            }
            inner.settings.language = id.to_owned();
            inner.dirty.settings = true;
            let tr = Lang::resolve(id).strings();
            if !std::ptr::eq(tr, inner.tr) {
                inner.tr = tr;
                inner.dirty.full = true;
            }
        }
        self.save_settings();
    }

    /// Puts every setting back to its default, saves, and applies the
    /// result: font, scheme, language, sandbox proxy, extensions, keymap.
    /// Stops serving this machine, which the default does not do. Opens the
    /// first-start setup again, because the default has not finished it.
    pub fn reset_settings(&self) {
        {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            inner.settings = Settings::default();
            inner.tr = Lang::resolve(&inner.settings.language).strings();
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.full = true;
        }
        self.stop_serving();
        self.apply_font();
        self.apply_scheme();
        self.save_settings();
        self.apply_cpu_modes();
        self.reload_extensions();
        self.open_setup_if_unanswered();
    }

    /// Turns an extension on or off and reloads all extensions.
    pub fn set_extension_enabled(&self, name: &str, enabled: bool) {
        let changed = {
            let mut inner = self.0.borrow_mut();
            let default_on = extension_default_on(&inner.ext, name);
            inner
                .settings
                .set_extension_enabled(name, enabled, default_on)
        };
        if changed {
            self.save_settings();
            self.reload_extensions();
        }
    }

    /// Sets setting `key` of extension `name`. With `reload` the extensions
    /// reload now; without, they reload when the user leaves the page, so
    /// typing into a field does not reload on every key.
    pub fn set_extension_setting(
        &self,
        name: &str,
        key: &str,
        value: &crate::ext::SettingValue,
        reload: bool,
    ) {
        let changed = {
            let mut inner = self.0.borrow_mut();
            let value = serde_json::to_value(value).ok();
            let changed = inner.settings.set_extension_setting(name, key, value);
            if changed && !reload {
                inner.settings_ui.extensions_stale = true;
            }
            changed
        };
        if changed {
            self.save_settings();
            if reload {
                self.reload_extensions();
            }
        }
    }

    /// Reloads the extensions if a typed setting changed since they loaded.
    pub fn apply_extension_settings(&self) {
        let stale = std::mem::take(&mut self.0.borrow_mut().settings_ui.extensions_stale);
        if stale {
            self.reload_extensions();
        }
    }

    /// Grants extension `name` every permission its manifest declares and
    /// loads it.
    pub fn grant_extension_permissions(&self, name: &str) {
        let changed = {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            let declared = inner
                .ext
                .extensions()
                .find(|ext| ext.name == name)
                .map(|ext| ext.permissions.names())
                .unwrap_or_default();
            inner.settings.grant_extension(name, &declared)
        };
        if changed {
            self.save_settings();
            self.reload_extensions();
        }
    }

    /// Takes every permission from extension `name`, so it loads again
    /// only after the user grants them anew. A shipped extension keeps an
    /// empty grant, which replaces its default one.
    pub fn revoke_extension_permissions(&self, name: &str) {
        let changed = {
            let settings = &mut self.0.borrow_mut().settings;
            if crate::bundle::is_shipped(name) {
                settings
                    .extension_permissions
                    .insert(name.to_owned(), Vec::new())
                    .is_none_or(|before| !before.is_empty())
            } else {
                settings.revoke_extension(name)
            }
        };
        if changed {
            self.save_settings();
            self.reload_extensions();
        }
    }

    /// Sets whether extension `name` holds the single permission
    /// `permission`, and reloads it. The grant saved is the extension's
    /// effective set, so a shipped extension's default grant is pinned to
    /// exactly the permissions the user leaves on.
    pub fn set_extension_permission(&self, name: &str, permission: Permission, on: bool) {
        let want = permission.name().to_owned();
        let mut granted = granted_permissions(&self.0.borrow().settings)
            .get(name)
            .cloned()
            .unwrap_or_default();
        let before = granted.clone();
        if on {
            if !granted.contains(&want) {
                granted.push(want);
            }
        } else {
            granted.retain(|held| held != &want);
        }
        granted.sort();
        if granted != before {
            self.0
                .borrow_mut()
                .settings
                .extension_permissions
                .insert(name.to_owned(), granted);
            self.save_settings();
            self.reload_extensions();
        }
    }

    /// Drops every extension VM and loads the extensions directory again,
    /// skipping the ones turned off. Each extension sees `app_started` again.
    pub fn reload_extensions(&self) {
        let errors = {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            let mut host = extension_host(&inner.dirs, &inner.settings);
            let errors = host.load_dirs_except(
                &extension_dirs(&inner.shipped_dir, &inner.extensions_dir),
                &inner.settings.disabled_extensions,
                &granted_permissions(&inner.settings),
                &setting_values(&inner.settings),
            );
            let mut errors = worth_a_notice(&host, errors);
            host.set_context(inner.ext_context());
            crate::agents::set_custom(inner.ext.owner(), &[]);
            errors.extend(crate::agents::set_custom(host.owner(), host.agents()));
            inner.ext_agents_seen = host.agents_revision();
            inner.ext = host;
            inner.tasks.look_again();
            if let Some(Some(server)) = &inner.control {
                (server.control).publish(extension_status(&inner.ext, &inner.settings));
                (server.control).publish_tools(inner.ext.tools());
            }
            inner.ext_tools_seen = inner.ext.tools_revision();
            inner.dirty.sidebar = true;
            inner.dirty.settings = true;
            if inner.palette.open {
                inner.dirty.palette_list = true;
            }
            errors
        };
        for error in errors {
            self.notify(error.to_string());
        }
        self.emit(&Event::AppStarted);
        self.rebuild_keymap();
        // The views of the old VMs are gone; the new ones render the panels.
        self.refresh_views(None, None);
    }

    /// Re-measures the terminal cell after a font change and remounts. The
    /// app's next update fits the grid to the new cell size.
    fn apply_font(&self) {
        let mut inner = self.0.borrow_mut();
        let inner = &mut *inner;
        inner.metrics = measure(&mut inner.shaper, &inner.settings);
        inner.dirty.full = true;
    }

    /// Switches to the scheme the settings and the desktop resolve to, if it
    /// changed. Runs every poll: without a change it compares a few bytes and
    /// allocates nothing.
    fn apply_scheme(&self) {
        let mut inner = self.0.borrow_mut();
        let scheme = resolve_scheme(&inner.settings.color_scheme, inner.system_dark());
        if std::ptr::eq(scheme, inner.scheme) {
            return;
        }
        inner.scheme = scheme;
        let colors = scheme.term_colors();
        for live in inner.live.values().flat_map(HashMap::values) {
            for (_, panel) in &live.panels {
                if let LivePanel::Terminal(terminal) = panel {
                    terminal.set_colors(colors);
                }
            }
        }
        inner.dirty.full = true;
    }

    pub(crate) fn save_settings(&self) {
        let mut inner = self.0.borrow_mut();
        if let Err(error) = inner.settings_store.save(&inner.settings) {
            let text = (inner.tr.settings_not_saved)(&error.to_string());
            inner.push_notice(text);
        }
        #[cfg(not(test))]
        crate::debug_browser::configure(&inner.settings, &inner.dirs);
        inner.apply_default_shell();
        inner.publish_environment();
    }

    /// Which terminals get the control socket, from the Agents page. New
    /// terminals follow it; open ones keep what they have.
    pub fn set_agent_control(&self, mode: AgentControl) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.agent_control == mode {
                return;
            }
            inner.settings.agent_control = mode;
            inner.dirty.settings = true;
        }
        self.save_settings();
    }

    /// Replaces the settings with `settings`, as a suggestion an agent
    /// made and the user allowed, and applies what changed: language,
    /// font, color scheme, sandbox proxy, extensions and keymap. Agents
    /// cannot suggest `remote` (`control::AGENT_LOCKED`), so serving stays
    /// as it is.
    pub fn replace_settings(&self, settings: Settings) {
        {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            inner.settings = settings;
            inner.tr = Lang::resolve(&inner.settings.language).strings();
            inner
                .runtimes
                .update_proxy(&inner.settings.sandbox, inner.ext.sandbox_rules());
            inner.dirty.full = true;
        }
        self.apply_font();
        self.apply_scheme();
        self.save_settings();
        self.apply_cpu_modes();
        self.reload_extensions();
    }

    // --- per-wake work --------------------------------------------------------

    /// Drains worker replies, extension requests and process exits. Cheap when
    /// nothing happened: it touches channels and atomics and allocates nothing.
    pub fn poll(&self) {
        loop {
            let reply = self.0.borrow().vcs.try_recv();
            let Some(reply) = reply else { break };
            self.apply_reply(reply);
        }
        self.poll_exits();
        self.poll_attention();
        self.poll_frozen_agents();
        self.poll_tasks_panel();
        self.poll_runtimes();
        self.poll_forges();
        self.poll_project_env();
        if cmdterm_env::sandbox_warnings_pending() {
            self.report_sandbox_warnings();
        }
        {
            let mut inner = self.0.borrow_mut();
            let probed = inner.settings_ui.machines.probe.changed()
                | inner.settings_ui.machines.vpn.changed();
            let agents = inner.settings_ui.agents.changed();
            if (agents || probed) && inner.settings_ui.open {
                inner.dirty.settings = true;
            }
            if agents {
                let tr = inner.tr;
                for change in inner.settings_ui.agents.take_finished() {
                    let (agent, env) = (change.agent.name(), change.env.label());
                    let (failed, text) = match &change.result {
                        Err(error) => (true, (tr.toast_agent_failed)(agent, env, error)),
                        Ok(()) if change.on => (false, (tr.toast_agent_installed)(agent, env)),
                        Ok(()) => (false, (tr.toast_agent_removed)(agent, env)),
                    };
                    inner.toast(failed, text);
                }
            }
            if agents && inner.setup.is_some() {
                inner.dirty.setup = true;
            }
            if agents && inner.palette.open && inner.palette.input.is_none() {
                inner.dirty.palette_list = true;
            }
            if inner.notifications.changed() {
                // Results that arrive while the overlay is open, such as an
                // update it ran, are read on arrival.
                if inner.notifications_open {
                    inner.notifications.mark_all_read();
                    inner.notifications.changed();
                }
                // The bell's badge counts the unread ones.
                inner.dirty.sidebar = true;
                inner.dirty.notifications |= inner.notifications_open;
            }
        }
        let due = self.0.borrow().notifications.take_due();
        if due && self.0.borrow().settings.check_updates {
            self.check_updates();
        }
        let fused = self.0.borrow().fusion.changed();
        if fused {
            self.apply_fusion();
        }
        let built = self.0.borrow().settings_ui.machines.jobs.changed();
        if built {
            self.image_jobs_changed();
        }
        self.apply_picked_folder();
        let tasks = {
            let inner = self.0.borrow();
            inner
                .tasks
                .wants_a_look(inner.ext.tasks_revision(), crate::tasks::unix_now())
        };
        if tasks {
            self.run_due_tasks();
        }
        self.poll_extension_ui();
        self.apply_requests();
        if !self.0.borrow().notices.is_empty() {
            self.expire_notices(Instant::now());
        }
        if self.0.borrow().settings_ui.toast.is_some() {
            self.0.borrow_mut().expire_toast(Instant::now());
        }
        self.apply_scheme();
        self.0.borrow_mut().sync_sandbox_rules();
        let control_changed = {
            let mut inner = self.0.borrow_mut();
            match &inner.control {
                Some(Some(server)) => {
                    let revision = server.control.revision();
                    let changed = inner.control_seen != revision;
                    if changed {
                        inner.control_seen = revision;
                        inner.dirty.approval = true;
                    }
                    changed
                }
                _ => false,
            }
        };
        if control_changed {
            self.allow_launches_without_asking();
        }
        let commands_changed = {
            let inner = self.0.borrow();
            inner.ext.commands_revision() != inner.keymap_revision
        };
        if commands_changed {
            self.rebuild_keymap();
        }
        self.poll_debug_browser();
        #[cfg(feature = "browser")]
        self.pump_browser();
    }

    #[cfg(feature = "browser")]
    fn pump_browser(&self) {
        let inner = self.0.borrow();
        let tabs: [Option<_>; MAX_PANES] = std::array::from_fn(|pane| {
            match inner.active_live().and_then(|live| live.pane_panel(pane)) {
                Some((_, LivePanel::Browser(browser))) => browser.tab().cloned(),
                _ => None,
            }
        });
        let visible = tabs.iter().any(Option::is_some);
        inner
            .browser_visible
            .store(visible, std::sync::atomic::Ordering::Release);
        if !visible {
            return;
        }
        let mut sizes = [inner.panel_size; MAX_PANES];
        for (slot, size) in sizes.iter_mut().zip(&inner.pane_sizes) {
            *slot = *size;
        }
        drop(inner);
        for (tab, size) in tabs.into_iter().zip(sizes) {
            if let Some(tab) = tab
                && tab.pump(size)
                && tab.title_changed()
            {
                self.0.borrow_mut().dirty.tabs = true;
            }
        }
    }

    /// Routes focused input to the browser panel in `pane`.
    #[cfg(feature = "browser")]
    pub fn browser_input(
        &self,
        pane: usize,
        event: schnellui::FocusedInputEvent,
    ) -> schnellui::FocusedInputResult {
        if matches!(
            event,
            schnellui::FocusedInputEvent::Focus(schnellui::RawFocusEvent::WidgetGained)
        ) {
            self.focus_pane(pane);
        }
        let tab = match self
            .0
            .borrow()
            .active_live()
            .and_then(|live| live.pane_panel(pane))
        {
            Some((_, LivePanel::Browser(browser))) => browser.tab().cloned(),
            _ => None,
        };
        match tab {
            Some(tab) => tab.input(event),
            None => schnellui::FocusedInputResult::Ignored,
        }
    }

    /// Drops `input` from the inputs being discovered. Returns whether it
    /// was to open as a local session.
    fn clear_pending(&self, input: &str) -> bool {
        let mut inner = self.0.borrow_mut();
        if let Some(index) = inner.pending_projects.iter().position(|p| p == input) {
            inner.pending_projects.remove(index);
        }
        inner.dirty.sidebar = true;
        let index = inner.temporary_inputs.iter().position(|p| p == input);
        index
            .map(|index| inner.temporary_inputs.remove(index))
            .is_some()
    }

    /// Adds the project the worker found for `input`. With `devcontainer`,
    /// the main worktree's terminals start in its devcontainer, if it has one.
    fn finish_discovery(
        &self,
        input: &str,
        env: Environment,
        repo: Repository,
        devcontainer: bool,
    ) {
        let temporary = self.clear_pending(input);
        let known = self
            .0
            .borrow()
            .state
            .projects
            .iter()
            .any(|p| !p.is_global() && p.env == env && p.root == repo.root);
        let name = repo
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.root.display().to_string());
        let id = self.0.borrow_mut().state.add_project(
            name.clone(),
            env.clone(),
            repo.root.clone(),
            repo.kind,
            None,
        );
        if let Some(project) = self.0.borrow_mut().state.project_mut(id) {
            // A known project stays kept; a local session opened again for
            // good is kept from now on.
            project.temporary = temporary && (project.temporary || !known);
            // A project added again keeps the git directory it had.
            if project.git_dir.is_none() {
                project.git_dir = repo.known().map(|known| known.git_dir);
            }
        }
        if devcontainer {
            if let Ok(Some(_)) = crate::container::find(&env, &repo.root) {
                if let Some(project) = self.0.borrow_mut().state.project_mut(id) {
                    project.worktree_mut(&repo.root).runtime = Runtime::Devcontainer;
                }
            } else {
                self.notify(self.tr().no_devcontainer);
            }
        }
        self.0.borrow_mut().tasks.look_again();
        self.emit(&Event::ProjectAdded {
            project_id: project_storage_id(&name, &env, &repo.root),
            name,
            root: repo.root.clone(),
            env: env.label().to_owned(),
        });
        self.select_project(id);
        self.save();
    }

    fn apply_reply(&self, reply: Reply) {
        match reply {
            Reply::Snapshot { project, snapshot } => {
                let mut inner = self.0.borrow_mut();
                inner.errors.remove(&project);
                if let Some(state) = inner.state.project_mut(project)
                    && state.forge != snapshot.forge
                {
                    state.forge = snapshot.forge.clone();
                }
                // A project saved before its git directory was recorded
                // gets the one this poll found, and is polled with it.
                let mut recorded = false;
                if let Some(state) = inner.state.project_mut(project)
                    && state.git_dir.is_none()
                    && let Some(known) = snapshot.repo.known()
                {
                    state.git_dir = Some(known.git_dir);
                    recorded = true;
                }
                // Turning jj on or off for a project changes what it reads.
                let kind = Some(snapshot.repo.kind);
                if let Some(state) = inner.state.project_mut(project)
                    && state.vcs.is_some()
                    && state.vcs != kind
                {
                    state.vcs = kind;
                    inner.dirty.palette_list = true;
                }
                let before = inner.active_branch().map(str::to_owned);
                // Says once when a worktree's `.git` stops leading to the
                // recorded metadata, as after a sandbox replaced it.
                let before_replaced = inner.snapshots.get(&project).map(|old| &old.replaced);
                let newly: Vec<String> = (snapshot.replaced.iter())
                    .filter(|path| !before_replaced.is_some_and(|old| old.contains(path)))
                    .map(|path| (inner.tr.git_dir_replaced)(&path.display().to_string()))
                    .collect();
                for text in newly {
                    inner.push_notice(text);
                }
                // A review shows the files on disk, so it follows their status.
                if let Some(old) = inner.snapshots.get(&project)
                    && let Some(live) = inner.live.get(&project)
                {
                    let status = |snapshot: &Snapshot, path: &Path| {
                        let i = snapshot.worktrees.iter().position(|w| w.path == path)?;
                        snapshot.status.get(i).cloned().flatten()
                    };
                    for (path, live) in live {
                        if status(old, path) == status(&snapshot, path) {
                            continue;
                        }
                        for (_, panel) in &live.panels {
                            match panel {
                                LivePanel::Explore(explore) => explore.files_changed(),
                                LivePanel::Files(files) => files.files_changed(),
                                _ => {}
                            }
                        }
                    }
                }
                if inner.snapshots.insert(project, *snapshot).is_none() {
                    // Tasks for the project wait for its worktrees.
                    inner.tasks.look_again();
                }
                inner.dirty.sidebar = true;
                // Agents read the worktrees' branches with their change requests.
                inner.forges.changed();
                if inner.active_branch() != before.as_deref() {
                    inner.dirty.status = true;
                }
                drop(inner);
                if recorded {
                    self.save();
                    self.reread_project(project);
                }
                // Change request pages may wait for this repository.
                self.ask_waiting_discussions();
            }
            Reply::Error { project, message } => {
                self.0.borrow_mut().errors.insert(project, message.clone());
                if message.starts_with("could not add worktree") {
                    self.fail_launches(project, &message);
                }
                self.notify(message);
            }
            Reply::WorktreeAdded { project, path } => {
                self.activate_worktree(project, &path);
                self.finish_launches(project, &path);
            }
            Reply::RemoteDone {
                project,
                path,
                op,
                result,
            } => {
                let mut inner = self.0.borrow_mut();
                let tr = inner.tr;
                let label = match (inner.remote_runs.iter())
                    .position(|run| run.project == project && run.path == path)
                {
                    Some(i) => inner.remote_runs.remove(i).label,
                    None => crate::search::folder_name(&path),
                };
                let text = match (&op, result) {
                    (RemoteOp::Fetch, Ok(_)) => (tr.vcs_fetched)(&label),
                    (RemoteOp::Pull, Ok(_)) => (tr.vcs_pulled)(&label),
                    (RemoteOp::Push(_), Ok(remote)) => {
                        (tr.vcs_pushed)(&label, remote.as_deref().unwrap_or_default())
                    }
                    (RemoteOp::Fetch, Err(reason)) => (tr.vcs_fetch_failed)(&label, &reason),
                    (RemoteOp::Pull, Err(reason)) => (tr.vcs_pull_failed)(&label, &reason),
                    (RemoteOp::Push(_), Err(reason)) => (tr.vcs_push_failed)(&label, &reason),
                };
                inner.push_notice(text);
                drop(inner);
                self.reread_project(project);
            }
            Reply::Discovered {
                input,
                env,
                repo,
                devcontainer,
            } => self.finish_discovery(&input, env, repo, devcontainer),
            Reply::GlobalOpened { input, env, root } => {
                self.clear_pending(&input);
                let name = self.tr().global_session.to_owned();
                let id = self
                    .0
                    .borrow_mut()
                    .state
                    .add_global_session(name, env, root);
                self.select_project(id);
                self.save();
            }
            Reply::DiscoverFailed { input, message } => {
                self.clear_pending(&input);
                self.notify(format!("{input}: {message}"));
            }
        }
    }

    /// Marks the sidebar and the tab strip dirty when a terminal's attention
    /// state changed. Reads one atomic per terminal; allocation-free.
    fn poll_attention(&self) {
        let mut inner = self.0.borrow_mut();
        let revision = inner
            .live
            .values()
            .flat_map(HashMap::values)
            .flat_map(|live| &live.panels)
            .fold(0u64, |sum, (_, panel)| match panel {
                LivePanel::Terminal(terminal) => {
                    sum.wrapping_add(terminal.session.attention_revision())
                }
                LivePanel::Browser(_)
                | LivePanel::Files(_)
                | LivePanel::Explore(_)
                | LivePanel::Monitor(_)
                | LivePanel::DebugBrowser(_)
                | LivePanel::Tasks(_) => sum,
            });
        if revision != inner.attention_revision {
            inner.attention_revision = revision;
            inner.dirty.sidebar = true;
            inner.dirty.tabs = true;
            inner.forges.soon();
            drop(inner);
            self.record_agent_titles();
            self.record_agent_sessions();
        }
    }

    /// Saves the titles agent sessions gave their terminals since the last
    /// look into their worktrees. Runs when an attention revision moved,
    /// which a new title also moves.
    fn record_agent_titles(&self) {
        let mut found = Vec::new();
        {
            let inner = self.0.borrow();
            for (project, worktrees) in &inner.live {
                for (path, live) in worktrees {
                    for (_, panel) in &live.panels {
                        if let LivePanel::Terminal(terminal) = panel
                            && let Some(title) = terminal.take_agent_title()
                            && !crate::agents::generic_title(&title)
                        {
                            found.push((*project, path.clone(), title));
                        }
                    }
                }
            }
        }
        let mut changed = false;
        {
            let mut inner = self.0.borrow_mut();
            for (project, path, title) in found {
                let config = inner
                    .state
                    .project_mut(project)
                    .and_then(|project| project.worktrees.iter_mut().find(|c| c.path == path));
                if let Some(config) = config {
                    changed |= config.note_agent_title(&title);
                }
            }
        }
        if changed {
            self.save();
        }
    }

    /// Shows terminals whose frozen agents resumed, and every half minute,
    /// or more often while memory runs low, freezes idle agents in hidden
    /// terminals. Allocation-free unless one
    /// of the two is due.
    fn poll_frozen_agents(&self) {
        let (thawed, check) = {
            let inner = self.0.borrow();
            (
                inner.thaw_signal.swap(false, Ordering::AcqRel),
                inner.freeze_check.swap(false, Ordering::AcqRel),
            )
        };
        if thawed {
            let inner = self.0.borrow();
            for live in inner.live.values().flat_map(|worktrees| worktrees.values()) {
                for (_, panel) in &live.panels {
                    if let LivePanel::Terminal(terminal) = panel {
                        terminal.finish_thaw();
                    }
                }
            }
        }
        if check {
            let after = self.0.borrow().freeze_after();
            if let Some(after) = after {
                self.freeze_idle_agents(std::time::Instant::now(), after);
            }
            self.batch_hidden_sessions();
        }
    }

    /// Tells the clients of remote sessions whether their terminals show,
    /// so hidden ones get output in batches, and gives agents in hidden
    /// terminals the CPU mode's background weight and hidden terminals the
    /// efficiency cores. Called after the main region changed, after a CPU
    /// mode change and every half minute, for clients that could not be
    /// told before and agents that started since. Allocation-free unless
    /// a terminal's weight or cores change.
    pub fn batch_hidden_sessions(&self) {
        let inner = self.0.borrow();
        let modes = inner.settings.cpu_mode;
        let active = inner.state.active();
        for (project, worktrees) in &inner.live {
            for (path, live) in worktrees {
                let current = active.is_some_and(|active| {
                    active.id == *project && active.current_worktree() == path.as_path()
                });
                for (id, panel) in &live.panels {
                    if let LivePanel::Terminal(terminal) = panel {
                        let shown = current && live.layout().position(*id).is_some();
                        terminal.set_shown(shown);
                        terminal.update_cpu(shown, &modes);
                    }
                }
            }
        }
    }

    /// Freezes the agents that sat idle in hidden terminals for `after`:
    /// stops them, to resume their sessions when the terminals show again.
    pub fn freeze_idle_agents(&self, now: std::time::Instant, after: std::time::Duration) {
        let mut due = Vec::new();
        {
            let inner = self.0.borrow();
            let active = inner.active_key();
            for (project, worktrees) in &inner.live {
                for (path, live) in worktrees {
                    let current = active
                        .as_ref()
                        .is_some_and(|(id, active)| id == project && active == path);
                    for (id, panel) in &live.panels {
                        let LivePanel::Terminal(terminal) = panel else {
                            continue;
                        };
                        let shown = current && live.layout().position(*id).is_some();
                        if terminal.freeze_due(shown, now, after) {
                            due.push(terminal.clone());
                        }
                    }
                }
            }
        }
        for terminal in due {
            terminal.freeze();
        }
    }

    /// Saves the layout when an agent session started or ended in a
    /// terminal, so the next start resumes the agents that were open, and
    /// keeps every session with an id and its title in its worktree, so the
    /// Agents tab lists it after it ended.
    fn record_agent_sessions(&self) {
        let (mut changed, found) = {
            let inner = self.0.borrow();
            let mut changed = false;
            let mut found = Vec::new();
            for (project, worktrees) in &inner.live {
                for (path, live) in worktrees {
                    for (_, panel) in &live.panels {
                        let LivePanel::Terminal(terminal) = panel else {
                            continue;
                        };
                        changed |= terminal.take_agent_session();
                        let agent = terminal.agent_session.borrow();
                        let Some(agent) = agent.as_ref().filter(|a| a.session.is_some()) else {
                            continue;
                        };
                        let title = Some(terminal.agent_title())
                            .filter(|t| !t.is_empty() && !crate::agents::generic_title(t));
                        found.push((*project, path.clone(), agent.clone(), title));
                    }
                }
            }
            (changed, found)
        };
        if !found.is_empty() {
            let now = crate::tasks::unix_now();
            let mut inner = self.0.borrow_mut();
            let mut listed = false;
            for (project, path, agent, title) in found {
                let config = inner
                    .state
                    .project_mut(project)
                    .and_then(|project| project.worktrees.iter_mut().find(|c| c.path == path));
                if let Some(config) = config {
                    listed |= config.note_agent_session(&agent, title.as_deref(), now);
                }
            }
            if listed {
                inner.dirty.sidebar = true;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }

    /// Shows `path` of project `id` and resumes `agent`'s session there in
    /// a new terminal, unless one of its terminals already has it open, as
    /// a saved layout resumes its agents itself.
    pub fn resume_agent_session(&self, id: u64, path: &Path, agent: &crate::state::AgentSession) {
        self.activate_worktree(id, path);
        let open = self.with_active(|live| live.and_then(|live| live.session_panel(agent)));
        if let Some(panel) = open {
            self.reveal_panel(panel);
            return;
        }
        self.open(PanelSpec::Terminal {
            title: None,
            command: None,
            cwd: None,
            session: None,
            control: None,
            agent: Some(agent.clone()),
            once: Vec::new(),
        });
    }

    /// Shows worktree `path` of project `id` with the tab and pane of
    /// terminal `panel` focused, for a click on an agent in the Agents tab.
    pub fn focus_agent(&self, id: u64, path: &Path, panel: u64) {
        self.activate_worktree(id, path);
        self.reveal_panel(panel);
    }

    /// Shows the tab of the shown worktree that holds panel `id` and
    /// focuses its pane.
    fn reveal_panel(&self, id: u64) {
        let mut inner = self.0.borrow_mut();
        if !inner.active_live_mut().is_some_and(|live| live.reveal(id)) {
            return;
        }
        inner.dirty.main = true;
        inner.mark_agent_focus();
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Changes what the rail names worktrees after.
    pub fn set_worktree_titles(&self, titles: crate::state::WorktreeTitles) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.worktree_titles == titles {
                return;
            }
            inner.settings.worktree_titles = titles;
            inner.dirty.sidebar = true;
            inner.dirty.palette_list = true;
        }
        self.save_settings();
    }

    /// Changes who draws the window's title bar. The window follows on its
    /// next redraw; the tab strip gains or loses the window's buttons.
    pub fn set_window_frame(&self, frame: crate::state::WindowFrame) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.window_frame == frame {
                return;
            }
            inner.settings.window_frame = frame;
            inner.dirty.tabs = true;
            inner.dirty.palette_list = true;
        }
        self.save_settings();
    }

    /// Whether the app draws the window's title bar, in the tab strip.
    pub fn draws_title_bar(&self) -> bool {
        self.0.borrow().settings.window_frame == crate::state::WindowFrame::App
    }

    /// Whether the window is maximized, as the window host last saw it.
    pub fn window_maximized(&self) -> bool {
        self.redraw_handle()
            .get()
            .is_some_and(schnellui::RedrawSignal::window_maximized)
    }

    /// Asks the window to minimize, maximize or close.
    pub fn request_window(&self, request: schnellui::WindowRequest) {
        if let Some(signal) = self.redraw_handle().get() {
            signal.request_window(request);
        }
    }

    /// Switches the CPU mode of the terminals at `place`, also of those that
    /// run.
    pub fn set_cpu_mode(&self, place: crate::cpu::Place, mode: crate::state::CpuMode) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.cpu_mode.of(place) == mode {
                return;
            }
            inner.settings.cpu_mode.set(place, mode);
            inner.dirty.palette_list = true;
            inner.dirty.status = true;
            inner.dirty.settings = true;
        }
        self.save_settings();
        self.apply_cpu_modes();
    }

    /// Moves the cap of every machine the terminals run on, and this
    /// machine's, to its CPU mode, and the weights of their scopes with it.
    fn apply_cpu_modes(&self) {
        if cfg!(test) {
            return;
        }
        {
            let inner = self.0.borrow();
            let modes = inner.settings.cpu_mode;
            let mut machines = vec![Environment::Local];
            let terminals = inner.live.values().flat_map(|worktrees| worktrees.values());
            for (_, panel) in terminals.flat_map(|live| &live.panels) {
                let LivePanel::Terminal(terminal) = panel else {
                    continue;
                };
                let Some(cpu) = &terminal.cpu else {
                    continue;
                };
                // A devcontainer's cap was set inside it when it started.
                let machine = match &cpu.env {
                    Environment::Container(_) => continue,
                    env => env.base(),
                };
                if !machines.contains(machine) {
                    machines.push(machine.clone());
                }
            }
            for machine in machines {
                let share = modes.share(modes.of(crate::cpu::Place::of(&machine)));
                crate::cpu::apply_share(machine, share, modes.efficiency_cores);
            }
        }
        self.batch_hidden_sessions();
    }

    /// Turns Jujutsu on or off for every project without a choice of its
    /// own, and reads them again.
    pub fn set_jj(&self, on: bool) {
        {
            let mut inner = self.0.borrow_mut();
            if inner.settings.jj == on {
                return;
            }
            inner.settings.jj = on;
            inner.dirty.palette_list = true;
        }
        self.save_settings();
        let ids: Vec<u64> = self
            .0
            .borrow()
            .state
            .projects
            .iter()
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.reread_project(id);
        }
    }

    /// Applies completed background secret-storage operations for per-project
    /// environment variables. Returns immediately without allocating while idle.
    fn poll_project_env(&self) {
        if !self.0.borrow().project_env.has_pending() {
            return;
        }
        let applied = self.0.borrow_mut().project_env.drain();
        if applied.is_empty() {
            return;
        }
        let mut save = false;
        let mut notices = Vec::new();
        {
            let mut inner = self.0.borrow_mut();
            let tr = inner.tr;
            for event in applied {
                match event {
                    crate::project_env::Applied::Loaded {
                        project_id,
                        found_any,
                    } => {
                        if inner.settings_ui.open {
                            inner.dirty.settings = true;
                        }
                        if found_any
                            && inner.live.get(&project_id).is_some_and(|w| !w.is_empty())
                            && let Some(project) = inner.state.project(project_id)
                        {
                            notices.push((tr.project_env_loaded_notice)(&project.name));
                        }
                    }
                    crate::project_env::Applied::SavedInKeyring { name } => {
                        if inner.settings_ui.open {
                            inner.dirty.settings = true;
                            let text = (tr.toast_project_env_keyring)(&name);
                            inner.toast(false, text);
                        }
                    }
                    crate::project_env::Applied::SaveFallback {
                        project_id,
                        name,
                        value,
                        error,
                    } => {
                        if let Some(project) = inner.state.project_mut(project_id)
                            && let Some(entry) =
                                project.env_vars.iter_mut().find(|v| v.name == name)
                        {
                            entry.value = Some(value);
                            save = true;
                        }
                        if inner.settings_ui.open {
                            inner.dirty.settings = true;
                            let text = (tr.toast_project_env_fallback)(&name, &error);
                            inner.toast(true, text);
                        }
                    }
                    crate::project_env::Applied::Removed | crate::project_env::Applied::Ignored => {
                    }
                }
            }
        }
        if save {
            self.save();
        }
        for notice in notices {
            self.notify(notice);
        }
    }

    /// Sets environment variable `name` to `value` on project `id`. Stores
    /// `value` in the operating system's secret storage when available, else
    /// in `state.json`.
    pub fn set_project_env_var(&self, id: u64, name: &str, value: &str) {
        let mut inner = self.0.borrow_mut();
        let Some(project) = inner.state.project(id).cloned() else {
            return;
        };
        let in_keyring = inner.project_env.set_var(&project, name, value);
        let stored_value = (!in_keyring).then(|| value.to_owned());
        if let Some(project) = inner.state.project_mut(id) {
            if let Some(existing) = project.env_vars.iter_mut().find(|v| v.name == name) {
                existing.value = stored_value;
            } else {
                project.env_vars.push(crate::state::ProjectEnvVar {
                    name: name.to_owned(),
                    value: stored_value,
                });
            }
        }
        inner.dirty.settings = true;
        if !in_keyring {
            let text = if project.temporary {
                (inner.tr.toast_project_env_session)(name)
            } else {
                (inner.tr.toast_project_env_plain)(name)
            };
            inner.toast(false, text);
        }
        drop(inner);
        self.save();
    }

    /// Removes environment variable `name` from project `id` and its keyring
    /// entry.
    pub fn remove_project_env_var(&self, id: u64, name: &str) {
        let mut inner = self.0.borrow_mut();
        let Some(project) = inner.state.project(id).cloned() else {
            return;
        };
        let was_in_keyring = project
            .env_vars
            .iter()
            .find(|v| v.name == name)
            .is_some_and(|v| v.value.is_none());
        inner.project_env.remove_var(&project, name, was_in_keyring);
        if let Some(project) = inner.state.project_mut(id) {
            project.env_vars.retain(|v| v.name != name);
        }
        inner.dirty.settings = true;
        let text = (inner.tr.toast_project_env_removed)(name);
        inner.toast(false, text);
        drop(inner);
        self.save();
    }

    /// Retries reading project `id`'s keyring-backed environment variables.
    pub fn reload_project_env_vars(&self, id: u64) {
        let mut inner = self.0.borrow_mut();
        let Some(project) = inner.state.project(id).cloned() else {
            return;
        };
        inner.project_env.load_project(&project);
        inner.dirty.settings = true;
    }

    #[cfg(test)]
    pub(crate) fn set_project_env_backend(
        &self,
        backend: Arc<dyn crate::project_env::SecretBackend>,
    ) {
        let mut inner = self.0.borrow_mut();
        inner.project_env.set_backend(backend);
        let state = inner.state.clone();
        inner.project_env.start_initial_loads(&state);
        if let Some(id) = state.active_project {
            inner
                .project_env
                .wait_for_project(id, std::time::Duration::from_secs(2));
        }
    }

    /// Turns Jujutsu on or off for project `id` and reads it again. Choosing
    /// what the setting says drops the project's own choice, so it follows
    /// later changes of the setting.
    pub fn set_project_jj(&self, id: u64, on: bool) {
        {
            let mut inner = self.0.borrow_mut();
            let default = inner.settings.jj;
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            if project.uses_jj(default) == on {
                return;
            }
            project.jj = (on != default).then_some(on);
            inner.dirty.palette_list = true;
        }
        self.save();
        self.reread_project(id);
    }

    /// Sets the branch or revision reviews in project `id` compare against
    /// when none is named. Empty detects it. Open reviews follow.
    pub fn set_project_base(&self, id: u64, base: &str) {
        let base = base.trim();
        let base = (!base.is_empty()).then(|| base.to_owned());
        {
            let mut inner = self.0.borrow_mut();
            let Some(project) = inner.state.project_mut(id) else {
                return;
            };
            if project.base == base {
                return;
            }
            project.base.clone_from(&base);
            for live in inner.live.get(&id).into_iter().flat_map(HashMap::values) {
                for (_, panel) in &live.panels {
                    if let LivePanel::Explore(explore) = panel {
                        explore.set_project_base(base.clone());
                    }
                }
            }
        }
        self.save();
    }

    /// Asks the worker for project `id` again, with the VCS it reads now.
    /// The shown project gets a new watch, which refreshes it.
    pub(crate) fn reread_project(&self, id: u64) {
        let inner = self.0.borrow();
        let Some(project) = inner.state.project(id).filter(|p| !p.is_global()) else {
            return;
        };
        let job = if inner.state.active_project == Some(id) {
            watch(project, inner.settings.jj)
        } else {
            Job::Refresh {
                project: id,
                env: project.env.clone(),
                root: project.root.clone(),
                jj: project.uses_jj(inner.settings.jj),
                known: project.known_repo(),
            }
        };
        inner.vcs.send(job);
    }

    /// The rail's search field changed to `query`.
    pub fn set_rail_query(&self, query: String) {
        let mut inner = self.0.borrow_mut();
        if inner.rail_query == query {
            return;
        }
        // The search finds projects and worktrees, so the Projects tab is
        // the one shown while it has words: the tabs never say Agents over
        // a list of worktrees.
        if !query.trim().is_empty() {
            inner.rail_tab = RailTab::Projects;
        }
        inner.rail_query = query;
        inner.rail_highlight = 0;
        inner.dirty.sidebar = true;
    }

    /// Moves the highlighted search result by `delta`, stopping at the ends.
    pub fn move_rail_highlight(&self, delta: isize) {
        let count = {
            let inner = self.0.borrow();
            crate::search::rail_hits(&inner, &inner.rail_query).len()
        };
        let mut inner = self.0.borrow_mut();
        let last = count.saturating_sub(1);
        let next = inner.rail_highlight.saturating_add_signed(delta).min(last);
        if next != inner.rail_highlight {
            inner.rail_highlight = next;
            inner.dirty.sidebar = true;
        }
    }

    /// Puts the keyboard focus into the rail's search field.
    /// Shows `tab` in the rail.
    pub fn set_rail_tab(&self, tab: RailTab) {
        let mut inner = self.0.borrow_mut();
        if inner.rail_tab != tab {
            inner.rail_tab = tab;
            inner.dirty.sidebar = true;
        }
    }

    /// Expands or collapses the working (0) or merged (1) worktrees or the
    /// earlier sessions (2) of the Agents tab.
    pub fn toggle_agent_group(&self, group: usize) {
        let mut inner = self.0.borrow_mut();
        if let Some(open) = inner.agents_expanded.get_mut(group) {
            *open = !*open;
            inner.dirty.sidebar = true;
        }
    }

    pub fn focus_rail_search(&self) {
        let mut inner = self.0.borrow_mut();
        inner.rail_search_focus = true;
        inner.dirty.search = true;
        // A hidden rail comes back with its field.
        if inner.rail_hidden {
            inner.rail_hidden = false;
            inner.dirty.full = true;
        }
    }

    /// Sends focus from the panel to the shown tab in the tab strip, the way
    /// from a terminal, which keeps Tab for the shell, to the chrome.
    /// Pressed again, it moves on to the status bar, then to the rail's
    /// search field while the rail shows, and then back to the panel, as F6
    /// steps through a window's parts elsewhere.
    pub fn focus_tabs(&self) {
        self.step_chrome(true);
    }

    /// [`Workspace::focus_tabs`] the other way round, as Shift+F6.
    pub fn focus_chrome_back(&self) {
        self.step_chrome(false);
    }

    fn step_chrome(&self, forward: bool) {
        const STOPS: [Option<ChromeStep>; 4] = [
            None,
            Some(ChromeStep::Tabs),
            Some(ChromeStep::Status),
            Some(ChromeStep::Rail),
        ];
        let (step, rail_hidden) = {
            let inner = self.0.borrow();
            (inner.chrome_step, inner.rail_hidden)
        };
        let at = STOPS.iter().position(|stop| *stop == step).unwrap_or(0);
        let mut next = at;
        loop {
            next = if forward {
                (next + 1) % STOPS.len()
            } else {
                (next + STOPS.len() - 1) % STOPS.len()
            };
            // A hidden rail is no stop: F6 never brings it back.
            if !(rail_hidden && STOPS[next] == Some(ChromeStep::Rail)) {
                break;
            }
        }
        let next = STOPS[next];
        self.0.borrow_mut().chrome_step = next;
        match next {
            None => {
                self.refocus_panel();
            }
            Some(ChromeStep::Tabs) => self.0.borrow_mut().dirty.focus_tabs = true,
            Some(ChromeStep::Status) => self.0.borrow_mut().dirty.focus_status = true,
            Some(ChromeStep::Rail) => {
                let mut inner = self.0.borrow_mut();
                inner.rail_search_focus = true;
                inner.dirty.search = true;
            }
        }
    }

    /// Sends focus from the rail, the tab strip or the status bar back to
    /// the shown panel, as Escape there does. Returns false while an overlay
    /// is open, whose own Escape comes first.
    pub fn refocus_panel(&self) -> bool {
        if self.palette_is_open()
            || self.settings_is_open()
            || self.setup_is_open()
            || self.approval_is_open()
        {
            return false;
        }
        // Escape on a button of the notification popover closes it and
        // goes back to the bell, the way a menu returns to its button.
        if self.notifications_is_open() {
            self.close_notifications();
            // The bell is in the rail's footer, or in the tab strip while
            // the rail is hidden.
            self.0.borrow_mut().dirty.bell = true;
            return true;
        }
        let mut inner = self.0.borrow_mut();
        inner.dirty.refocus = true;
        // The next F6 starts over from the panel.
        inner.chrome_step = None;
        true
    }

    /// Grows the focused pane of a split tab by `delta` thousandths of the
    /// split around it, or shrinks it for a negative one.
    pub fn resize_pane(&self, delta: i32) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        let tab = live.tab_mut();
        if !tab.layout.resize(tab.focused, delta) {
            return;
        }
        inner.dirty.main = true;
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Gives the split between pane `boundary` of the shown tab and the pane
    /// after it `share` thousandths of its room for its first half, the end
    /// of a drag of the line between them.
    pub fn set_pane_share(&self, boundary: usize, share: u16) {
        let mut inner = self.0.borrow_mut();
        let Some(live) = inner.active_live_mut() else {
            return;
        };
        if !live.tab_mut().layout.set_share(boundary, share) {
            return;
        }
        inner.dirty.main = true;
        drop(inner);
        self.resize_active();
        self.save();
    }

    /// Hides the rail, so the panel takes the whole width, or shows it
    /// again. Every region changes width, so the app remounts.
    pub fn toggle_sidebar(&self) {
        let mut inner = self.0.borrow_mut();
        inner.rail_hidden = !inner.rail_hidden;
        inner.rail_auto_hidden = false;
        inner.rail_search_focus = false;
        inner.dirty.full = true;
    }

    /// Hides the rail in a window too narrow for it beside the panes, and
    /// shows it again when the window widens, if it hid it itself.
    pub fn fit_rail_to_window(&self, width: f32) {
        let mut inner = self.0.borrow_mut();
        if width < crate::ui::RAIL_HIDES_BELOW {
            if !inner.rail_hidden {
                inner.rail_hidden = true;
                inner.rail_auto_hidden = true;
                inner.rail_search_focus = false;
            }
        } else if inner.rail_auto_hidden {
            inner.rail_hidden = false;
            inner.rail_auto_hidden = false;
        }
    }

    /// Width the rail takes: none while hidden.
    pub fn rail_width(&self) -> f32 {
        if self.0.borrow().rail_hidden {
            0.0
        } else {
            crate::ui::rail_width()
        }
    }

    /// Esc in the rail's search field: empties a filled field and keeps
    /// the focus there, as the settings search does; an empty field gives
    /// the focus back to the panel.
    pub fn escape_rail_search(&self) {
        let mut inner = self.0.borrow_mut();
        if inner.rail_query.is_empty() {
            drop(inner);
            self.clear_rail_search();
            return;
        }
        inner.rail_query.clear();
        inner.rail_highlight = 0;
        inner.rail_search_focus = true;
        inner.dirty.search = true;
        inner.dirty.sidebar = true;
    }

    /// Empties the rail's search field and gives the focus back to the
    /// panel.
    pub fn clear_rail_search(&self) {
        let mut inner = self.0.borrow_mut();
        inner.rail_query.clear();
        inner.rail_highlight = 0;
        inner.rail_search_focus = false;
        inner.dirty.search = true;
        inner.dirty.sidebar = true;
    }

    /// Opens the highlighted search result and clears the search.
    pub fn open_rail_highlight(&self) {
        let hit = {
            let inner = self.0.borrow();
            let hits = crate::search::rail_hits(&inner, &inner.rail_query);
            let index = inner.rail_highlight.min(hits.len().saturating_sub(1));
            hits.into_iter().nth(index)
        };
        if let Some(hit) = hit {
            self.open_search_hit(hit.project, &hit.path);
        }
    }

    /// Shows the worktree a search result names and clears the search.
    pub fn open_search_hit(&self, project: u64, path: &Path) {
        self.clear_rail_search();
        self.activate_worktree(project, path);
    }

    fn poll_exits(&self) {
        let mut exited = Vec::new();
        let mut failed = Vec::new();
        {
            let mut inner = self.0.borrow_mut();
            let titles_changed = inner.active_live().is_some_and(|live| {
                live.panels
                    .iter()
                    .fold(false, |changed, (_, panel)| match panel {
                        LivePanel::Terminal(terminal) => terminal.title_changed() | changed,
                        LivePanel::Files(files) => files.title_changed() | changed,
                        LivePanel::Explore(explore) => explore.title_changed() | changed,
                        LivePanel::Browser(_)
                        | LivePanel::Monitor(_)
                        | LivePanel::DebugBrowser(_)
                        | LivePanel::Tasks(_) => changed,
                    })
            });
            inner.dirty.tabs |= titles_changed;
            // An explore panel turned to or from its change request page,
            // whose native view is built again, or that page changed.
            let remount = inner.active_live().is_some_and(|live| {
                live.panels
                    .iter()
                    .fold(false, |remount, (_, panel)| match panel {
                        LivePanel::Explore(explore) => explore.take_remount() | remount,
                        _ => remount,
                    })
                    // The tasks page is widgets, and so are the monitor's
                    // labels: new lines or labels that moved mount them
                    // again while a pane of the shown tab holds them.
                    | (0..live.layout().len()).fold(false, |remount, pane| {
                        match live.pane_panel(pane) {
                            Some((_, LivePanel::Tasks(tasks))) => tasks.take_remount() | remount,
                            Some((_, LivePanel::Monitor(monitor))) => {
                                monitor.take_remount() | remount
                            }
                            _ => remount,
                        }
                    })
            });
            inner.dirty.main |= remount;
            let dirs_changed = inner.active_live().is_some_and(|live| {
                live.panels
                    .iter()
                    .fold(false, |changed, (_, panel)| match panel {
                        LivePanel::Terminal(terminal) => terminal.dir_changed() | changed,
                        LivePanel::Browser(_)
                        | LivePanel::Files(_)
                        | LivePanel::Explore(_)
                        | LivePanel::Monitor(_)
                        | LivePanel::DebugBrowser(_)
                        | LivePanel::Tasks(_) => changed,
                    })
            });
            inner.dirty.status |= dirs_changed;
            let paste_error = inner.active_live().and_then(|live| {
                live.panels.iter().find_map(|(_, panel)| match panel {
                    LivePanel::Terminal(terminal) => terminal.take_paste_error(),
                    _ => None,
                })
            });
            if let Some(error) = paste_error {
                let text = (inner.tr.paste_image_failed)(&error);
                inner.push_notice(text);
            }
            let inner = &*inner;
            for live in inner.live.values().flat_map(HashMap::values) {
                for (id, panel) in &live.panels {
                    if let LivePanel::Terminal(terminal) = panel
                        && let Some(code) = terminal.session.exit_code()
                    {
                        exited.push((*id, code));
                        if code != Some(0) && terminal.started.elapsed() < FAILED_START {
                            let output = terminal.last_line().unwrap_or_default();
                            failed.push((inner.tr.terminal_failed_start)(code, &output));
                        }
                    }
                }
            }
        }
        if exited.is_empty() {
            return;
        }
        // A terminal whose program exited closes its pane, in whichever
        // worktree it runs. Removing it first announces each exit once.
        {
            let mut inner = self.0.borrow_mut();
            let active = inner.active_key();
            let mut shown = false;
            for (project, worktrees) in &mut inner.live {
                for (path, live) in worktrees {
                    let mut removed = false;
                    for (id, _) in &exited {
                        removed |= live.remove_panel(*id);
                    }
                    shown |= removed
                        && active
                            .as_ref()
                            .is_some_and(|(p, w)| p == project && w == path);
                }
            }
            inner.dirty.tabs = true;
            inner.dirty.sidebar = true;
            inner.dirty.main |= shown;
            for text in failed {
                inner.push_notice(text);
            }
        }
        self.resize_active();
        self.save();
        for (id, code) in exited {
            self.emit(&Event::TerminalExited { id, code });
        }
    }

    /// Runs `job`, work an extension's request or answer started. A panic
    /// in it is a bug in the app, not in the extension; it shows as a notice
    /// instead of ending the app, since no extension may take the app down
    /// (SOUL principle 7). Unwinding releases the borrows `job` held.
    pub(crate) fn contain(&self, job: impl FnOnce()) {
        let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)) else {
            return;
        };
        let message = (payload
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned()))
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
        if let Ok(mut inner) = self.0.try_borrow_mut() {
            let text = (inner.tr.extension_request_failed)(&message);
            inner.push_notice(text);
        }
    }

    fn apply_requests(&self) {
        let mut requests = std::mem::take(&mut self.0.borrow_mut().requests);
        self.0.borrow_mut().ext.drain_requests(&mut requests);
        for request in requests.drain(..) {
            self.contain(|| match request {
                Request::OpenTerminal {
                    cwd,
                    command,
                    title,
                } => self.open_terminal_in(cwd, command, title),
                Request::OpenBrowser { url } => self.open_browser(&url),
                Request::Notify { level, message } => self.notify(match level {
                    NotifyLevel::Info => message,
                    NotifyLevel::Warning => (self.tr().warning)(&message),
                    NotifyLevel::Error => (self.tr().error)(&message),
                }),
                Request::SendText { terminal, text } => {
                    let target = match terminal {
                        Some(id) => self.terminal_by_id(id),
                        None => self.active_terminal(),
                    };
                    if let Some(target) = target {
                        let _ = target.session.write_bytes(text.as_bytes());
                    }
                }
                Request::Copy {
                    from,
                    to,
                    worktree,
                    exclude,
                    done,
                } => self.copy_into_env(from, to, worktree, exclude, done),
                Request::Run {
                    command,
                    worktree,
                    cwd,
                    on_host,
                    done,
                } => self.run_in_env(command, worktree, cwd, on_host, done),
                Request::Trust {
                    name,
                    path,
                    worktree,
                    done,
                } => self.trust_in_env(name, path, worktree, done),
                Request::OpenView { view, key } => self.open_view(view, key),
                Request::RefreshView { view, key } => {
                    self.refresh_views(Some(&view), key.as_deref());
                }
                Request::Http(request) => self.forge_http(request),
                Request::ForgeReport {
                    forge,
                    results,
                    retry_after,
                } => self.forge_report(&forge, results, retry_after),
                Request::ForgeDiscussion { id, discussion, .. } => {
                    self.forge_discussion_done(&id, discussion);
                }
                Request::SwitchWorktree { path } => {
                    let found = self.0.borrow().find_worktree(&path);
                    match found {
                        Some(id) => self.activate_worktree(id, &path),
                        None => {
                            self.notify((self.tr().no_worktree_at)(&path.display().to_string()))
                        }
                    }
                }
            });
        }
        self.0.borrow_mut().requests = requests;
    }

    /// Runs the tool calls agents made, publishes changed tools and renders
    /// views the user asked to render again. Allocation-free when nothing changed.
    fn poll_extension_ui(&self) {
        let control = self.0.borrow().running_control();
        if let Some(control) = &control {
            for mut call in control.take_tool_calls() {
                call.caller.project_id = call.project.and_then(|id| {
                    let inner = self.0.borrow();
                    let project = inner.state.project(id)?;
                    Some(project_storage_id(
                        &project.name,
                        &project.env,
                        &project.root,
                    ))
                });
                let result =
                    self.0
                        .borrow_mut()
                        .ext
                        .call_tool(&call.name, &call.arguments, &call.caller);
                let result = match result {
                    Ok(result) => result,
                    Err(error) => {
                        self.notify(error.to_string());
                        Err(error.message)
                    }
                };
                control.answer_tool(call.id, result);
            }
        }
        {
            let mut inner = self.0.borrow_mut();
            let inner = &mut *inner;
            let tools = inner.ext.tools_revision();
            if tools != inner.ext_tools_seen {
                inner.ext_tools_seen = tools;
                if let Some(control) = &control {
                    control.publish_tools(inner.ext.tools());
                }
            }
        }
        let agent_errors = {
            let mut inner = self.0.borrow_mut();
            let agents = inner.ext.agents_revision();
            if agents == inner.ext_agents_seen {
                Vec::new()
            } else {
                inner.ext_agents_seen = agents;
                inner.dirty.settings = true;
                crate::agents::set_custom(inner.ext.owner(), inner.ext.agents())
            }
        };
        for error in agent_errors {
            self.notify(error.to_string());
        }
        let wanted = self.with_active(|live| {
            live.and_then(|live| {
                live.panels.iter().find_map(|(_, panel)| match panel {
                    LivePanel::Files(files) if files.take_rerender() => files
                        .view_source()
                        .map(|(view, key)| (view.to_owned(), key.to_owned())),
                    _ => None,
                })
            })
        });
        if let Some((view, key)) = wanted {
            if view == crate::forges::DISCUSSION_VIEW {
                self.0.borrow_mut().ask_discussion(&key);
            }
            self.refresh_views(Some(&view), Some(&key));
        }
        // An explore panel that turned to its change request page, or
        // reloads it.
        let asking = self.with_active(|live| {
            live?.panels.iter().find_map(|(_, panel)| match panel {
                LivePanel::Explore(explore) => explore
                    .take_ask_change()
                    .map(|again| (explore.clone(), again)),
                _ => None,
            })
        });
        if let Some((explore, again)) = asking {
            let mut inner = self.0.borrow_mut();
            let Some(key) = inner
                .state
                .active()
                .map(|p| crate::forges::discussion_key(p.id, p.current_worktree()))
            else {
                return;
            };
            if again {
                inner.ask_discussion(&key);
            }
            let page = inner.change_page(&key);
            let tr = inner.tr;
            drop(inner);
            explore.set_change(&page, tr);
        }
    }

    /// Shows view `view` with `key` in the active worktree: focuses the
    /// panel that shows it already, else opens one in a new tab.
    pub fn open_view(&self, view: String, key: String) {
        if view != crate::forges::DISCUSSION_VIEW && !self.0.borrow().ext.has_view(&view) {
            self.notify((self.tr().view_unknown)(&view));
            return;
        }
        let found = self.with_active(|live| {
            let live = live?;
            let (id, _) = live.panels.iter().find(|(_, panel)| {
                matches!(panel, LivePanel::Files(files)
                    if files.view_source() == Some((view.as_str(), key.as_str())))
            })?;
            live.tabs
                .iter()
                .enumerate()
                .find_map(|(index, tab)| tab.layout.position(*id).map(|pane| (index, pane)))
        });
        match found {
            Some((tab, pane)) => {
                self.select_tab(tab);
                let mut inner = self.0.borrow_mut();
                if let Some(live) = inner.active_live_mut() {
                    live.tab_mut().focused = pane;
                }
                inner.dirty.main = true;
                drop(inner);
                self.refresh_views(Some(&view), Some(&key));
            }
            None => self.open(PanelSpec::View { view, key }),
        }
    }

    /// Renders the extension view panels of every worktree again: all of
    /// them, those of `view`, or the one of `view` and `key`.
    pub fn refresh_views(&self, view: Option<&str>, key: Option<&str>) {
        let panels: Vec<Rc<FilesPanel>> = {
            let inner = self.0.borrow();
            inner
                .live
                .values()
                .flat_map(HashMap::values)
                .flat_map(|live| &live.panels)
                .filter_map(|(_, panel)| match panel {
                    LivePanel::Files(files) => {
                        let (v, k) = files.view_source()?;
                        let wanted =
                            view.is_none_or(|view| view == v) && key.is_none_or(|key| key == k);
                        wanted.then(|| files.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        for panel in panels {
            self.0.borrow_mut().render_view(&panel);
        }
    }

    fn terminal_by_id(&self, panel_id: u64) -> Option<Rc<TerminalPanel>> {
        let inner = self.0.borrow();
        inner
            .live
            .values()
            .flat_map(HashMap::values)
            .find_map(|live| {
                live.panels.iter().find_map(|(id, panel)| match panel {
                    LivePanel::Terminal(terminal) if *id == panel_id => Some(terminal.clone()),
                    _ => None,
                })
            })
    }

    /// Writes the panel layout of every running worktree into the state and
    /// saves it. Called after structural changes, never per frame.
    pub fn save(&self) {
        let mut inner = self.0.borrow_mut();
        let inner = &mut *inner;
        for (id, worktrees) in &inner.live {
            let Some(project) = inner.state.project_mut(*id) else {
                continue;
            };
            for (path, live) in worktrees {
                let config = project.worktree_mut(path);
                config.panels = live.panels.iter().map(|(_, panel)| panel.spec()).collect();
                let tabs = live
                    .tabs
                    .iter()
                    .filter_map(|tab| {
                        Some(TabConfig {
                            layout: tab.layout.filter_map(&mut |id| live.index_of(id))?,
                            focused: tab.focused,
                        })
                    })
                    .collect();
                config.set_tabs(tabs, live.active);
            }
        }
        if let Err(error) = inner.store.save(&inner.state) {
            let text = (inner.tr.state_not_saved)(&error.to_string());
            inner.push_notice(text);
        }
    }
}

/// Nothing polls an idle window, so wake it once a new notice is due. The
/// signal is read when the timer fires, so notices queued before the window
/// installed its signal expire too.
/// The job that polls `project` while it is shown. A global session has no
/// repository, so the worker stops polling the one shown before it.
/// `jj` is [`crate::state::Settings::jj`].
fn watch(project: &crate::state::Project, jj: bool) -> Job {
    if project.is_global() {
        return Job::Unwatch;
    }
    Job::Watch {
        project: project.id,
        env: project.env.clone(),
        root: project.root.clone(),
        jj: project.uses_jj(jj),
        known: project.known_repo(),
    }
}

/// Where a new worktree goes: `<repo>-<suffix>` beside the main worktree.
/// The runtime of worktree `path`: its project's choice, else the default
/// from the settings, else [`Runtime::default`], the sandbox except on
/// Windows. A devcontainer is skipped where the worktree has no
/// `devcontainer.json`. Blocking: looking for one runs a command on a
/// machine that is not this one.
pub(crate) fn new_worktree_runtime(
    project: Option<Runtime>,
    default: Runtime,
    env: &Environment,
    path: &Path,
) -> Runtime {
    let mut has_devcontainer = None;
    for runtime in project.into_iter().chain([default]) {
        if runtime != Runtime::Devcontainer
            || *has_devcontainer
                .get_or_insert_with(|| matches!(crate::container::find(env, path), Ok(Some(_))))
        {
            return runtime;
        }
    }
    Runtime::default()
}

/// `revision` as part of a directory name, such as `origin-main` for
/// `origin/main`.
fn folder_name(revision: &str) -> String {
    let name: String = (revision.chars())
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let name = name.trim_matches(['-', '.']);
    if name.is_empty() {
        "detached".to_owned()
    } else {
        name.to_owned()
    }
}

fn sibling_path(root: &Path, suffix: &str) -> PathBuf {
    let dir_name = format!(
        "{}-{suffix}",
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    root.parent()
        .map_or_else(|| PathBuf::from(&dir_name), |parent| parent.join(&dir_name))
}

fn arm_notice_expiry(wake: Arc<OnceLock<RedrawSignal>>) {
    arm_expiry(wake, NOTICE_TTL);
}

/// Wakes the window once `after` has passed, so whatever expires then goes.
pub(crate) fn arm_expiry(wake: Arc<OnceLock<RedrawSignal>>, after: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after + Duration::from_millis(50));
        if let Some(signal) = wake.get() {
            signal.request_redraw();
        }
    });
}

/// Where [`Workspace::push_panel`] puts a new panel.
#[derive(Clone, Copy)]
enum Place {
    /// A new tab after the others, which is shown.
    NewTab,
    /// A new pane beside the focused one in the shown tab, which takes focus.
    Split(SplitAxis),
}

/// `index` moved by `delta` in a ring of `count` places.
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn wrap(index: usize, delta: isize, count: usize) -> usize {
    (index as isize + delta).rem_euclid(count as isize) as usize
}

/// Cell metrics of the terminal font the settings ask for.
fn measure(shaper: &mut TextShaper, settings: &Settings) -> TerminalCellMetrics {
    TerminalCellMetrics::measure_with_family(
        shaper,
        settings.font_size,
        1.0,
        settings.font_family.as_deref(),
    )
}

impl Inner {
    pub fn system_dark(&self) -> bool {
        self.system_dark.load(Ordering::Acquire)
    }

    /// How long an agent may sit idle in a hidden terminal before it
    /// freezes: `freeze_idle_agents`, shortened while memory runs low.
    /// `None` when the setting turns freezing off.
    pub fn freeze_after(&self) -> Option<Duration> {
        let minutes = self.settings.freeze_idle_agents;
        if minutes == 0 {
            return None;
        }
        let configured = Duration::from_secs(u64::from(minutes) * 60);
        let pressure = crate::memory::Pressure::load(&self.memory_pressure);
        Some(pressure.freeze_after(configured))
    }

    /// What terminal views draw with: cell size, selection color, family.
    pub fn grid_style(&self) -> GridStyle {
        GridStyle {
            metrics: self.metrics,
            selection: self.scheme.selection,
            font_family: self.settings.font_family.clone(),
        }
    }

    #[cfg(test)]
    pub fn ext_context_for_tests(&self) -> ExtContext {
        self.ext_context()
    }

    /// The snapshot extensions read through `term.context()`.
    fn ext_context(&self) -> ExtContext {
        let Some(project) = self.state.active() else {
            return ExtContext::default();
        };
        let path = project.current_worktree();
        let branch = self.snapshots.get(&project.id).and_then(|snapshot| {
            snapshot
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .and_then(|worktree| worktree.branch.clone())
        });
        let runtime = project
            .worktree(path)
            .map(|config| config.runtime)
            .unwrap_or_default();
        ExtContext {
            project: Some(project.name.clone()),
            project_id: Some(project_storage_id(
                &project.name,
                &project.env,
                &project.root,
            )),
            worktree: Some(path.to_path_buf()),
            branch,
            env: project.env.label().to_owned(),
            runtime: crate::runtime::runtime_name(runtime).to_owned(),
        }
    }

    pub(crate) fn push_notice(&mut self, text: String) {
        if self.notices.len() == MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notifications
            .push(crate::notifications::Topic::Notice(text.clone()), true);
        self.notices.push_back(Notice::new(text));
        self.dirty.status = true;
        arm_notice_expiry(self.wake.clone());
    }

    fn expire_notices(&mut self, now: Instant) {
        if let Some(newest) = self.notices.back()
            && now.saturating_duration_since(newest.at) >= NOTICE_TTL
        {
            self.notices.clear();
            self.dirty.status = true;
        }
    }

    /// Branch of the shown worktree, when the snapshot knows it.
    pub fn active_branch(&self) -> Option<&str> {
        let project = self.state.active()?;
        let current = project.current_worktree();
        self.snapshots
            .get(&project.id)?
            .worktrees
            .iter()
            .find(|worktree| worktree.path == current)?
            .branch
            .as_deref()
    }

    /// The most urgent attention state among the terminals of one worktree,
    /// with the message of the terminal that has it.
    pub fn worktree_attention(&self, project: u64, path: &Path) -> (Attention, String) {
        self.live_get(project, path)
            .map(LiveWorktree::attention)
            .unwrap_or_default()
    }

    /// The most urgent attention state among every worktree of `project`,
    /// and how many worktrees have it.
    pub fn project_attention(&self, project: u64) -> (Attention, usize) {
        let Some(worktrees) = self.live.get(&project) else {
            return (Attention::None, 0);
        };
        worktrees.values().map(|live| live.attention().0).fold(
            (Attention::None, 0),
            |(best, count), state| match state.cmp(&best) {
                std::cmp::Ordering::Greater => (state, 1),
                std::cmp::Ordering::Equal if state != Attention::None => (best, count + 1),
                _ => (best, count),
            },
        )
    }

    /// Whether a terminal of `path` in project `id` has `agent`'s session
    /// open.
    pub fn agent_open(&self, id: u64, path: &Path, agent: &crate::state::AgentSession) -> bool {
        self.live_get(id, path)
            .is_some_and(|live| live.session_panel(agent).is_some())
    }

    /// Rebuilds the rail when it shows the Agents tab, whose rows mark the
    /// agent in the focused pane, after the focused pane or tab changed.
    fn mark_agent_focus(&mut self) {
        if self.rail_tab == RailTab::Agents {
            self.dirty.sidebar = true;
        }
    }

    pub(crate) fn live_get(&self, id: u64, path: &Path) -> Option<&LiveWorktree> {
        self.live.get(&id)?.get(path)
    }

    /// The running panels of the shown worktree. Allocation-free.
    pub fn active_live(&self) -> Option<&LiveWorktree> {
        let project = self.state.active()?;
        self.live_get(project.id, project.current_worktree())
    }

    /// Columns and rows of every pane of the shown tab. A lone pane gets the
    /// whole panel grid.
    fn pane_grids(&self) -> Vec<(u16, u16)> {
        match self.active_live() {
            Some(live) if live.layout().len() > 1 => self
                .pane_sizes
                .iter()
                .copied()
                .map(|size| grid_for(size, self.metrics))
                .collect(),
            _ => vec![self.grid],
        }
    }

    /// Columns and rows of every pane of `layout` laid out in the panel area.
    /// `sizes` is scratch space.
    fn grids_for(&self, layout: &PaneLayout<u64>, sizes: &mut Vec<Size>) -> Vec<(u16, u16)> {
        if layout.len() == 1 {
            return vec![self.grid];
        }
        pane_sizes(layout, self.panel_size, sizes);
        sizes
            .iter()
            .map(|size| grid_for(*size, self.metrics))
            .collect()
    }

    /// Columns and rows of the focused pane of the shown worktree.
    pub fn pane_grid(&self) -> (u16, u16) {
        let focused = self.active_live().map_or(0, LiveWorktree::focused);
        self.pane_grids().get(focused).copied().unwrap_or(self.grid)
    }

    fn active_live_mut(&mut self) -> Option<&mut LiveWorktree> {
        let project = self.state.active()?;
        let id = project.id;
        let path = project
            .active_worktree
            .as_deref()
            .unwrap_or(&project.root)
            .to_path_buf();
        self.live.get_mut(&id)?.get_mut(&path)
    }

    pub fn active_key(&self) -> Option<(u64, PathBuf)> {
        let project = self.state.active()?;
        Some((project.id, project.current_worktree().to_path_buf()))
    }

    /// The window's redraw signal, for worker threads to wake it.
    pub(crate) fn wake_handle(&self) -> Arc<OnceLock<RedrawSignal>> {
        self.wake.clone()
    }

    /// Where the VCS of the worktree at `path` runs, and whether it is read
    /// with Jujutsu where it has a jj repository.
    fn vcs_env(&self, path: &Path) -> (Environment, bool, Option<crate::vcs::KnownRepo>) {
        self.find_worktree(path)
            .and_then(|project| self.state.project(project))
            .map_or((Environment::Local, self.settings.jj, None), |project| {
                let jj = project.uses_jj(self.settings.jj);
                (project.env.clone(), jj, project.known_repo())
            })
    }

    pub(crate) fn find_worktree(&self, path: &Path) -> Option<u64> {
        self.snapshots
            .iter()
            .find(|(_, snapshot)| snapshot.worktrees.iter().any(|w| w.path == path))
            .map(|(id, _)| *id)
            .or_else(|| {
                self.state
                    .projects
                    .iter()
                    .find(|project| project.root == path)
                    .map(|project| project.id)
            })
    }

    /// Renders an extension view panel with its extension's `render`.
    fn render_view(&mut self, panel: &FilesPanel) {
        let Some((view, key)) = panel.view_source() else {
            return;
        };
        if view == crate::forges::DISCUSSION_VIEW {
            self.render_discussion(panel, key);
            self.dirty.tabs = true;
            return;
        }
        let tr = self.tr;
        match self.ext.render_view(view, key) {
            Ok(Some(page)) => {
                let title = if page.title.is_empty() {
                    key
                } else {
                    &page.title
                };
                panel.show_markdown(title, &page.markdown);
            }
            Ok(None) => panel.show_message(key, (tr.view_gone)(view)),
            Err(error) => {
                panel.show_message(key, error.to_string());
                self.push_notice(error.to_string());
            }
        }
        self.dirty.tabs = true;
    }

    /// The control socket if it runs, without starting it.
    pub(crate) fn running_control(&self) -> Option<Arc<crate::control::Control>> {
        self.control_ref().cloned()
    }

    /// The control socket if it runs, borrowed. Allocation-free.
    pub(crate) fn control_ref(&self) -> Option<&Arc<crate::control::Control>> {
        Some(&self.control.as_ref()?.as_ref()?.control)
    }

    /// The oldest settings change an agent suggested, if one waits.
    pub fn pending_suggestion(&self) -> Option<crate::control::Suggestion> {
        self.running_control()?.pending_suggestion()
    }

    /// Hands the control socket the settings and what sandboxes may reach,
    /// for `GET /v1/sandbox` and for checking suggestions. Cold: after a
    /// settings change, an extension reload or a new sandbox rule.
    /// Hands the default shell of the settings to `cmdterm-env`, for
    /// terminals started from now on. Windows only: elsewhere terminals
    /// start `$SHELL`.
    pub(crate) fn apply_default_shell(&self) {
        if cfg!(windows) {
            let argv = (self.settings.shell.as_ref())
                .and_then(|choice| choice.resolve(&self.shells))
                .map(|(_, argv)| argv);
            cmdterm_env::set_default_shell(argv);
        }
    }

    pub(crate) fn publish_environment(&self) {
        let Some(control) = self.running_control() else {
            return;
        };
        let rules = self.ext.sandbox_rules();
        let sandbox = &self.settings.sandbox;
        let guards: Vec<serde_json::Value> = crate::runtime::all_guards(rules)
            .iter()
            .map(|guard| {
                serde_json::json!({
                    "id": guard.id,
                    "title": guard.title,
                    "hosts": guard.hosts,
                    "kind": guard.kind,
                    "mode": sandbox.guard_mode(&guard.id, guard.kind),
                })
            })
            .collect();
        let environment = serde_json::json!({
            "settings": {
                "sandbox": sandbox,
                "default_runtime": self.settings.default_runtime,
                "container_engine": self.settings.container_engine,
                "agent_control": self.settings.agent_control,
            },
            "network": if !sandbox.network {
                "none"
            } else if sandbox.restrict_network {
                "allowed hosts only, through the proxy"
            } else if sandbox.host_network {
                "everything"
            } else {
                crate::control::NETWORK_ANY_HOST_THROUGH_PROXY
            },
            // Where servers in a local Linux bubblewrap sandbox listen: on a
            // loopback of the sandbox's own (forwarded with /v1/ports) or on
            // the host's. The control socket corrects both fields for
            // terminals elsewhere (macOS, SSH, remote machines).
            "loopback": if crate::runtime::isolates(sandbox) { "sandbox" } else { "host" },
            "allowed_hosts": crate::runtime::allowed_hosts(sandbox, rules),
            "from_extensions": {
                "hosts": rules.hosts,
                "writable": rules.writable,
                "hidden": rules.hidden,
                "blocked": rules.blocked,
                "unblocked": rules.unblocked,
                "blocklists": rules.blocklists.iter().map(|list| &list.id).collect::<Vec<_>>(),
            },
            "guards": guards,
            "docs": self.dirs.data.join("CONFIGURATION.md"),
        });
        let settings = serde_json::to_value(&self.settings).unwrap_or_default();
        control.publish_settings(settings, environment, self.runtimes.refusals());
        control.publish_ports(Some(self.runtimes.ports.clone()));
    }

    /// The oldest launch an agent asked for, if one waits.
    pub fn pending_launch(&self) -> Option<crate::control::Launch> {
        self.running_control()?.pending_launch()
    }

    /// The oldest extension change an agent asked for, if one waits.
    pub fn pending_change(&self) -> Option<crate::control::Change> {
        self.running_control()?.pending()
    }

    /// The control socket, started on first use. A socket that cannot
    /// start is reported once.
    pub(crate) fn control(&mut self) -> Option<Arc<crate::control::Control>> {
        if self.control.is_none() {
            let wake = self.wake.clone();
            let started =
                crate::control::start(&self.extensions_dir, &self.dirs.state, move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
            match started {
                Ok(server) => {
                    let status = extension_status(&self.ext, &self.settings);
                    server.control.publish(status);
                    self.control = Some(Some(server));
                    self.publish_environment();
                }
                Err(error) => {
                    self.push_notice((self.tr.control_failed)(&error.to_string()));
                    self.control = Some(None);
                }
            }
        }
        Some(self.control.as_ref()?.as_ref()?.control.clone())
    }

    /// What a terminal in `env` for `worktree` gets of the control socket:
    /// its token, the variables, and the socket to forward. Every terminal
    /// gets `SCRIPTSCHNELL_TERM` and [`crate::term::identity::VARS`]; the
    /// socket and the token only where the `agent_control` setting and the
    /// environment let it reach the socket. `saved` is what a remote
    /// session's shell got at an earlier start: its socket name and token
    /// stay, so that shell still reaches the socket.
    #[allow(clippy::type_complexity)]
    pub(crate) fn terminal_control(
        &mut self,
        env: &Environment,
        worktree: &Path,
        saved: Option<&crate::state::SessionControl>,
    ) -> (
        Option<crate::control::Registration>,
        Vec<(std::ffi::OsString, std::ffi::OsString)>,
        Vec<(PathBuf, PathBuf)>,
        Option<crate::state::SessionControl>,
    ) {
        use crate::control::{Registration, Terminal, VAR_MARKER, VAR_SOCKET, VAR_TOKEN};
        let mut vars = vec![(VAR_MARKER.into(), "1".into())];
        vars.extend(
            crate::term::identity::VARS
                .iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        let reach = match self.settings.agent_control {
            AgentControl::On => true,
            AgentControl::Local => env.base().is_local(),
            AgentControl::Host => matches!(env, Environment::Local),
            AgentControl::Off => false,
        };
        let Some(control) = reach.then(|| self.control()).flatten() else {
            return (None, vars, Vec::new(), None);
        };
        let local = control.socket().to_path_buf();
        let saved = saved.filter(|saved| crate::control::valid_socket_name(&saved.socket));
        let name = saved.map_or_else(crate::control::socket_name, |saved| saved.socket.clone());
        let Some(inside) = env.forwarded_socket(&local, &name) else {
            return (None, vars, Vec::new(), None);
        };
        if !self.forwards_to(env, &local) {
            return (None, vars, Vec::new(), None);
        }
        let project_id = self.find_worktree(worktree);
        let project = (project_id.and_then(|id| self.state.project(id)))
            .map(|project| project.name.clone())
            .unwrap_or_default();
        let terminal = Terminal {
            project_id,
            project,
            worktree: worktree.to_path_buf(),
            machine: env.base().label().to_owned(),
            kind: match env {
                Environment::Sandbox(_) => "sandbox",
                Environment::Container(_) => "container",
                Environment::Local => "local",
                Environment::Ssh(_) => "ssh",
                Environment::Remote(_) => "remote",
                Environment::Vm(_) => "vm",
                Environment::Custom(_) => "custom",
            },
        };
        let token = saved.map(|saved| saved.token.clone());
        let registration = Registration::new(&control, terminal, token);
        vars.push((VAR_SOCKET.into(), inside.clone().into_os_string()));
        vars.push((VAR_TOKEN.into(), registration.token().into()));
        let seat = crate::state::SessionControl {
            socket: name,
            token: registration.token().to_owned(),
        };
        (Some(registration), vars, vec![(local, inside)], Some(seat))
    }

    /// The control socket's directory as a mount for a new devcontainer on
    /// `host`, at [`crate::control::CONTAINER_DIR`]. Only containers of this
    /// machine's engines get it.
    pub(crate) fn control_mounts(&mut self, host: &Environment) -> Vec<(PathBuf, PathBuf)> {
        if !host.is_local()
            || matches!(
                self.settings.agent_control,
                AgentControl::Off | AgentControl::Host
            )
        {
            return Vec::new();
        }
        let Some(control) = self.control() else {
            return Vec::new();
        };
        let Some(dir) = control.socket().parent() else {
            return Vec::new();
        };
        vec![(
            dir.to_path_buf(),
            PathBuf::from(crate::control::CONTAINER_DIR),
        )]
    }

    /// Whether terminals in `env` should forward the control socket. SSH
    /// hosts are checked once per run, because ssh answers a refused forward
    /// with a warning in the terminal and a connection of its own. The first
    /// check waits up to two seconds; a terminal that opens before it ends
    /// goes without the socket.
    fn forwards_to(&self, env: &Environment, local: &Path) -> bool {
        let base = env.base();
        if !matches!(base, Environment::Ssh(_)) {
            return true;
        }
        let Ok(mut known) = self.forwarding.lock() else {
            return false;
        };
        match known.get(base) {
            Some(Some(ok)) => return *ok,
            Some(None) => return false,
            None => {}
        }
        known.insert(base.clone(), None);
        drop(known);
        let (done, answer) = mpsc::channel();
        let (forwarding, base, local) =
            (self.forwarding.clone(), base.clone(), local.to_path_buf());
        let _ = std::thread::Builder::new()
            .name("control-probe".into())
            .spawn(move || {
                let probe = PathBuf::from(format!("/tmp/{}.sock", crate::control::socket_name()));
                let result = base.can_forward_socket(&local, &probe);
                if let Ok(mut known) = forwarding.lock() {
                    match result {
                        Some(ok) => known.insert(base, Some(ok)),
                        // Could not tell: ask again with the next terminal.
                        None => known.remove(&base),
                    };
                }
                let _ = done.send(result == Some(true));
            });
        answer
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap_or(false)
    }

    /// The title `worktree` saved for `agent`'s session.
    fn saved_agent_title(
        &self,
        worktree: &Path,
        agent: &crate::state::AgentSession,
    ) -> Option<&str> {
        self.state
            .projects
            .iter()
            .filter_map(|project| project.worktree(worktree))
            .flat_map(|config| &config.agent_sessions)
            .find(|record| record.is(agent))?
            .title
            .as_deref()
    }

    /// Effective environment variables for terminals in `(id, path)`: the
    /// project's variables followed by the worktree's, with worktree values
    /// taking precedence.
    pub(crate) fn worktree_env_vars(&self, id: u64, path: &Path) -> Vec<(String, String)> {
        let Some(project) = self.state.project(id) else {
            return Vec::new();
        };
        let project_vars = self.project_env.resolve_project(project);
        let worktree_vars = project
            .worktree(path)
            .map_or(&[][..], |config| &config.env_vars);
        crate::project_env::merge_env_vars(project_vars, worktree_vars)
    }

    /// Starts the saved panels of `path` of project `id` from `config`,
    /// unless they already run.
    fn start_live(&mut self, id: u64, path: &Path, config: &crate::state::WorktreeConfig) {
        if self.live_get(id, path).is_some() {
            return;
        }
        let term_env = self.terminal_env(id, path);
        let vars = self.worktree_env_vars(id, path);
        let mut live = LiveWorktree::default();
        // Panel id per saved index; a panel that fails to open has none.
        let mut ids = Vec::with_capacity(config.panels.len());
        for spec in &config.panels {
            let panel = self.open_panel(&term_env, path, spec, &vars);
            ids.push(panel.as_ref().map(|(id, _)| *id));
            live.panels.extend(panel);
        }
        let (tabs, active) = config.tabs();
        live.tabs = tabs
            .iter()
            .filter_map(|tab| {
                let layout = tab
                    .layout
                    .filter_map(&mut |index| ids.get(index).copied().flatten())?;
                Some(LiveTab {
                    layout,
                    focused: tab.focused,
                })
            })
            .collect();
        live.active = active;
        live.normalize();
        self.live
            .entry(id)
            .or_default()
            .insert(path.to_path_buf(), live);
    }

    pub(crate) fn open_panel(
        &mut self,
        env: &crate::runtime::TermEnv,
        cwd: &Path,
        spec: &PanelSpec,
        vars: &[(String, String)],
    ) -> Option<(u64, LivePanel)> {
        self.next_panel += 1;
        let id = self.next_panel;
        match spec {
            PanelSpec::Terminal {
                title,
                command,
                cwd: dir,
                session,
                control: saved_control,
                agent,
                once,
            } => {
                let worktree = cwd;
                let cwd = dir.as_deref().unwrap_or(cwd);
                let mut registration = None;
                let session_control;
                // A terminal on a remote machine or a local VM runs in a
                // session that outlives the app; the saved name attaches to
                // it again. Elsewhere a saved name is dropped.
                let (remote_env, remote_session) = match env {
                    crate::runtime::TermEnv::Ready(env) if env.keeps_sessions() => (
                        Some(env.clone()),
                        Some(
                            session
                                .clone()
                                .unwrap_or_else(|| crate::control::random_hex(16)),
                        ),
                    ),
                    crate::runtime::TermEnv::Ready(_) => (None, None),
                    crate::runtime::TermEnv::Waiting(_) => (None, session.clone()),
                };
                // The agent saved with the terminal resumes its session, unless
                // the terminal attaches to a remote session it still runs in.
                let resume = match (env, agent) {
                    (crate::runtime::TermEnv::Ready(_), Some(agent))
                        if !(remote_env.is_some() && session.is_some()) =>
                    {
                        crate::agents::resume_command(agent)
                    }
                    _ => None,
                };
                // A terminal that runs the agent itself starts it resumed,
                // without the arguments it first started with: a prompt among
                // them, as launches pass one, would run again.
                let resumed = match (command, &resume) {
                    (Some(argv), Some(resume)) if crate::agents::runs(argv, &resume[0]) => Some(
                        argv[..1]
                            .iter()
                            .chain(&resume[1..])
                            .cloned()
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                };
                // A session the agent no longer has starts anew instead.
                let check = match (env, agent, &resume) {
                    (crate::runtime::TermEnv::Ready(env), Some(agent), Some(_)) => {
                        crate::agents::SessionCheck::new(env, vars, agent)
                    }
                    _ => None,
                };
                let typed = resume.filter(|_| command.is_none());
                // Every terminal gets a scope, so a later mode change
                // reaches it. A remote session's scope is named after the
                // session, which may already run in it. Tests stay out of
                // the user's systemd.
                let cpu = match env {
                    crate::runtime::TermEnv::Ready(env) if !cfg!(test) => {
                        let attached = remote_env.is_some() && session.is_some();
                        let id = remote_session
                            .clone()
                            .unwrap_or_else(|| crate::control::random_hex(16));
                        crate::cpu::CpuControl::start(env, &self.settings.cpu_mode, &id, attached)
                    }
                    _ => None,
                };
                let cpu_limit = cpu.as_ref().map(|(limit, _)| limit.clone());
                let mut builder = match (env, command) {
                    // The runtime still starts: follow its log. The panel keeps
                    // its own spec, so the layout saves what it will run.
                    (crate::runtime::TermEnv::Waiting(log), _) => {
                        // The placeholder keeps the saved session's socket
                        // and token with its name, so a save while it waits
                        // does not lose what the session's shell was given.
                        session_control =
                            saved_control.clone().filter(|_| remote_session.is_some());
                        Environment::Local
                            .command("tail")
                            .args(["-n", "+1", "-F"])
                            .arg(log)
                            .to_pty()
                    }
                    (crate::runtime::TermEnv::Ready(env), command) => {
                        // Only a remote session keeps its socket and token.
                        let saved = saved_control.as_ref().filter(|_| remote_session.is_some());
                        let (control, mut control_vars, sockets, seat) =
                            self.terminal_control(env, worktree, saved);
                        registration = control;
                        session_control = seat.filter(|_| remote_session.is_some());
                        // The worktree's variables go where the control
                        // variables go, so they reach the shell on another
                        // machine or in a container too, not only the local
                        // ssh or client process. They come last and win.
                        control_vars
                            .extend((vars.iter()).map(|(key, value)| (key.into(), value.into())));
                        // `once` follows the command only when it starts anew.
                        let once = if resumed.is_some() {
                            &[][..]
                        } else {
                            &once[..]
                        };
                        // A resumed agent checks first that its session
                        // still exists.
                        let wrapped = (check.as_ref())
                            .zip(resumed.as_ref())
                            .map(|(check, argv)| check.wrap(argv));
                        match resumed.as_ref().or(command.as_ref()) {
                            Some(argv) if !argv.is_empty() => {
                                let run = wrapped.as_deref().unwrap_or(argv);
                                // An agent is found on the login shell's
                                // `PATH` too, and finds bash as `$SHELL`
                                // where the environment has it.
                                let launch = crate::agents::agent_launch(
                                    argv,
                                    self.settings.agents_use_bash,
                                );
                                let mut command = match launch {
                                    Some(launch) => env.command("sh").args(launch).args(run),
                                    None => env.command(&run[0]).args(&run[1..]),
                                }
                                .args(once)
                                .cwd(cwd);
                                for (key, value) in &control_vars {
                                    command = command.var(key, value);
                                }
                                for (local, inside) in &sockets {
                                    command = command.forward_socket(local, inside);
                                }
                                if let Some(name) = remote_env.as_ref().and(remote_session.as_ref())
                                {
                                    command = command.session(name);
                                }
                                command.limit_cpu(cpu_limit).to_pty()
                            }
                            _ => env.limited_shell(
                                Some(cwd),
                                &control_vars,
                                &sockets,
                                remote_env.as_ref().and(remote_session.as_deref()),
                                cpu_limit.as_ref(),
                            ),
                        }
                    }
                };
                for name in crate::term::identity::FOREIGN_VARS {
                    builder.env_remove(name);
                }
                let wake = self.wake.clone();
                let wake: crate::term::Wake = Arc::new(move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
                let (cols, rows) = self.grid;
                let size = TermSize {
                    cols,
                    rows,
                    pixel_width: (f32::from(cols) * self.metrics.width) as u16,
                    pixel_height: (f32::from(rows) * self.metrics.height) as u16,
                };
                let panel_wake = wake.clone();
                match TerminalSession::spawn(id, builder, size, self.scheme.term_colors(), wake) {
                    Ok(session) => {
                        if let Some(registration) = &registration {
                            registration.attach(session.status(), session.input());
                        }
                        // An agent stopped in the middle of a turn goes on.
                        let carry_on = agent
                            .as_ref()
                            .is_some_and(|agent| agent.working && agent.session.is_some());
                        if typed.is_some() || (resumed.is_some() && carry_on) {
                            let line = typed.map(|line| line.join(" "));
                            let name = agent.as_ref().map_or("", |agent| agent.agent.as_str());
                            crate::agents::resume_in(session.input(), name, line, carry_on, check);
                        }
                        let _ = self.ext.emit(&Event::TerminalOpened {
                            id,
                            cwd: Some(cwd.to_path_buf()),
                        });
                        Some((
                            id,
                            LivePanel::Terminal(Rc::new({
                                let mut panel = TerminalPanel::new(
                                    session,
                                    title.clone(),
                                    command.clone(),
                                    dir.clone(),
                                );
                                panel.control = registration;
                                *panel.agent_session.get_mut() = agent.clone();
                                panel.resumed_title = agent.as_ref().and_then(|agent| {
                                    let title = self.saved_agent_title(worktree, agent)?;
                                    Some((agent.clone(), title.to_owned()))
                                });
                                panel.remote_session = remote_session;
                                panel.session_control = session_control;
                                // The agent's process group is in reach only
                                // for a shell on this machine.
                                panel.freezable = command.is_none()
                                    && matches!(env, crate::runtime::TermEnv::Ready(env)
                                        if env.base().is_local());
                                panel.thaw_signal = Some(self.thaw_signal.clone());
                                panel.wake = Some(panel_wake);
                                panel.remote_env = remote_env;
                                panel.cpu = cpu.map(|(_, control)| control);
                                if let crate::runtime::TermEnv::Ready(env) = env {
                                    panel.clipboard = Some(crate::clipboard::Target::of(
                                        env,
                                        &crate::clipboard::staging_dir(&self.runtimes.dirs.cache),
                                    ));
                                }
                                panel
                            })),
                        ))
                    }
                    Err(error) => {
                        self.push_notice((self.tr.terminal_failed)(&error.to_string()));
                        None
                    }
                }
            }
            PanelSpec::Browser { url } => Some((id, LivePanel::Browser(BrowserPanel::new(url)))),
            PanelSpec::DebugBrowser => {
                Some((id, LivePanel::DebugBrowser(DebugBrowserPanel::new(self.tr))))
            }
            PanelSpec::Files { file } => {
                // Files are read where the VCS runs, not in the worktree's
                // container or sandbox.
                let (env, jj, known) = self.vcs_env(cwd);
                let wake = self.wake.clone();
                let wake: crate::term::Wake = Arc::new(move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
                let root = cwd.to_path_buf();
                let files = FilesPanel::new(env, root, jj, known, file.clone(), wake, self.tr);
                let (cols, rows) = self.grid;
                files.resize(cols, rows);
                Some((id, LivePanel::Files(files)))
            }
            PanelSpec::Explore {
                review,
                change,
                base,
                committed,
            } => {
                // History is read where the VCS runs, like files.
                let (env, jj, known) = self.vcs_env(cwd);
                let project_base = self
                    .find_worktree(cwd)
                    .and_then(|project| self.state.project(project))
                    .and_then(|project| project.base.clone());
                let wake = self.wake.clone();
                let wake: crate::term::Wake = Arc::new(move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
                let mode = if *change {
                    ExploreMode::Change
                } else if *review {
                    ExploreMode::Review
                } else {
                    ExploreMode::History
                };
                let explore = ExplorePanel::new(
                    env,
                    cwd.to_path_buf(),
                    jj,
                    known,
                    mode,
                    base.clone(),
                    project_base,
                    !*committed,
                    wake,
                    self.tr,
                );
                let (cols, rows) = explore_grid(self.grid, self.metrics);
                explore.resize(cols, rows);
                Some((id, LivePanel::Explore(explore)))
            }
            PanelSpec::View { view, key } => {
                let wake = self.wake.clone();
                let wake: crate::term::Wake = Arc::new(move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
                let panel = FilesPanel::extension_view(view.clone(), key.clone(), wake, self.tr);
                let (cols, rows) = self.grid;
                panel.resize(cols, rows);
                self.render_view(&panel);
                Some((id, LivePanel::Files(panel)))
            }
            PanelSpec::Monitor => {
                // The machine the project lives on, where the VCS runs. A
                // container or sandbox shares its kernel and its /proc.
                let env = self
                    .find_worktree(cwd)
                    .and_then(|project| self.state.project(project))
                    .map_or(Environment::Local, |project| project.env.clone());
                let wake = self.wake.clone();
                let wake: crate::term::Wake = Arc::new(move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
                let monitor = MonitorPanel::new(env, cwd.to_path_buf(), wake, self.tr);
                let (cols, rows) = explore_grid(self.grid, self.metrics);
                monitor.resize(cols, rows);
                Some((id, LivePanel::Monitor(monitor)))
            }
            PanelSpec::Tasks => {
                let wake = self.wake.clone();
                let wake: crate::term::Wake = Arc::new(move || {
                    if let Some(signal) = wake.get() {
                        signal.request_redraw();
                    }
                });
                let tasks = TasksPanel::new(wake, self.tr);
                let (cols, rows) = explore_grid(self.grid, self.metrics);
                tasks.resize(cols, rows);
                Some((id, LivePanel::Tasks(tasks)))
            }
        }
    }
}

/// `path`, `~/path`, `host:/path`, `ssh://[user@]host[:port]/path`, or
/// `cmdterm://host[:port]/path[?vm=name]` for a cmdterm remote machine, or
/// `vm://name/path` for a Firecracker VM on this machine. VPN locations
/// (`tailscale://`, `netbird://`, `nebula://`) need a scan to find the peer,
/// so [`crate::vpn::VpnLocation`] parses them instead. A scheme an
/// extension registered with `term.environment` gives
/// `scheme://target/path` in that environment.
pub fn parse_location(input: &str) -> Option<(Environment, PathBuf)> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some((scheme, rest)) = input.split_once("://")
        && cmdterm_env::custom_environment(scheme).is_some()
    {
        let (target, path) = rest.split_once('/')?;
        if !cmdterm_env::valid_target(target) {
            return None;
        }
        // `coder://ws/~/src` is relative to the home there.
        let path = if path.starts_with('~') {
            PathBuf::from(path)
        } else {
            PathBuf::from(format!("/{path}"))
        };
        let target = cmdterm_env::CustomTarget {
            scheme: scheme.to_owned(),
            target: target.to_owned(),
        };
        return Some((Environment::Custom(target), path));
    }
    if let Some(rest) = input.strip_prefix("vm://") {
        let (name, path) = rest.split_once('/')?;
        if name.is_empty() {
            return None;
        }
        let path = if path.starts_with('~') {
            PathBuf::from(path)
        } else {
            PathBuf::from(format!("/{path}"))
        };
        let target = cmdterm_env::VmTarget {
            name: name.to_owned(),
        };
        return Some((Environment::Vm(target), path));
    }
    if let Some(rest) = input.strip_prefix("cmdterm://") {
        let (rest, vm) = match rest.split_once("?vm=") {
            Some((rest, vm)) => (rest, Some(vm.to_owned())),
            None => (rest, None),
        };
        let (authority, path) = rest.split_once('/')?;
        if authority.is_empty() {
            return None;
        }
        let mut target = cmdterm_env::RemoteTarget::new(authority);
        target.vm = vm.filter(|vm| !vm.is_empty());
        // `cmdterm://box/~/src` is relative to the remote home.
        let path = if path.starts_with('~') {
            PathBuf::from(path)
        } else {
            PathBuf::from(format!("/{path}"))
        };
        return Some((Environment::Remote(target), path));
    }
    if let Some(rest) = input.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().ok()),
            None => (authority, None),
        };
        // A user or host such as `-oProxyCommand=...` would be an option.
        if !cmdterm_env::valid_ssh_destination(host) {
            return None;
        }
        let mut target = cmdterm_env::SshTarget::new(host);
        target.port = port;
        return Some((Environment::Ssh(target), PathBuf::from(format!("/{path}"))));
    }
    // Any other scheme, such as an extension's that is not loaded, is not
    // an SSH host.
    if input
        .split_once("://")
        .is_some_and(|(scheme, _)| !scheme.is_empty() && !scheme.contains('/'))
    {
        return None;
    }
    if let Some((host, path)) = input.split_once(':')
        && !host.is_empty()
        && !host.contains('/')
        && !windows_drive(host)
        && (path.starts_with('/') || path.starts_with('~'))
    {
        return cmdterm_env::valid_ssh_destination(host)
            .then(|| (Environment::ssh(host), PathBuf::from(path)));
    }
    let path = match input.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)?
            .join(rest),
        None => PathBuf::from(input),
    };
    let path = cmdterm_env::canonicalize(&path).unwrap_or(path);
    Some((Environment::Local, path))
}

/// On Windows, the `C` of `C:/src` is a drive letter, not an SSH host.
fn windows_drive(host: &str) -> bool {
    cfg!(windows) && host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic()
}

/// The repository URL in `input` when it names a repository to clone rather
/// than a checkout to open: `https://`, `http://`, `git://`, `file://` and
/// `git+ssh://` URLs, `ssh://` URLs whose user is `git` or `hg` or whose path
/// ends in `.git`, and scp-like `user@host:path` with a relative path.
/// Other `ssh://` and `host:/path` inputs stay checkouts on an SSH host.
pub fn clone_url(input: &str) -> Option<&str> {
    let input = input.trim();
    if input.is_empty() || input.contains(char::is_whitespace) {
        return None;
    }
    let lower = input.to_ascii_lowercase();
    let schemes = [
        "https://",
        "http://",
        "git://",
        "file://",
        "git+ssh://",
        "ssh+git://",
    ];
    if schemes.iter().any(|scheme| lower.starts_with(scheme)) {
        return Some(input);
    }
    if let Some(rest) = input.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        let user = authority.split_once('@').map(|(user, _)| user);
        let repo =
            matches!(user, Some("git" | "hg")) || path.trim_end_matches('/').ends_with(".git");
        return repo.then_some(input);
    }
    let (host, path) = input.split_once(':')?;
    let scp = host.contains('@')
        && !host.contains('/')
        && !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('~');
    scp.then_some(input)
}

fn normalize_url(url: &str) -> String {
    if url.contains("://") {
        url.to_owned()
    } else if url.starts_with("localhost") || url.starts_with("127.0.0.1") {
        format!("http://{url}")
    } else {
        format!("https://{url}")
    }
}

/// A host for this run's extensions: each gets a directory under the cache
/// directory and one for `term.store` under the data directory, and those
/// off by default that the user turned on load.
fn extension_host(dirs: &cmdterm_env::AppDirs, settings: &Settings) -> ExtensionHost {
    let mut host = ExtensionHost::with_config(crate::ext::Config {
        cache_dir: Some(dirs.cache.join("extensions")),
        data_dir: Some(dirs.data.join("extension-data")),
        project_data_dir: Some(dirs.data.join("project-data")),
        ..crate::ext::Config::default()
    });
    host.turn_on(&settings.enabled_extensions);
    host.set_forge_hosts(&settings.forge_hosts);
    if !crate::bundle::THIRD_PARTY_EXTENSIONS {
        host.allow_only(Some(&crate::bundle::EXTENSIONS));
    }
    host
}

/// The directories extensions load from: the shipped ones, then the user's
/// own in builds with the `third-party-extensions` feature.
fn extension_dirs<'a>(shipped: &'a Path, own: &'a Path) -> Vec<&'a Path> {
    if crate::bundle::THIRD_PARTY_EXTENSIONS {
        vec![shipped, own]
    } else {
        vec![shipped]
    }
}

/// The id `term.project_store` names a project by, such as
/// `cmdterm-3f1c09a2b4d5e6f7`: its name for people who look at the
/// directory, and a hash of its environment and root, so it stays the same
/// across restarts and does not collide with a project of the same name
/// elsewhere. FNV-1a, so it never changes with the Rust version.
pub(crate) fn project_storage_id(name: &str, env: &Environment, root: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let env = serde_json::to_string(env).unwrap_or_default();
    let root = root.to_string_lossy();
    for byte in env.bytes().chain([0]).chain(root.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let mut readable: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    readable = readable.trim_matches('-').to_owned();
    if readable.is_empty() {
        format!("{hash:016x}")
    } else {
        format!("{readable}-{hash:016x}")
    }
}

/// Whether extension `name` loads unless the user turns it off; false for
/// one whose manifest says `enabled = false`.
fn extension_default_on(host: &ExtensionHost, name: &str) -> bool {
    host.extensions()
        .find(|ext| ext.name == name)
        .is_none_or(|ext| ext.default_on)
}

/// What each extension may use: the user's grants, and for shipped
/// extensions without one, the default from [`crate::bundle::default_grants`].
pub(crate) fn granted_permissions(settings: &Settings) -> BTreeMap<String, Vec<String>> {
    let mut granted = crate::bundle::default_grants().clone();
    for (name, names) in &settings.extension_permissions {
        granted.insert(name.clone(), names.clone());
    }
    granted
}

/// The extension setting values in `settings`, as the extension host takes
/// them. Values that are not a setting value at all are dropped; the host
/// drops the ones that do not fit their setting.
fn setting_values(settings: &Settings) -> crate::ext::SettingValues {
    settings
        .extension_settings
        .iter()
        .map(|(name, own)| {
            let own = own
                .iter()
                .filter_map(|(key, value)| {
                    let value = serde_json::from_value(value.clone()).ok()?;
                    Some((key.clone(), value))
                })
                .collect();
            (name.clone(), own)
        })
        .collect()
}

/// The status of every extension, as the control socket lists them.
pub(crate) fn extension_status(
    host: &ExtensionHost,
    settings: &Settings,
) -> Vec<serde_json::Value> {
    host.extensions()
        .map(|ext| {
            serde_json::json!({
                "name": ext.name,
                "path": ext.path,
                "description": ext.description,
                "loaded": ext.enabled && !ext.turned_off,
                "turned_off": !settings.extension_enabled(&ext.name, ext.default_on),
                "error": ext.last_error,
                "permissions": ext.permissions.names(),
                "missing_permissions": ext.missing.names(),
                "shipped": crate::bundle::is_shipped(&ext.name),
            })
        })
        .collect()
}

/// The load errors to show as notices. A shipped extension still waiting for
/// its first grant, listed on the Extensions page, gets no notice on every
/// start. One the user granted only some permissions runs, so its errors are
/// worth showing.
fn worth_a_notice(host: &ExtensionHost, errors: Vec<ExtError>) -> Vec<ExtError> {
    errors
        .into_iter()
        .filter(|error| {
            !(crate::bundle::EXTENSIONS.contains(&error.extension.as_str())
                && host.extensions().any(|ext| {
                    ext.name == error.extension
                        && !ext.permissions.is_empty()
                        && ext.missing == ext.permissions
                }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_and_local_locations() {
        assert_eq!(
            parse_location("box:/srv/app"),
            Some((Environment::ssh("box"), PathBuf::from("/srv/app")))
        );
        let (env, path) = parse_location("ssh://me@box:2222/srv/app").unwrap();
        assert_eq!(path, PathBuf::from("/srv/app"));
        let Environment::Ssh(target) = env else {
            panic!("expected ssh")
        };
        assert_eq!(target.destination, "me@box");
        assert_eq!(target.port, Some(2222));
        assert_eq!(parse_location("/tmp").unwrap().0, Environment::Local);
        assert_eq!(parse_location("  "), None);
        // Users and hosts that ssh would read as options are refused.
        assert_eq!(parse_location("ssh://-oProxyCommand=x/p"), None);
        assert_eq!(parse_location("ssh://-me@box/p"), None);
        assert_eq!(parse_location("ssh://me@-oProxyCommand=x/p"), None);
        assert_eq!(parse_location("-oProxyCommand=x:/p"), None);
        assert_eq!(parse_location("my box:/p"), None);

        let (env, path) = parse_location("cmdterm://box/srv/app").unwrap();
        assert_eq!(path, PathBuf::from("/srv/app"));
        assert_eq!(
            env,
            Environment::Remote(cmdterm_env::RemoteTarget::new("box"))
        );
        let (env, path) = parse_location("cmdterm://box:9000/~/src?vm=ci").unwrap();
        assert_eq!(path, PathBuf::from("~/src"));
        let Environment::Remote(target) = env else {
            panic!("expected remote")
        };
        assert_eq!(target.address, "box:9000");
        assert_eq!(target.vm.as_deref(), Some("ci"));
        assert_eq!(parse_location("cmdterm:///srv"), None);
        assert_eq!(
            parse_location("vm://dev/~/src"),
            Some((
                Environment::Vm(cmdterm_env::VmTarget { name: "dev".into() }),
                PathBuf::from("~/src")
            ))
        );
        assert_eq!(parse_location("vm:///x"), None);
    }

    #[test]
    fn drive_letters_are_local_on_windows() {
        let (env, _) = parse_location("C:/src/app").unwrap();
        if cfg!(windows) {
            assert_eq!(env, Environment::Local);
            assert_eq!(parse_location("C:\\src").unwrap().0, Environment::Local);
        } else {
            assert_eq!(env, Environment::ssh("C"));
        }
        // Longer hosts stay SSH hosts everywhere.
        assert_eq!(parse_location("cd:/srv").unwrap().0, Environment::ssh("cd"));
    }

    #[test]
    fn locations_reach_environments_extensions_register() {
        let input = "ws-test-coder://me@ws.main/~/src";
        assert_eq!(parse_location(input), None, "unknown until registered");
        let provider = cmdterm_env::CustomProvider {
            scheme: "ws-test-coder".into(),
            title: "Coder".into(),
            extension: "coder".into(),
            program: "coder".into(),
            args: vec!["ssh".into(), "{target}".into(), "--".into()],
            pty_args: None,
        };
        let owner = u64::MAX - 10;
        cmdterm_env::set_custom_environments(owner, vec![provider]);
        let target = cmdterm_env::CustomTarget {
            scheme: "ws-test-coder".into(),
            target: "me@ws.main".into(),
        };
        assert_eq!(
            parse_location(input),
            Some((Environment::Custom(target.clone()), PathBuf::from("~/src")))
        );
        assert_eq!(
            parse_location("ws-test-coder://ws/srv/app"),
            Some((
                Environment::Custom(cmdterm_env::CustomTarget {
                    target: "ws".into(),
                    ..target
                }),
                PathBuf::from("/srv/app")
            ))
        );
        assert_eq!(parse_location("ws-test-coder://-oX=y/srv"), None);
        assert_eq!(parse_location("ws-test-coder://ws"), None);
        assert_eq!(clone_url(input), None);
        cmdterm_env::set_custom_environments(owner, Vec::new());
    }

    #[test]
    fn repository_urls_are_told_from_checkouts() {
        for url in [
            "https://example.com/org/app.git",
            "HTTP://example.com/app",
            "git://example.com/app",
            "file:///srv/app",
            "git@example.com:org/app.git",
            "ssh://git@example.com/org/app",
            "ssh://me@box/srv/app.git",
        ] {
            assert_eq!(clone_url(url), Some(url), "{url}");
        }
        for checkout in [
            "/srv/app",
            "~/src/app",
            "box:/srv/app",
            "me@box:~/app",
            "ssh://me@box/srv/app",
            "vm://dev/~/src",
            "cmdterm://box/~/src",
            "",
        ] {
            assert_eq!(clone_url(checkout), None, "{checkout}");
        }
    }

    #[test]
    fn urls_get_a_scheme() {
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("servo.org"), "https://servo.org");
        assert_eq!(normalize_url("file:///x"), "file:///x");
    }
}
