//! The tasks panel: a debug view of everything the app runs, across every
//! project and worktree. It lists each worktree's panels with their tab and
//! pane and whether they are shown, hidden, frozen, resuming or exited, the
//! agents in the terminals and what they wait for, the extension tasks with
//! their last and next runs, and the threads of this process.
//!
//! The panel and worktree rows come from the workspace on the UI thread.
//! [`Workspace::poll`](crate::workspace::Workspace::poll) looks at most once
//! a second while the panel is shown, and only builds new rows when a
//! [`fingerprint`] of what they show moved, so an idle frame allocates
//! nothing. The thread rows come from a sampler thread that reads
//! `/proc/self/task` every two seconds, and only while the panel is drawn.
//!
//! Like the monitor panel it draws into one terminal grid and rewrites a row
//! only when the hash of what it shows changed.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::{Duration, Instant};

use schnellui::raw_keyboard::Key;
use schnellui::{FocusedInputEvent, FocusedInputResult, RawInputState, RawKeyEvent};

use crate::i18n::Strings;
use crate::monitor::Hash;
use crate::term::{Attention, Wake};
use crate::terminal_panel::Freeze;
use crate::workspace::{Inner, LivePanel, MAX_PANES};

/// Accessible names of the tasks panel grids, by pane position, like
/// [`crate::terminal_panel::TERMINAL_NAMES`].
pub const TASKS_NAMES: [&str; MAX_PANES] = [
    "Tasks",
    "Tasks (pane 2)",
    "Tasks (pane 3)",
    "Tasks (pane 4)",
    "Tasks (pane 5)",
    "Tasks (pane 6)",
    "Tasks (pane 7)",
    "Tasks (pane 8)",
];

/// Time between two looks at the workspace while the panel is shown.
const LOOK_INTERVAL: Duration = Duration::from_secs(1);
/// Time between two thread samples.
const THREAD_INTERVAL: Duration = Duration::from_secs(2);
/// Linux reports thread CPU time in these ticks per second (`USER_HZ`),
/// which is 100 on every architecture it runs on.
const CLOCK_TICKS: u64 = 100;
/// Columns of a thread group's name and count.
const THREAD_NAME: usize = 26;

// --- what the workspace reports -----------------------------------------------

/// Whether a panel runs and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// In a pane of the shown tab of the shown worktree.
    Shown,
    /// Running in a tab or worktree that is not shown.
    Hidden,
    /// An idle agent the app stopped, see [`crate::terminal_panel::Freeze`].
    Frozen,
    /// A frozen agent that resumes.
    Thawing,
    /// The program ended with an exit code.
    Exited,
    /// The program ended by a signal.
    Killed,
}

/// What a panel is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Terminal,
    Browser,
    Files,
    Explore,
    Monitor,
    DebugBrowser,
    Tasks,
}

impl Kind {
    fn of(panel: &LivePanel) -> Self {
        match panel {
            LivePanel::Terminal(_) => Self::Terminal,
            LivePanel::Browser(_) => Self::Browser,
            LivePanel::Files(_) => Self::Files,
            LivePanel::Explore(_) => Self::Explore,
            LivePanel::Monitor(_) => Self::Monitor,
            LivePanel::DebugBrowser(_) => Self::DebugBrowser,
            LivePanel::Tasks(_) => Self::Tasks,
        }
    }
}

/// One running panel.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelEntry {
    /// Tab position, from 0.
    pub tab: usize,
    /// Pane position in its tab, from 0. `None` when the tab has one pane.
    pub pane: Option<usize>,
    pub kind: Kind,
    pub status: Status,
    pub title: String,
    /// The code an exited program ended with.
    pub exit: Option<i32>,
    /// The agent in a terminal and what it reported last.
    pub agent: Option<(String, Attention)>,
    /// What the terminal's program wants from the user.
    pub attention: Attention,
    /// How long the terminal has run.
    pub age: Option<Duration>,
    /// How long until an idle agent in a hidden terminal is frozen.
    pub freezes_in: Option<Duration>,
}

/// One worktree with running panels.
#[derive(Clone, Debug, PartialEq)]
pub struct WorktreeEntry {
    /// The project's id and the worktree's path, to open one of its panels.
    pub project_id: u64,
    pub path: PathBuf,
    pub project: String,
    /// The branch, or the folder's name when it has none.
    pub place: String,
    /// The machine the project lives on.
    pub env: String,
    pub shown: bool,
    pub panels: Vec<PanelEntry>,
}

/// One recurring extension task (`term.task`).
#[derive(Clone, Debug, PartialEq)]
pub struct TaskEntry {
    pub id: String,
    pub title: String,
    /// Seconds between two runs.
    pub every: u64,
    /// Seconds since its newest run for any project; `None` before the first.
    pub since: Option<u64>,
}

/// What the workspace runs, from one look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub worktrees: Vec<WorktreeEntry>,
    pub tasks: Vec<TaskEntry>,
}

/// The counts of the header row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub panels: usize,
    pub worktrees: usize,
    pub terminals: usize,
    pub frozen: usize,
    pub exited: usize,
    /// Terminals whose program waits for the user.
    pub waiting: usize,
    /// Threads of this process; `None` before the first sample.
    pub threads: Option<usize>,
}

impl Snapshot {
    fn summary(&self) -> Summary {
        let panels = || self.worktrees.iter().flat_map(|w| &w.panels);
        let terminals = || panels().filter(|p| p.kind == Kind::Terminal);
        Summary {
            panels: panels().count(),
            worktrees: self.worktrees.len(),
            terminals: terminals().count(),
            frozen: terminals()
                .filter(|p| matches!(p.status, Status::Frozen | Status::Thawing))
                .count(),
            exited: terminals()
                .filter(|p| matches!(p.status, Status::Exited | Status::Killed))
                .count(),
            waiting: terminals().filter(|p| p.attention.wants_user()).count(),
            threads: None,
        }
    }
}

/// A hash of everything a [`snapshot`] shows, at minute resolution for the
/// ages and countdowns. Allocation-free, so the poll that compares it
/// costs nothing while the workspace stays the same.
pub fn fingerprint(inner: &Inner, now: Instant) -> u64 {
    let mut hash = Hash::new();
    let active = inner.state.active();
    hash.add(active.map_or(u64::MAX, |project| project.id));
    if let Some(project) = active {
        hash.add_bytes(project.current_worktree().as_os_str().as_encoded_bytes());
    }
    hash.add(crate::tasks::unix_now() / 60);
    hash.add(inner.state.projects.len() as u64);
    hash.add(inner.snapshots.len() as u64);
    hash.add(u64::from(inner.settings.freeze_idle_agents));
    hash.add(u64::from(inner.memory_pressure.load(Ordering::Acquire)));
    hash.add(inner.ext.tasks_revision());
    for (key, last) in &inner.state.task_runs {
        hash.add_str(key);
        hash.add(*last);
    }
    // The map's order is the same for the same map, which is all a
    // fingerprint needs.
    for (project, worktrees) in &inner.live {
        hash.add(*project);
        for (path, live) in worktrees {
            hash.add_bytes(path.as_os_str().as_encoded_bytes());
            hash.add(live.active as u64);
            for tab in &live.tabs {
                hash.add(u64::MAX);
                tab.layout.for_each(&mut |id| hash.add(id));
            }
            for (id, panel) in &live.panels {
                hash.add(*id);
                hash.add(Kind::of(panel) as u64);
                if let LivePanel::Terminal(terminal) = panel {
                    let session = &terminal.session;
                    hash.add(terminal.freeze_state() as u64);
                    hash.add_str(&terminal.shown_title());
                    hash.add(match session.exit_code() {
                        None => 0,
                        Some(None) => 1,
                        Some(Some(code)) => 2 + u64::from(code as u32),
                    });
                    hash.add(session.attention_revision());
                    hash.add(session.agent_state() as u64);
                    hash.add(terminal.idle_since().map_or(0, |since| {
                        1 + now.saturating_duration_since(since).as_secs() / 60
                    }));
                    hash.add(now.saturating_duration_since(terminal.started).as_secs() / 60);
                }
            }
        }
    }
    hash.0
}

/// What the workspace runs now. Cold: it allocates every title.
pub fn snapshot(inner: &Inner, now: Instant) -> Snapshot {
    let active = inner
        .state
        .active()
        .map(|project| (project.id, project.current_worktree()));
    let freeze_after = inner.freeze_after();
    let mut worktrees = Vec::new();
    for project in &inner.state.projects {
        let Some(live) = inner.live.get(&project.id) else {
            continue;
        };
        let mut paths: Vec<&PathBuf> = live.keys().collect();
        paths.sort();
        for path in paths {
            let worktree = &live[path];
            if worktree.panels.is_empty() {
                continue;
            }
            let current =
                active.is_some_and(|(id, shown)| id == project.id && shown == path.as_path());
            let branch = inner
                .snapshots
                .get(&project.id)
                .and_then(|snapshot| snapshot.worktrees.iter().find(|w| &w.path == path))
                .and_then(|w| w.branch.clone());
            let place = branch.unwrap_or_else(|| {
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            });
            let mut panels = Vec::with_capacity(worktree.panels.len());
            for (index, tab) in worktree.tabs.iter().enumerate() {
                let split = tab.layout.len() > 1;
                let mut pane = 0;
                tab.layout.for_each(&mut |id| {
                    if let Some((_, panel)) = worktree.panel(id) {
                        let shown = current && index == worktree.active;
                        panels.push(entry(
                            panel,
                            index,
                            split.then_some(pane),
                            shown,
                            now,
                            freeze_after,
                        ));
                    }
                    pane += 1;
                });
            }
            worktrees.push(WorktreeEntry {
                project_id: project.id,
                path: path.clone(),
                project: project.name.clone(),
                place,
                env: project.env.label().to_owned(),
                shown: current,
                panels,
            });
        }
    }
    let unix = crate::tasks::unix_now();
    let tasks = inner
        .ext
        .tasks()
        .iter()
        .map(|task| {
            let last = inner
                .state
                .task_runs
                .iter()
                .filter(|(key, _)| {
                    key.as_str() == task.id
                        || key
                            .strip_prefix(task.id.as_str())
                            .is_some_and(|rest| rest.starts_with('@'))
                })
                .map(|(_, last)| *last)
                .max();
            TaskEntry {
                id: task.id.clone(),
                title: task.title.clone(),
                every: task.every,
                since: last.map(|last| unix.saturating_sub(last)),
            }
        })
        .collect();
    Snapshot { worktrees, tasks }
}

fn entry(
    panel: &LivePanel,
    tab: usize,
    pane: Option<usize>,
    shown: bool,
    now: Instant,
    freeze_after: Option<Duration>,
) -> PanelEntry {
    let status = if shown { Status::Shown } else { Status::Hidden };
    let LivePanel::Terminal(terminal) = panel else {
        return PanelEntry {
            tab,
            pane,
            kind: Kind::of(panel),
            status,
            title: panel.title(),
            exit: None,
            agent: None,
            attention: Attention::None,
            age: None,
            freezes_in: None,
        };
    };
    let session = &terminal.session;
    let (status, exit) = match (session.exit_code(), terminal.freeze_state()) {
        (Some(Some(code)), _) => (Status::Exited, Some(code)),
        (Some(None), _) => (Status::Killed, None),
        (None, Freeze::Stopped) => (Status::Frozen, None),
        (None, Freeze::Thawing) => (Status::Thawing, None),
        (None, Freeze::Live) => (status, None),
    };
    let agent = terminal
        .agent_session
        .borrow()
        .as_ref()
        .map(|agent| (agent.agent.clone(), session.agent_state()));
    let freezes_in = match (terminal.idle_since(), freeze_after) {
        (Some(since), Some(after)) if status == Status::Hidden => {
            Some(after.saturating_sub(now.saturating_duration_since(since)))
        }
        _ => None,
    };
    PanelEntry {
        tab,
        pane,
        kind: Kind::Terminal,
        status,
        title: terminal.title(),
        exit,
        agent,
        attention: session.attention(),
        age: Some(now.saturating_duration_since(terminal.started)),
        freezes_in,
    }
}

// --- threads ------------------------------------------------------------------

/// The threads of this process that share a name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadGroup {
    pub name: String,
    pub count: usize,
    /// Threads in state `R`.
    pub running: usize,
    /// CPU time of all of them, user and system, in ticks.
    pub ticks: u64,
    /// CPU used since the last sample, in per mille of one core.
    pub load: u32,
}

/// The name, state and CPU ticks in one `/proc/<pid>/task/<tid>/stat`.
fn parse_stat(text: &str) -> Option<(&str, char, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?;
    let mut fields = text.get(close + 1..)?.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // utime and stime are fields 14 and 15; the state is field 3.
    let mut fields = fields.skip(10);
    let user: u64 = fields.next()?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    Some((name, state, user + system))
}

/// Groups `(tid, name, state, ticks)` readings by name, with the load
/// since `previous` over `elapsed`, busiest first. Remembers the ticks of
/// each thread in `previous`.
fn group_threads(
    readings: &[(u32, String, char, u64)],
    previous: &mut HashMap<u32, u64>,
    elapsed: Option<Duration>,
) -> Vec<ThreadGroup> {
    let mut groups: Vec<ThreadGroup> = Vec::new();
    let mut deltas: Vec<u64> = Vec::new();
    for (tid, name, state, ticks) in readings {
        let delta = previous
            .get(tid)
            .map_or(0, |before| ticks.saturating_sub(*before));
        let index = match groups.iter().position(|group| &group.name == name) {
            Some(index) => index,
            None => {
                groups.push(ThreadGroup {
                    name: name.clone(),
                    count: 0,
                    running: 0,
                    ticks: 0,
                    load: 0,
                });
                deltas.push(0);
                groups.len() - 1
            }
        };
        let group = &mut groups[index];
        group.count += 1;
        group.running += usize::from(*state == 'R');
        group.ticks += ticks;
        deltas[index] += delta;
    }
    if let Some(elapsed) = elapsed.filter(|elapsed| !elapsed.is_zero()) {
        let millis = elapsed.as_millis().max(1) as u64;
        for (group, delta) in groups.iter_mut().zip(&deltas) {
            // ticks * (1000 ms / CLOCK_TICKS) per elapsed ms, in per mille.
            let used = delta * 1000 / CLOCK_TICKS;
            group.load = u32::try_from(used * 1000 / millis).unwrap_or(u32::MAX);
        }
    }
    previous.clear();
    previous.extend(readings.iter().map(|(tid, _, _, ticks)| (*tid, *ticks)));
    groups.sort_by(|a, b| {
        (b.load, b.ticks, b.running)
            .cmp(&(a.load, a.ticks, a.running))
            .then_with(|| a.name.cmp(&b.name))
    });
    groups
}

/// Names this app gives its threads that are longer than the 15 bytes
/// Linux keeps.
const LONG_THREAD_NAMES: &[&str] = &[
    "scriptschnell-term",
    "cmdterm-agent-freeze",
    "cmdterm-agent-prompt",
    "cmdterm-agent-resume",
    "cmdterm-agent-scripts",
    "cmdterm-agent-stop",
    "cmdterm-agent-thaw",
    "cmdterm-blocklists",
    "cmdterm-bridge-control",
    "cmdterm-bridge-poll",
    "cmdterm-browser-tick",
    "cmdterm-ext-watchdog",
    "cmdterm-ports-http",
    "cmdterm-ports-tcp",
    "cmdterm-proxy-plain",
    "cmdterm-proxy-tcp",
    "cmdterm-proxy-unix",
    "cmdterm-web-links",
    "debug-browser-bidi",
    "debug-browser-cdp",
    "debug-browser-copy",
    "debug-browser-open",
    "debug-browser-panel",
    "debug-browser-stderr",
    "schnellui-debug-http",
    "schnellui-svg-raster",
];

/// The full name of a thread whose name Linux cut, when only one of
/// [`LONG_THREAD_NAMES`] starts with what is left.
fn full_thread_name(name: &str) -> &str {
    if name.len() != 15 {
        return name;
    }
    let mut found = LONG_THREAD_NAMES
        .iter()
        .filter(|long| long.starts_with(name));
    match (found.next(), found.next()) {
        (Some(long), None) => long,
        _ => name,
    }
}

/// Reads every thread of this process. `None` without `/proc`.
fn read_threads() -> Option<Vec<(u32, String, char, u64)>> {
    let dir = std::fs::read_dir("/proc/self/task").ok()?;
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let Some(tid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        // A thread that ended since the listing has no stat any more.
        let Ok(text) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some((name, state, ticks)) = parse_stat(&text) {
            out.push((tid, name.to_owned(), state, ticks));
        }
    }
    Some(out)
}

/// What the sampler thread and the UI thread share.
#[derive(Default)]
struct Shared {
    /// The newest sample; `None` inside when there is no `/proc`.
    latest: Mutex<Option<Option<Vec<ThreadGroup>>>>,
    revision: AtomicU64,
    /// Set by every frame that shows the panel, cleared by the sampler.
    seen: AtomicBool,
    stop: AtomicBool,
}

fn sampler(shared: Arc<Shared>, wake: Wake) {
    let mut previous = HashMap::new();
    let mut last: Option<Instant> = None;
    let mut sent: Option<Option<Vec<ThreadGroup>>> = None;
    while !shared.stop.load(Ordering::Acquire) {
        if !shared.seen.swap(false, Ordering::AcqRel) {
            // A panel that was hidden measures from a fresh baseline.
            last = None;
            std::thread::park_timeout(THREAD_INTERVAL);
            continue;
        }
        let at = Instant::now();
        let groups = read_threads().map(|readings| {
            let elapsed = last.map(|last| at - last);
            group_threads(&readings, &mut previous, elapsed)
        });
        last = Some(at);
        // An unchanged sample leaves the grid alone.
        if sent.as_ref() != Some(&groups) {
            sent = Some(groups.clone());
            if let Ok(mut latest) = shared.latest.lock() {
                *latest = Some(groups);
            }
            shared.revision.fetch_add(1, Ordering::Release);
            wake();
        }
        std::thread::park_timeout(THREAD_INTERVAL);
    }
}

// --- the rows -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tone {
    Ink,
    Muted,
    Good,
    Warn,
    Bad,
    Accent,
}

impl Tone {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Blank,
    /// A section heading: panels, extension tasks, threads.
    Section,
    /// A worktree under the panels heading.
    Group,
    /// A panel, task or thread group with a badge in front.
    Item,
    /// The column headings above a section's items.
    Columns,
    Message,
}

/// One body line, built when the snapshot or the threads change.
#[derive(Clone, Debug, PartialEq)]
struct Line {
    style: Style,
    badge: &'static str,
    tone: Tone,
    kind: &'static str,
    text: String,
    detail: String,
    /// The panel a click on the line opens.
    jump: Option<Jump>,
}

/// What a line of the tasks page is, for the widget page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Blank,
    Section,
    Group,
    Item,
    Columns,
    Message,
}

/// The color a line's state takes on the widget page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowTone {
    Ink,
    Muted,
    Good,
    Warn,
    Bad,
    Accent,
}

/// One line of the tasks page as the widget page draws it.
#[derive(Clone, Debug)]
pub struct TaskRow {
    pub kind: RowKind,
    pub tone: RowTone,
    pub badge: &'static str,
    pub panel_kind: &'static str,
    pub text: String,
    pub detail: String,
    pub jump: Option<Jump>,
}

/// A panel to open: its project, worktree and tab.
#[derive(Clone, Debug, PartialEq)]
pub struct Jump {
    pub project: u64,
    pub path: PathBuf,
    pub tab: usize,
}

impl Line {
    fn new(style: Style, text: impl Into<String>) -> Self {
        Self {
            style,
            badge: "",
            tone: Tone::Ink,
            kind: "",
            text: text.into(),
            detail: String::new(),
            jump: None,
        }
    }

    fn blank() -> Self {
        Self::new(Style::Blank, "")
    }

    /// Headings in the badge, kind, text and detail columns.
    fn columns(badge: &'static str, kind: &'static str, text: &str, detail: String) -> Self {
        Self {
            style: Style::Columns,
            badge,
            tone: Tone::Muted,
            kind,
            text: text.to_owned(),
            detail,
            jump: None,
        }
    }
}

/// `45s`, `12m`, `3h 05m` or `2d 04h`.
fn duration(out: &mut String, secs: u64, tr: &Strings) {
    let _ = match secs {
        0..60 => write!(out, "{secs}s"),
        60..3_600 => write!(out, "{}m", secs / 60),
        3_600..86_400 => write!(out, "{}h {:02}m", secs / 3_600, secs / 60 % 60),
        _ => write!(
            out,
            "{}{} {:02}h",
            secs / 86_400,
            tr.monitor_days,
            secs / 3_600 % 24
        ),
    };
}

fn duration_text(secs: u64, tr: &Strings) -> String {
    let mut out = String::new();
    duration(&mut out, secs, tr);
    out
}

/// Every body line of `snapshot` and `threads`, and the header text.
fn build_lines(
    snapshot: &Snapshot,
    threads: Option<&Option<Vec<ThreadGroup>>>,
    tr: &'static Strings,
) -> (String, Vec<Line>) {
    let mut summary = snapshot.summary();
    summary.threads = threads.and_then(|threads| {
        threads
            .as_ref()
            .map(|groups| groups.iter().map(|group| group.count).sum())
    });
    // The bar above the grid names the panel; the header counts.
    let header = (tr.tasks_summary)(&summary);
    let mut lines = vec![Line::blank(), Line::new(Style::Section, tr.tasks_panels)];
    if snapshot.worktrees.is_empty() {
        lines.push(Line::new(Style::Message, tr.tasks_none));
    }
    for (index, worktree) in snapshot.worktrees.iter().enumerate() {
        let mut group = Line::new(
            Style::Group,
            format!("{} · {}", worktree.project, worktree.place),
        );
        group.detail = worktree.env.clone();
        if worktree.shown {
            group.detail.push_str(" · ");
            group.detail.push_str(tr.tasks_shown_worktree);
        }
        lines.push(group);
        // The column names sit under the first worktree, right above the
        // rows they name.
        if index == 0 {
            lines.push(Line::columns(
                tr.tasks_col_state,
                tr.tasks_col_kind,
                tr.tasks_col_panel,
                tr.tasks_col_where.to_owned(),
            ));
        }
        for panel in &worktree.panels {
            lines.push(panel_line(worktree, panel, tr));
        }
    }
    // Where each panel sits lines up in a column after the widest title.
    let widest = lines
        .iter()
        .filter(|line| !line.kind.is_empty())
        .map(|line| line.text.chars().count())
        .max()
        .unwrap_or(0);
    // The details pad to the widest too, so every chevron after them sits
    // in one column next to its row.
    let widest_detail = lines
        .iter()
        .filter(|line| !line.kind.is_empty())
        .map(|line| line.detail.chars().count())
        .max()
        .unwrap_or(0);
    for line in lines.iter_mut().filter(|line| !line.kind.is_empty()) {
        let pad = widest - line.text.chars().count();
        line.text.extend(std::iter::repeat_n(' ', pad));
        let pad = widest_detail - line.detail.chars().count();
        line.detail.extend(std::iter::repeat_n(' ', pad));
    }
    lines.push(Line::blank());
    lines.push(Line::new(Style::Section, tr.tasks_extension));
    if snapshot.tasks.is_empty() {
        lines.push(Line::new(Style::Message, tr.tasks_none));
    } else {
        lines.push(Line::columns(
            tr.tasks_col_state,
            "",
            tr.tasks_col_task,
            tr.tasks_col_schedule.to_owned(),
        ));
    }
    for task in &snapshot.tasks {
        let due = task.since.is_none_or(|since| since >= task.every);
        let mut detail = (tr.tasks_every)(&duration_text(task.every, tr));
        detail.push_str(" · ");
        match task.since {
            Some(since) => detail.push_str(&(tr.tasks_last)(&duration_text(since, tr))),
            None => detail.push_str(tr.tasks_never),
        }
        if let Some(since) = task.since.filter(|_| !due) {
            detail.push_str(" · ");
            detail.push_str(&(tr.tasks_next)(&duration_text(task.every - since, tr)));
        }
        let mut text = task.id.clone();
        if !task.title.is_empty() && task.title != task.id {
            text.push_str(" · ");
            text.push_str(&task.title);
        }
        lines.push(Line {
            style: Style::Item,
            badge: if due {
                tr.tasks_due
            } else {
                tr.tasks_scheduled
            },
            tone: if due { Tone::Warn } else { Tone::Muted },
            kind: "",
            text,
            detail,
            jump: None,
        });
    }
    lines.push(Line::blank());
    lines.push(Line::new(Style::Section, tr.tasks_threads));
    match threads {
        None => lines.push(Line::new(Style::Message, tr.monitor_waiting)),
        Some(None) => lines.push(Line::new(Style::Message, tr.tasks_no_threads)),
        Some(Some(groups)) => {
            // Busy threads first, so what costs something stands at the top.
            let mut order: Vec<&ThreadGroup> = groups.iter().collect();
            order.sort_by_key(|group| {
                (
                    group.running == 0,
                    std::cmp::Reverse(group.load),
                    std::cmp::Reverse(group.ticks),
                )
            });
            // Threads that used under 0.1% of a core since the last look
            // fold into one line, so the busy ones stand alone and no row
            // reads running at 0.0%.
            let (order, idle): (Vec<&ThreadGroup>, Vec<&ThreadGroup>) =
                order.into_iter().partition(|group| group.load > 0);
            // Headings only over threads that have numbers to show.
            if !order.is_empty() {
                let mut heading = tr.tasks_col_thread.to_owned();
                let width = heading.chars().count();
                heading.extend(std::iter::repeat_n(' ', THREAD_NAME.saturating_sub(width)));
                lines.push(Line::columns(
                    tr.tasks_col_state,
                    "",
                    &heading,
                    format!("{} · {}", tr.tasks_col_cpu, tr.tasks_col_time),
                ));
            }
            for group in order {
                let mut text = full_thread_name(&group.name).to_owned();
                if group.count > 1 {
                    let _ = write!(text, " ×{}", group.count);
                }
                // Linux cuts thread names to 15 bytes; a column that fits
                // one with a count lines the numbers up.
                let width = text.chars().count();
                text.extend(std::iter::repeat_n(' ', THREAD_NAME.saturating_sub(width)));
                // Load and CPU time right-aligned in their own columns.
                let load = format!("{}.{}%", group.load / 10, group.load % 10);
                let millis = group.ticks * 1000 / CLOCK_TICKS;
                let mut time = String::new();
                if millis < 60_000 {
                    let _ = write!(time, "{}.{}s", millis / 1000, millis / 100 % 10);
                } else {
                    duration(&mut time, millis / 1000, tr);
                }
                // The page sets these in a proportional font, so a dot
                // parts them where padding would not line up.
                let detail = format!("{load} · {time}");
                // A thread asleep right now that used the CPU since the last
                // look reads busy, not idle.
                let (badge, tone) = if group.running > 0 {
                    (tr.tasks_running, Tone::Good)
                } else if group.load > 0 {
                    (tr.tasks_busy, Tone::Ink)
                } else {
                    (tr.tasks_sleeping, Tone::Muted)
                };
                lines.push(Line {
                    style: Style::Item,
                    badge,
                    tone,
                    kind: "",
                    text,
                    detail,
                    jump: None,
                });
            }
            if !idle.is_empty() {
                let count = idle.iter().map(|group| group.count).sum();
                lines.push(Line::new(Style::Message, (tr.tasks_idle_threads)(count)));
            }
        }
    }
    (header, lines)
}

fn panel_line(worktree: &WorktreeEntry, panel: &PanelEntry, tr: &'static Strings) -> Line {
    let (badge, tone) = match panel.status {
        Status::Shown => (tr.tasks_shown, Tone::Good),
        Status::Hidden => (tr.tasks_hidden, Tone::Muted),
        Status::Frozen => (tr.tasks_frozen, Tone::Accent),
        Status::Thawing => (tr.tasks_thawing, Tone::Warn),
        Status::Exited => (tr.tasks_exited, Tone::Bad),
        Status::Killed => (tr.tasks_killed, Tone::Bad),
    };
    let mut detail = (tr.tasks_place)(panel.tab + 1, panel.pane.map(|pane| pane + 1));
    let mut part = |text: &str| {
        if !text.is_empty() {
            detail.push_str(" · ");
            detail.push_str(text);
        }
    };
    if let Some(code) = panel.exit {
        part(&(tr.tasks_exit_code)(code));
    }
    if let Some((agent, state)) = &panel.agent {
        let state = (tr.tasks_agent_state)(*state);
        if state.is_empty() {
            part(agent);
        } else {
            part(&format!("{agent} {state}"));
        }
    } else if panel.attention.wants_user() {
        part((tr.tasks_agent_state)(panel.attention));
    }
    if let Some(age) = panel.age {
        // The list rebuilds once a minute, so the age counts whole minutes.
        let secs = age.as_secs();
        let age = if secs < 60 {
            "<1m".to_owned()
        } else {
            duration_text(secs / 60 * 60, tr)
        };
        part(&(tr.tasks_age)(&age));
    }
    if let Some(left) = panel.freezes_in {
        part(&(tr.tasks_freezes_in)(&duration_text(left.as_secs(), tr)));
    }
    let tone = if panel.attention.wants_user() && tone != Tone::Bad {
        Tone::Bad
    } else {
        tone
    };
    Line {
        style: Style::Item,
        badge,
        tone,
        kind: (tr.tasks_kind)(panel.kind),
        text: panel.title.clone(),
        detail,
        jump: Some(Jump {
            project: worktree.project_id,
            path: worktree.path.clone(),
            tab: panel.tab,
        }),
    }
}

// --- the panel ----------------------------------------------------------------

struct View {
    header: String,
    lines: Vec<Line>,
    cols: usize,
    rows: usize,
    /// First body line shown.
    top: usize,
    /// Bumped by every change that redraws all rows: a resize, a scroll.
    generation: u32,
}

impl View {
    fn body_rows(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    fn scroll(&mut self, delta: isize) {
        let max = self.lines.len().saturating_sub(self.body_rows());
        let top = self.top.saturating_add_signed(delta).min(max);
        if top != self.top {
            self.top = top;
            self.generation = self.generation.wrapping_add(1);
        }
    }
}

pub struct TasksPanel {
    shared: Arc<Shared>,
    sampler: Option<Thread>,
    view: RefCell<View>,
    view_revision: Cell<u64>,
    tr: Cell<&'static Strings>,
    snapshot: RefCell<Snapshot>,
    threads: RefCell<Option<Option<Vec<ThreadGroup>>>>,
    /// The sampler's revision the lines were built with.
    threads_seen: Cell<u64>,
    /// When the workspace was last looked at, and its fingerprint then.
    looked: Cell<Option<(Instant, u64)>>,
    /// Set when the lines changed, so the widget page mounts again.
    remount: Cell<bool>,
}

impl Drop for TasksPanel {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = &self.sampler {
            thread.unpark();
        }
    }
}

impl TasksPanel {
    pub fn new(wake: Wake, tr: &'static Strings) -> Rc<Self> {
        let shared = Arc::new(Shared {
            seen: AtomicBool::new(true),
            ..Shared::default()
        });
        let sampler = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("cmdterm-tasks".to_owned())
                .spawn(move || sampler(shared, wake))
                .ok()
                .map(|handle| handle.thread().clone())
        };
        let panel = Rc::new(Self {
            shared,
            sampler,
            view: RefCell::new(View {
                header: String::new(),
                lines: Vec::new(),
                cols: 80,
                rows: 24,
                top: 0,
                generation: 0,
            }),
            view_revision: Cell::new(0),
            tr: Cell::new(tr),
            snapshot: RefCell::new(Snapshot::default()),
            threads: RefCell::new(None),
            threads_seen: Cell::new(0),
            looked: Cell::new(None),
            remount: Cell::new(false),
        });
        panel.rebuild();
        panel
    }

    /// Tab label.
    pub fn title(&self) -> String {
        self.tr.get().tasks.to_owned()
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut view = self.view.borrow_mut();
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        if (view.cols, view.rows) != (cols, rows) {
            view.cols = cols;
            view.rows = rows;
            view.scroll(0);
            view.generation = view.generation.wrapping_add(1);
            drop(view);
            self.touch();
        }
    }

    fn touch(&self) {
        self.view_revision
            .set(self.view_revision.get().wrapping_add(1));
    }

    /// Whether the workspace is due another look: never looked at, or a
    /// second passed. Allocation-free.
    pub fn look_due(&self, now: Instant) -> bool {
        self.looked
            .get()
            .is_none_or(|(at, _)| now.saturating_duration_since(at) >= LOOK_INTERVAL)
    }

    /// Records a look at the workspace. Returns whether `fingerprint`
    /// differs from the last look's, so the caller builds a new snapshot.
    pub fn looked_at(&self, now: Instant, fingerprint: u64) -> bool {
        let changed = self
            .looked
            .get()
            .is_none_or(|(_, seen)| seen != fingerprint);
        self.looked.set(Some((now, fingerprint)));
        changed
    }

    /// Makes the next poll build a snapshot whatever the fingerprint says.
    pub fn refresh(&self) {
        self.looked.set(None);
    }

    pub fn set_snapshot(&self, snapshot: Snapshot) {
        if *self.snapshot.borrow() == snapshot {
            return;
        }
        *self.snapshot.borrow_mut() = snapshot;
        self.rebuild();
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.borrow().clone()
    }

    #[cfg(test)]
    pub fn text(&self) -> String {
        let view = self.view.borrow();
        let mut out = view.header.clone();
        for line in &view.lines {
            out.push('\n');
            for part in [
                line.badge,
                line.kind,
                line.text.as_str(),
                line.detail.as_str(),
            ] {
                if !part.is_empty() {
                    out.push_str(part);
                    out.push(' ');
                }
            }
        }
        out
    }

    fn rebuild(&self) {
        let tr = self.tr.get();
        let (header, lines) =
            build_lines(&self.snapshot.borrow(), self.threads.borrow().as_ref(), tr);
        let mut view = self.view.borrow_mut();
        if view.header != header || view.lines != lines {
            self.remount.set(true);
        }
        view.header = header;
        view.lines = lines;
        view.scroll(0);
        drop(view);
        self.touch();
    }

    /// Takes the newest thread sample and says, once, whether the lines
    /// changed since the page was mounted. Allocation-free when nothing
    /// changed, so the workspace can ask every poll.
    pub fn take_remount(&self) -> bool {
        self.receive();
        self.remount.replace(false)
    }

    /// The header and the lines, for the widget page. Builds them again
    /// first when the language changed.
    pub fn rows(&self, tr: &'static Strings) -> (String, Vec<TaskRow>) {
        if !std::ptr::eq(self.tr.get(), tr) {
            self.tr.set(tr);
            self.rebuild();
        }
        self.remount.set(false);
        let view = self.view.borrow();
        let rows = view
            .lines
            .iter()
            .map(|line| TaskRow {
                kind: match line.style {
                    Style::Blank => RowKind::Blank,
                    Style::Section => RowKind::Section,
                    Style::Group => RowKind::Group,
                    Style::Item => RowKind::Item,
                    Style::Columns => RowKind::Columns,
                    Style::Message => RowKind::Message,
                },
                tone: match line.tone {
                    Tone::Ink => RowTone::Ink,
                    Tone::Muted => RowTone::Muted,
                    Tone::Good => RowTone::Good,
                    Tone::Warn => RowTone::Warn,
                    Tone::Bad => RowTone::Bad,
                    Tone::Accent => RowTone::Accent,
                },
                badge: line.badge,
                panel_kind: line.kind,
                text: line.text.trim_end().to_owned(),
                detail: line.detail.trim_end().to_owned(),
                jump: line.jump.clone(),
            })
            .collect();
        (view.header.clone(), rows)
    }

    /// Takes the newest thread sample. Allocation-free when there is none.
    fn receive(&self) {
        let revision = self.shared.revision.load(Ordering::Acquire);
        if revision == self.threads_seen.replace(revision) {
            return;
        }
        let Some(groups) = self
            .shared
            .latest
            .lock()
            .ok()
            .and_then(|mut latest| latest.take())
        else {
            return;
        };
        *self.threads.borrow_mut() = Some(groups);
        self.rebuild();
    }

    /// Handles one focused input event: `r` looks at the workspace again
    /// now. Scrolling and the pointer go to the page's widgets.
    pub fn input(&self, event: FocusedInputEvent) -> FocusedInputResult {
        let handled = match event {
            FocusedInputEvent::Key(key) => self.key(&key),
            FocusedInputEvent::Focus(_) => true,
            FocusedInputEvent::Pointer(_)
            | FocusedInputEvent::Ime(_)
            | FocusedInputEvent::Clipboard(_) => false,
        };
        if handled {
            FocusedInputResult::Handled
        } else {
            FocusedInputResult::Ignored
        }
    }

    fn key(&self, key: &RawKeyEvent) -> bool {
        let mods = key.modifiers;
        if key.state == RawInputState::Released || mods.control || mods.alt || mods.super_key {
            return false;
        }
        if matches!(&key.logical_key, Key::Character(text) if text.as_str() == "r") {
            self.refresh();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_thread_stat_with_spaces_and_parens_in_the_name() {
        let stat = "4242 (a (b) c) S 1 2 3 4 5 6 7 8 9 10 120 30 0 0 20 0 1 0";
        assert_eq!(parse_stat(stat), Some(("a (b) c", 'S', 150)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn threads_group_by_name_with_their_load() {
        let mut previous = HashMap::new();
        let first = vec![
            (1, "main".to_owned(), 'S', 100),
            (2, "worker".to_owned(), 'S', 10),
            (3, "worker".to_owned(), 'R', 10),
        ];
        let groups = group_threads(&first, &mut previous, None);
        assert_eq!(groups[0].name, "main");
        assert_eq!(groups[1].count, 2);
        assert_eq!(groups[1].running, 1);
        assert!(groups.iter().all(|group| group.load == 0));
        // One second later the workers used half a core between them.
        let second = vec![
            (1, "main".to_owned(), 'S', 100),
            (2, "worker".to_owned(), 'R', 35),
            (3, "worker".to_owned(), 'R', 35),
        ];
        let groups = group_threads(&second, &mut previous, Some(Duration::from_secs(1)));
        assert_eq!(groups[0].name, "worker");
        assert_eq!(groups[0].load, 500);
        assert_eq!(groups[0].running, 2);
        assert_eq!(groups[1].load, 0);
    }

    #[test]
    fn reads_this_process_threads_on_linux() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let threads = read_threads().unwrap();
        assert!(!threads.is_empty());
    }

    #[test]
    fn cut_thread_names_get_their_full_name_back() {
        assert_eq!(full_thread_name("cmdterm-blockli"), "cmdterm-blocklists");
        assert_eq!(full_thread_name("scriptschnell-t"), "scriptschnell-term");
        // Two names start with it, so it stays as Linux left it.
        assert_eq!(full_thread_name("cmdterm-agent-s"), "cmdterm-agent-s");
        assert_eq!(full_thread_name("zbus::Connectio"), "zbus::Connectio");
        assert_eq!(full_thread_name("control"), "control");
    }

    #[test]
    fn durations_read_short() {
        let tr = crate::i18n::Lang::En.strings();
        assert_eq!(duration_text(42, tr), "42s");
        assert_eq!(duration_text(12 * 60 + 5, tr), "12m");
        assert_eq!(duration_text(3 * 3600 + 5 * 60, tr), "3h 05m");
        assert_eq!(duration_text(2 * 86_400 + 4 * 3600, tr), "2d 04h");
    }

    #[test]
    fn lines_show_states_agents_and_tasks() {
        let tr = crate::i18n::Lang::En.strings();
        let snapshot = Snapshot {
            worktrees: vec![WorktreeEntry {
                project_id: 1,
                path: PathBuf::from("/repo"),
                project: "app".to_owned(),
                place: "main".to_owned(),
                env: "local".to_owned(),
                shown: true,
                panels: vec![
                    PanelEntry {
                        tab: 0,
                        pane: None,
                        kind: Kind::Terminal,
                        status: Status::Frozen,
                        title: "claude".to_owned(),
                        exit: None,
                        agent: Some(("claude".to_owned(), Attention::Done)),
                        attention: Attention::Done,
                        age: Some(Duration::from_secs(600)),
                        freezes_in: None,
                    },
                    PanelEntry {
                        tab: 1,
                        pane: Some(1),
                        kind: Kind::Terminal,
                        status: Status::Exited,
                        title: "make".to_owned(),
                        exit: Some(2),
                        agent: None,
                        attention: Attention::None,
                        age: Some(Duration::from_secs(5)),
                        freezes_in: None,
                    },
                ],
            }],
            tasks: vec![TaskEntry {
                id: "ext.sync".to_owned(),
                title: "Sync".to_owned(),
                every: 3600,
                since: Some(600),
            }],
        };
        let (header, lines) = build_lines(&snapshot, Some(&None), tr);
        assert!(header.contains("2 panels in 1 worktree"), "{header}");
        assert!(header.contains("1 frozen"), "{header}");
        assert!(header.contains("1 exited"), "{header}");
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                let text = line.text.trim_end();
                format!("{}|{}|{}|{}", line.badge, line.kind, text, line.detail)
            })
            .collect();
        let has = |part: &str| text.iter().any(|line| line.contains(part));
        assert!(has("app · main|local · active"), "{text:#?}");
        assert!(
            has("frozen|terminal|claude|tab 1 · claude waiting for a prompt · open for 10m"),
            "{text:#?}"
        );
        assert!(
            has("exited|terminal|make|tab 2, pane 2 · exit code 2 · open for <1m"),
            "{text:#?}"
        );
        assert!(
            has("scheduled||ext.sync · Sync|every 1h 00m · last run 10m ago · next in 50m"),
            "{text:#?}"
        );
        assert!(has(tr.tasks_no_threads), "{text:#?}");
    }
}
