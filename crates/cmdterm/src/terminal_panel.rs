//! A terminal panel: a running [`TerminalSession`] plus the UI-side state
//! that maps it onto a SchnellUI terminal grid (scrollback offset, selection).
//!
//! An agent that sits idle in a hidden terminal can be frozen: the grid keeps
//! showing what it showed, the agent stops, and its session resumes in the
//! same shell when the terminal shows again. Input that arrives while it
//! resumes waits and goes to the agent once it is ready.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::term::{GridSync, MouseButton, MouseEvent, MouseEventKind, TermSize, TerminalSession};
use schnellui::scene::Color;
use schnellui::widgets::{TerminalCellMetrics, TerminalGrid, TerminalGridPoint, TerminalSelection};
use schnellui::{
    FocusedClipboardEvent, FocusedInputEvent, FocusedInputResult, RawFocusEvent, RawImeEvent,
    RawInputState, RawPointerAction, RawPointerButton, RawPointerEvent, RawWheelDelta,
};

use crate::workspace::MAX_PANES;

/// Accessible names of the terminal grids, by pane position: `Terminal`,
/// then `Terminal (pane 2)` and so on. One focused-input handler serves each
/// name and routes to the panel in its pane.
pub const TERMINAL_NAMES: [&str; MAX_PANES] = [
    "Terminal",
    "Terminal (pane 2)",
    "Terminal (pane 3)",
    "Terminal (pane 4)",
    "Terminal (pane 5)",
    "Terminal (pane 6)",
    "Terminal (pane 7)",
    "Terminal (pane 8)",
];
const WHEEL_LINES: f32 = 3.0;
/// Presses closer together than this, in time and in logical pixels, count
/// as a double or triple click. The same values as SchnellUI's text inputs.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_SLOP: f32 = 5.0;

/// What a press selects and a drag from it extends by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectUnit {
    Cell,
    /// Double-click: a word, a run of blanks or one punctuation character.
    Word,
    /// Triple-click: a line with the rows it wrapped onto.
    Line,
}

/// The selection gesture while the left button is down.
#[derive(Clone, Copy, Debug)]
struct Gesture {
    unit: SelectUnit,
    /// What the press selected, in order. A drag keeps it whole in either
    /// direction.
    anchor: (TerminalGridPoint, TerminalGridPoint),
}

/// The last left press, to count double and triple clicks.
#[derive(Clone, Copy, Debug)]
struct Click {
    at: Instant,
    position: schnellui::scene::Point,
    count: u8,
}

/// The click count of a press at `position`: one more than the last press
/// when it was close enough, back to one after a triple click.
fn click_count(last: Option<Click>, at: Instant, position: schnellui::scene::Point) -> u8 {
    match last {
        Some(last)
            if at.duration_since(last.at) <= MULTI_CLICK_INTERVAL
                && (last.position.x - position.x).abs() <= MULTI_CLICK_SLOP
                && (last.position.y - position.y).abs() <= MULTI_CLICK_SLOP =>
        {
            last.count % 3 + 1
        }
        _ => 1,
    }
}

/// The selection a drag to the unit `under` the pointer makes from a press
/// that selected `anchor`: it keeps the anchor and reaches the far side of
/// the unit under the pointer. The first point is where it started.
fn extend_selection(
    anchor: (TerminalGridPoint, TerminalGridPoint),
    under: (TerminalGridPoint, TerminalGridPoint),
) -> (TerminalGridPoint, TerminalGridPoint) {
    if under.0 < anchor.0 {
        (anchor.1, under.0)
    } else {
        (anchor.0, under.1.max(anchor.1))
    }
}

/// Whether the agent in a terminal runs, see [`TerminalPanel::freeze`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freeze {
    Live,
    /// The agent was stopped; the grid shows it as it was.
    Stopped,
    /// The agent's session resumes; input waits.
    Thawing,
}

/// Ctrl+V or Alt+V, the keys coding agents read the clipboard's image on.
fn is_image_paste_key(key: &schnellui::RawKeyEvent) -> bool {
    use schnellui::raw_keyboard::Key;
    let m = key.modifiers;
    matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("v"))
        && !m.shift
        && !m.super_key
        && m.control != m.alt
}

/// Ctrl+Shift+C or Ctrl+Shift+V. The window reads them as copy and paste;
/// one that reaches the panel as a key found nothing to copy or paste, or
/// repeats, and must not reach the program as Ctrl+C.
fn is_clipboard_chord(key: &schnellui::RawKeyEvent) -> bool {
    use schnellui::raw_keyboard::Key;
    let m = key.modifiers;
    matches!(&key.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("c") || c.eq_ignore_ascii_case("v"))
        && m.control
        && m.shift
        && !m.alt
        && !m.super_key
}

fn set_error(slot: &Mutex<Option<String>>, message: String) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(message);
    }
}

fn spawn(job: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("clipboard-paste".into())
        .spawn(job);
}

/// What every terminal grid is drawn with. Built from the settings and the
/// color scheme when a panel view mounts.
#[derive(Clone, Debug, PartialEq)]
pub struct GridStyle {
    pub metrics: TerminalCellMetrics,
    /// Background of selected text.
    pub selection: Color,
    /// Installed font family; `None` is the bundled mono face.
    pub font_family: Option<String>,
}

pub struct TerminalPanel {
    pub session: TerminalSession,
    /// Title from the saved panel spec; the program's own title wins when set.
    pub fixed_title: Option<String>,
    pub command: Option<Vec<String>>,
    /// Working directory from the spec; `None` is the worktree root.
    pub cwd: Option<std::path::PathBuf>,
    /// When the program started, to tell a failed start from a later exit.
    pub started: std::time::Instant,
    sync: RefCell<GridSync>,
    /// Bumped for UI-only changes (scrollback offset, selection) so the grid
    /// re-patches even though the session did not change.
    view_revision: Cell<u64>,
    selection: Cell<Option<(TerminalGridPoint, TerminalGridPoint)>>,
    gesture: Cell<Option<Gesture>>,
    last_click: Cell<Option<Click>>,
    /// URL found by the last Ctrl+click, taken by the app.
    link: RefCell<Option<String>>,
    /// The program title last shown in the tab bar.
    shown_title: RefCell<String>,
    shown_dir: RefCell<String>,
    /// The session's agent title revision the app last recorded.
    agent_title_seen: Cell<u64>,
    /// The coding agent open in the terminal, saved with the panel so the
    /// next start resumes its session.
    pub agent_session: RefCell<Option<crate::state::AgentSession>>,
    /// The session's agent session revision the app last recorded.
    agent_session_seen: Cell<u64>,
    /// The agent session this terminal resumed and the title the worktree
    /// saved for it, which stands until the agent sets a title of its own.
    pub resumed_title: Option<(crate::state::AgentSession, String)>,
    /// The process group in the foreground when the agent named its
    /// session: the agent's own.
    agent_group: Cell<Option<i32>>,
    /// The agent's process group while [`Self::freeze`] has it stopped.
    stopped_group: Cell<Option<i32>>,
    /// Whether the app may stop the agent here and resume it: a shell on
    /// this machine, also in a sandbox, where the agent's process group is
    /// in reach.
    pub freezable: bool,
    freeze: Cell<Freeze>,
    /// Since when the agent sat idle while hidden.
    idle_since: Cell<Option<Instant>>,
    /// Set by the thaw thread once the resumed agent is ready.
    thaw_ready: Arc<AtomicBool>,
    /// Set with `thaw_ready` for the app, which then looks at every
    /// thawing terminal.
    pub thaw_signal: Option<Arc<AtomicBool>>,
    /// Asks the window for a frame from another thread.
    pub wake: Option<crate::term::Wake>,
    /// Input that arrived while the agent resumed, with the cell metrics
    /// it came with.
    held: RefCell<Vec<(FocusedInputEvent, TerminalCellMetrics)>>,
    /// The terminal's token for the control socket, while it has one.
    pub control: Option<crate::control::Registration>,
    /// The session the program runs in on a remote machine or a local VM,
    /// saved with the panel so the next start attaches to it again.
    pub remote_session: Option<String>,
    /// Where [`Self::remote_session`] runs, to end it when the panel closes.
    pub remote_env: Option<cmdterm_env::Environment>,
    /// The control socket as the shell in [`Self::remote_session`] knows
    /// it, saved with the panel.
    pub session_control: Option<crate::state::SessionControl>,
    /// The session's client was told the terminal is hidden, so the
    /// server sends its output in batches. Clients start shown.
    told_hidden: Cell<bool>,
    /// Where pasted images go for this terminal's program.
    pub clipboard: Option<crate::clipboard::Target>,
    /// The scope the terminal runs in, to change its CPU weight.
    pub cpu: Option<crate::cpu::CpuControl>,
    /// The copies of agents' settings only this terminal's sandbox has.
    pub(crate) agent_copies: Option<crate::agent_sync::OwnCopies>,
    /// Why the last image paste failed, set by its worker thread and taken
    /// by the app.
    paste_error: Arc<Mutex<Option<String>>>,
}

impl TerminalPanel {
    pub fn new(
        session: TerminalSession,
        fixed_title: Option<String>,
        command: Option<Vec<String>>,
        cwd: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            session,
            fixed_title,
            command,
            cwd,
            started: std::time::Instant::now(),
            sync: RefCell::new(GridSync::new()),
            view_revision: Cell::new(0),
            selection: Cell::new(None),
            gesture: Cell::new(None),
            last_click: Cell::new(None),
            link: RefCell::new(None),
            shown_title: RefCell::new(String::new()),
            shown_dir: RefCell::new(String::new()),
            agent_title_seen: Cell::new(0),
            agent_session: RefCell::new(None),
            agent_session_seen: Cell::new(0),
            resumed_title: None,
            agent_group: Cell::new(None),
            stopped_group: Cell::new(None),
            freezable: false,
            freeze: Cell::new(Freeze::Live),
            idle_since: Cell::new(None),
            thaw_ready: Arc::new(AtomicBool::new(false)),
            thaw_signal: None,
            wake: None,
            held: RefCell::new(Vec::new()),
            control: None,
            remote_session: None,
            remote_env: None,
            session_control: None,
            told_hidden: Cell::new(false),
            clipboard: None,
            cpu: None,
            agent_copies: None,
            paste_error: Arc::new(Mutex::new(None)),
        }
    }

    /// Tells a remote session's client whether the terminal shows, when
    /// that changed: a hidden terminal's output then comes in larger,
    /// rarer messages, which spares a laptop's battery and mobile data.
    /// Allocation-free; a client that cannot be told yet is told on a
    /// later call.
    pub fn set_shown(&self, shown: bool) {
        if self.remote_session.is_none() || self.told_hidden.get() == !shown {
            return;
        }
        if self.session.tell_shown(shown) {
            self.told_hidden.set(!shown);
        }
    }

    /// Gives the terminal the CPU weight and cores `modes` ask for: an
    /// agent's in a hidden terminal runs at the mode's background nice
    /// level, any other at the default; hidden terminals run on efficiency
    /// cores where the machine has them. Allocation-free while nothing
    /// changes.
    pub fn update_cpu(&self, shown: bool, modes: &crate::state::CpuModes) {
        let Some(cpu) = &self.cpu else {
            return;
        };
        let agent = self.agent_session.borrow().is_some();
        cpu.update(modes, shown, agent, || {
            let leader = self
                .session
                .child_pid()
                .and_then(|pid| i32::try_from(pid).ok());
            leader
                .into_iter()
                .chain(self.session.foreground_group())
                .collect()
        });
    }

    /// Ends the remote session the program runs in, off the UI thread, so
    /// closing the panel ends the program as it does locally. Quitting the
    /// app does not call this: the session keeps running, and the panel
    /// attaches to it on the next start. A local program ends here too,
    /// rather than when the last reference to the panel drops: on Windows
    /// its working directory cannot be removed while it runs.
    pub fn end_session(&self) {
        if self.session.exit_code().is_some() {
            return;
        }
        let (Some(env), Some(name)) = (self.remote_env.clone(), self.remote_session.clone()) else {
            self.session.kill();
            return;
        };
        let _ = std::thread::Builder::new()
            .name("end-session".into())
            .spawn(move || {
                let _ = env.end_session(&name);
            });
    }

    /// The last line on screen that is not blank, such as the error of a
    /// program that failed to start. Cold: runs once when it exits.
    pub fn last_line(&self) -> Option<String> {
        let term = self.session.lock();
        let bottom = term.stable_index(term.size().rows);
        (term.screen().first_stable()..bottom)
            .rev()
            .find_map(|stable| {
                let line = term.row_text(term.row_by_stable(stable)?).trim().to_owned();
                (!line.is_empty()).then_some(line)
            })
    }

    /// Why the last image paste failed, once. Allocation-free when none did.
    pub fn take_paste_error(&self) -> Option<String> {
        self.paste_error.try_lock().ok()?.take()
    }

    /// Saves the clipboard image `png` where the program runs and pastes
    /// its path, off the UI thread.
    fn paste_image(&self, image: schnellui::platform::ClipboardImage) -> FocusedInputResult {
        let Some(target) = self.clipboard.clone() else {
            return FocusedInputResult::Ignored;
        };
        let input = self.session.input();
        let error = self.paste_error.clone();
        spawn(move || {
            let staged = crate::clipboard::encode_png(&image)
                .and_then(|png| target.stage(&png, &crate::clipboard::paste_name()));
            match staged {
                Ok(path) => input.paste(&crate::clipboard::paste_text(&path)),
                Err(message) => set_error(&error, message),
            }
        });
        self.follow_output();
        FocusedInputResult::Handled
    }

    /// Ctrl+V or Alt+V where the program cannot read the clipboard: stages
    /// the clipboard's image for the shims first, then sends the key.
    fn bridge_key(&self, code: crate::term::KeyCode, mods: crate::term::Mods) {
        let Some(target) = self.clipboard.clone() else {
            return;
        };
        let input = self.session.input();
        let error = self.paste_error.clone();
        spawn(move || {
            let staged = crate::clipboard::read_png()
                .map(|png| png.and_then(|png| target.stage(&png, crate::clipboard::BRIDGE_NAME)));
            if let Some(Err(message)) = staged {
                set_error(&error, message);
            }
            input.key_down(code, mods);
        });
    }

    /// The working directory the shell reported through OSC 7, from a
    /// `file://` URL with any host.
    pub fn current_dir(&self) -> Option<std::path::PathBuf> {
        let term = self.session.lock();
        // The host is ignored: shells put their own hostname here, including
        // on SSH projects, and the path is what matters. The model already
        // decoded the path.
        let path = &term.current_dir()?.path;
        (path.starts_with('/') && !path.chars().any(char::is_control))
            .then(|| std::path::PathBuf::from(path))
    }

    /// The http(s) URL from the last Ctrl+click, once.
    pub fn take_link(&self) -> Option<String> {
        self.link.borrow_mut().take()
    }

    /// Tab label: the fixed title, else the program's OSC title, else "shell".
    /// A label that only names the agent, as a launch's fixed title does,
    /// gives way to the agent session's title, so a resumed session shows
    /// its saved one.
    pub fn title(&self) -> String {
        let label = self.fixed_title.clone().or_else(|| {
            let term = self.session.lock();
            let title = program_title(term.title().trim());
            (!title.is_empty()).then(|| title.to_owned())
        });
        let generic = label
            .as_deref()
            .is_none_or(|title| crate::agents::generic_title(crate::term::clean_title(title)));
        if generic && self.agent_session.borrow().is_some() {
            let agent = self.agent_title();
            if !agent.is_empty() && !crate::agents::generic_title(&agent) {
                return agent;
            }
        }
        label.unwrap_or_else(|| "shell".to_owned())
    }

    /// Whether an agent session runs here and has a title that says more
    /// than the agent's name.
    pub fn agent_titled(&self) -> bool {
        if self.agent_session.borrow().is_none() {
            return false;
        }
        let title = self.agent_title();
        !title.is_empty() && !crate::agents::generic_title(&title)
    }

    /// The title of the agent session in this terminal, once after it
    /// changed.
    pub fn take_agent_title(&self) -> Option<String> {
        let revision = self.session.agent_title_revision();
        if revision == self.agent_title_seen.replace(revision) {
            return None;
        }
        Some(self.session.agent_title()).filter(|title| !title.is_empty())
    }

    /// The title the agent session set last. Until it set one that says
    /// more than the agent's name, a resumed session has its saved title.
    /// Empty when neither is known.
    pub fn agent_title(&self) -> String {
        let live = self.session.agent_title();
        if !live.is_empty() && !crate::agents::generic_title(&live) {
            return live;
        }
        let resumed = self.resumed_title.as_ref().filter(|(resumed, _)| {
            self.agent_session.borrow().as_ref().is_some_and(|agent| {
                agent.agent == resumed.agent && agent.session == resumed.session
            })
        });
        resumed.map_or(live, |(_, title)| title.clone())
    }

    /// Takes the agent session the hooks named since the last call, and
    /// whether the agent is in the middle of a turn, into
    /// [`Self::agent_session`]. Returns whether that changed.
    pub fn take_agent_session(&self) -> bool {
        let mut changed = false;
        let revision = self.session.agent_session_revision();
        // The stopped agent ends its session; the saved one stays.
        if self.freeze.get() == Freeze::Stopped {
            self.agent_session_seen.set(revision);
            return false;
        }
        if revision != self.agent_session_seen.replace(revision) {
            let value = self.session.agent_session();
            if !value.is_empty() {
                self.agent_group.set(self.session.foreground_group());
            }
            let next = crate::agents::parse_session(&value);
            // A value the app cannot resume leaves the saved one alone,
            // except the empty one, which ends the session.
            let mut current = self.agent_session.borrow_mut();
            let same = |a: &crate::state::AgentSession, b: &crate::state::AgentSession| {
                a.agent == b.agent && a.session == b.session
            };
            let differs = match (current.as_ref(), next.as_ref()) {
                (Some(a), Some(b)) => !same(a, b),
                (None, None) => false,
                _ => true,
            };
            if (next.is_some() || value.is_empty()) && differs {
                *current = next;
                changed = true;
            }
        }
        // Until the agent reports in this run, the saved state stands: a
        // resumed agent has not picked up its turn yet.
        if self.agent_session_seen.get() == 0 {
            return changed;
        }
        let working = matches!(
            self.session.agent_state(),
            crate::term::Attention::Busy
                | crate::term::Attention::Input
                | crate::term::Attention::Permission
        );
        if let Some(agent) = self.agent_session.borrow_mut().as_mut()
            && agent.working != working
        {
            agent.working = working;
            changed = true;
        }
        changed
    }

    pub fn freeze_state(&self) -> Freeze {
        self.freeze.get()
    }

    /// The program's title as [`Self::title_changed`] last saw it.
    pub fn shown_title(&self) -> std::cell::Ref<'_, String> {
        self.shown_title.borrow()
    }

    /// Since when the agent sat idle while hidden, as the last freeze check
    /// saw it.
    pub fn idle_since(&self) -> Option<Instant> {
        self.idle_since.get()
    }

    /// Whether to freeze the agent now: it waits for a prompt after its
    /// turn, nobody typed since, the terminal has been hidden that long, and
    /// the agent is still the program in the terminal's foreground.
    pub fn freeze_due(&self, shown: bool, now: Instant, after: Duration) -> bool {
        let idle = self.freezable
            && !shown
            && self.freeze.get() == Freeze::Live
            && self.session.exit_code().is_none()
            && self.session.agent_state() == crate::term::Attention::Done
            && (self.agent_session.borrow().as_ref()).is_some_and(|a| a.session.is_some());
        if !idle {
            self.idle_since.set(None);
            return false;
        }
        let since = *self.idle_since.get().get_or_insert(now);
        self.idle_since.set(Some(since));
        let Some(group) = self.agent_group.get() else {
            return false;
        };
        let program = (self.agent_session.borrow().as_ref()).and_then(crate::agents::program_of);
        now.duration_since(since) >= after
            && self.session.foreground_group() == Some(group)
            && program.is_some_and(|program| crate::agents::group_runs(group, program))
    }

    /// Stops the agent and keeps the grid as it is until [`Self::thaw`]
    /// resumed it.
    pub fn freeze(&self) {
        let Some(group) = self.agent_group.take() else {
            return;
        };
        self.sync.borrow_mut().freeze(&self.session.lock());
        self.stopped_group.set(Some(group));
        self.freeze.set(Freeze::Stopped);
        self.idle_since.set(None);
        crate::agents::stop_group(group);
        self.touch();
    }

    /// Resumes a stopped agent in its shell. The grid stays frozen and
    /// input waits until [`Self::finish_thaw`].
    pub fn thaw(&self) {
        if self.freeze.get() != Freeze::Stopped {
            return;
        }
        self.freeze.set(Freeze::Thawing);
        self.thaw_ready.store(false, Ordering::Release);
        let (agent, line) = match self.agent_session.borrow().as_ref() {
            Some(saved) => (
                saved.agent.clone(),
                crate::agents::resume_command(saved).map(|command| command.join(" ")),
            ),
            None => (String::new(), None),
        };
        // Once another group than the stopped agent's has the foreground,
        // the shell is back there: on this machine it reports none, in a
        // sandbox its own.
        let stopped = self.stopped_group.take();
        let shell_back = stopped.is_some() && self.session.foreground_group() != stopped;
        let signal = self.thaw_signal.clone();
        let wake = self.wake.clone();
        let ready = self.thaw_ready.clone();
        let done: crate::term::Wake = Arc::new(move || {
            if let Some(signal) = &signal {
                signal.store(true, Ordering::Release);
            }
            if let Some(wake) = &wake {
                wake();
            }
        });
        match line {
            Some(line) => {
                crate::agents::thaw(self.session.input(), &agent, line, shell_back, ready, done)
            }
            None => {
                ready.store(true, Ordering::Release);
                done();
            }
        }
    }

    /// Shows the terminal again once the resumed agent is ready, and hands
    /// it the input that waited. Returns whether it did.
    pub fn finish_thaw(&self) -> bool {
        if self.freeze.get() != Freeze::Thawing || !self.thaw_ready.load(Ordering::Acquire) {
            return false;
        }
        self.freeze.set(Freeze::Live);
        self.sync.borrow_mut().thaw();
        for (event, metrics) in self.held.take() {
            self.input(event, metrics);
        }
        self.touch();
        true
    }

    /// Whether the program's title changed since the last call. Allocation-free
    /// while the title stays the same.
    pub fn title_changed(&self) -> bool {
        let term = self.session.lock();
        let title = term.title();
        let mut shown = self.shown_title.borrow_mut();
        if shown.as_str() == title {
            return false;
        }
        shown.clear();
        shown.push_str(title);
        true
    }

    /// True once after the shell reports a new working directory (OSC 7).
    /// Allocates only when the directory changes.
    pub fn dir_changed(&self) -> bool {
        let term = self.session.lock();
        let dir = term.current_dir().map_or("", |dir| dir.path.as_str());
        let mut shown = self.shown_dir.borrow_mut();
        if shown.as_str() == dir {
            return false;
        }
        shown.clear();
        shown.push_str(dir);
        true
    }

    /// Sets the cursor for a rebuilt grid: solid in the pane that takes
    /// focus, hollow in the others. A grid mounted again gets a new widget,
    /// so the focus event that would have told the old one it lost focus
    /// never reaches this panel.
    pub fn mount_focused(&self, focused: bool) {
        if self.sync.borrow_mut().set_focused(focused) {
            self.touch();
        }
    }

    pub fn view(self: &Rc<Self>, style: &GridStyle, label: &str) -> TerminalGrid {
        // Shown again: the stopped agent resumes behind the frozen grid.
        self.thaw();
        let revision = self.clone();
        let patch = self.clone();
        let selection_color = style.selection;
        let grid = TerminalGrid::patched(
            move || revision.revision(),
            move |grid| {
                let selection = patch.selection.get().map(|(start, end)| TerminalSelection {
                    start,
                    end,
                    background: selection_color,
                    foreground: None,
                });
                let term = patch.session.lock();
                patch.sync.borrow_mut().sync(&term, grid, selection);
            },
        )
        .metrics(style.metrics)
        .label(label);
        match &style.font_family {
            Some(family) => grid.font_family(family.clone()),
            None => grid,
        }
    }

    /// Switches the session's colors and redraws every row on the next sync.
    pub fn set_colors(&self, colors: crate::term::TermColors) {
        self.session.set_colors(colors);
        self.sync.borrow_mut().invalidate();
        self.touch();
    }

    fn revision(&self) -> u64 {
        self.session
            .revision()
            .wrapping_add(self.view_revision.get())
    }

    fn touch(&self) {
        self.view_revision.set(self.view_revision.get() + 1);
    }

    pub fn resize(&self, cols: u16, rows: u16, metrics: TerminalCellMetrics) {
        self.session.resize(TermSize {
            cols,
            rows,
            pixel_width: (f32::from(cols) * metrics.width) as u16,
            pixel_height: (f32::from(rows) * metrics.height) as u16,
        });
    }

    /// Handles one focused input event for this terminal.
    pub fn input(
        &self,
        event: FocusedInputEvent,
        metrics: TerminalCellMetrics,
    ) -> FocusedInputResult {
        if self.freeze.get() != Freeze::Live && holds(&event) {
            self.held.borrow_mut().push((event, metrics));
            return FocusedInputResult::Handled;
        }
        match event {
            FocusedInputEvent::Key(key) => {
                if key.state == RawInputState::Released || is_clipboard_chord(&key) {
                    return FocusedInputResult::Handled;
                }
                if key.modifiers.shift && !key.modifiers.control {
                    use schnellui::raw_keyboard::{Key, NamedKey};
                    let page = i32::from(self.session.size().rows.saturating_sub(1)) as isize;
                    let lines = match key.logical_key {
                        Key::Named(NamedKey::PageUp) => Some(page),
                        Key::Named(NamedKey::PageDown) => Some(-page),
                        _ => None,
                    };
                    if let Some(lines) = lines {
                        self.scroll(lines);
                        return FocusedInputResult::Handled;
                    }
                }
                let Some((code, mods)) = crate::term::keys::translate(&key) else {
                    return match &key.text {
                        Some(text) if !text.is_empty() => {
                            self.session.note_input();
                            let _ = self.session.write_bytes(text.as_bytes());
                            self.follow_output();
                            FocusedInputResult::Handled
                        }
                        _ => FocusedInputResult::Ignored,
                    };
                };
                self.session.note_input();
                let bridged = self.clipboard.as_ref().is_some_and(|t| t.bridges());
                if bridged && is_image_paste_key(&key) {
                    self.bridge_key(code, mods);
                    self.follow_output();
                    return FocusedInputResult::Handled;
                }
                // Encoded under the model's lock, written after it.
                self.session.key_down(code, mods);
                self.follow_output();
                FocusedInputResult::Handled
            }
            FocusedInputEvent::Pointer(pointer) => self.pointer(pointer, metrics),
            FocusedInputEvent::Focus(focus) => {
                let gained = matches!(
                    focus,
                    RawFocusEvent::WidgetGained | RawFocusEvent::WindowGained
                );
                self.session.focus_changed(gained);
                // The cursor shows whether keys reach this pane.
                if self.sync.borrow_mut().set_focused(gained) {
                    self.touch();
                }
                FocusedInputResult::Handled
            }
            FocusedInputEvent::Ime(RawImeEvent::Commit(text)) => {
                self.session.note_input();
                let _ = self.session.write_bytes(text.as_bytes());
                self.follow_output();
                FocusedInputResult::Handled
            }
            FocusedInputEvent::Ime(_) => FocusedInputResult::Handled,
            FocusedInputEvent::Clipboard(FocusedClipboardEvent::Copy) => {
                match self.selection.get() {
                    Some((start, end)) => {
                        let term = self.session.lock();
                        let text = self.sync.borrow().selection_text(
                            &term,
                            (start.row, start.column),
                            (end.row, end.column),
                        );
                        FocusedInputResult::CopyText(text)
                    }
                    // Unhandled, the chord would reach the program as Ctrl+C.
                    None => FocusedInputResult::Handled,
                }
            }
            FocusedInputEvent::Clipboard(FocusedClipboardEvent::Paste(text)) => {
                self.session.paste(&text);
                self.follow_output();
                FocusedInputResult::Handled
            }
            FocusedInputEvent::Clipboard(FocusedClipboardEvent::PasteImage(image)) => {
                self.paste_image(image)
            }
        }
    }

    fn pointer(
        &self,
        pointer: RawPointerEvent,
        metrics: TerminalCellMetrics,
    ) -> FocusedInputResult {
        let size = self.session.size();
        let column = ((pointer.position.x / metrics.width).max(0.0) as usize)
            .min(usize::from(size.cols.saturating_sub(1)));
        let row = ((pointer.position.y / metrics.height).max(0.0) as usize)
            .min(usize::from(size.rows.saturating_sub(1)));
        let point = TerminalGridPoint::new(row, column);

        // Applications that grab the mouse get it, unless Shift is held.
        let grabbed = self.session.lock().is_mouse_grabbed() && !pointer.modifiers.shift;
        if grabbed {
            let mut steps = 1;
            let (kind, button) = match pointer.action {
                // A program that grabbed the mouse hears no leave in xterm
                // either.
                RawPointerAction::Leave => return FocusedInputResult::Ignored,
                RawPointerAction::Move => (MouseEventKind::Move, MouseButton::None),
                RawPointerAction::Button { button, state } => (
                    match state {
                        RawInputState::Pressed => MouseEventKind::Press,
                        RawInputState::Released => MouseEventKind::Release,
                    },
                    match button {
                        RawPointerButton::Left => MouseButton::Left,
                        RawPointerButton::Middle => MouseButton::Middle,
                        RawPointerButton::Right => MouseButton::Right,
                        _ => return FocusedInputResult::Ignored,
                    },
                ),
                RawPointerAction::Wheel(delta) => {
                    let lines = wheel_lines(delta, metrics);
                    if lines == 0 {
                        return FocusedInputResult::Handled;
                    }
                    // One press per notch.
                    steps = lines.unsigned_abs();
                    (
                        MouseEventKind::Press,
                        if lines > 0 {
                            MouseButton::WheelUp
                        } else {
                            MouseButton::WheelDown
                        },
                    )
                }
            };
            let mut event = MouseEvent::new(kind, button, column, row);
            event.mods = crate::term::keys::modifiers(pointer.modifiers);
            // Encoded under the model's lock, written after it.
            self.session.send_mouse(event, steps);
            return FocusedInputResult::Handled;
        }

        // Ctrl+click opens a link and never starts a selection. Releases fall
        // through so a drag that ends with Ctrl held still finishes.
        if pointer.modifiers.control
            && pointer.action
                == (RawPointerAction::Button {
                    button: RawPointerButton::Left,
                    state: RawInputState::Pressed,
                })
        {
            let context = {
                let term = self.session.lock();
                self.sync
                    .borrow()
                    .link_context(&term, point.row, point.column)
            };
            if let Some((uri, text, index)) = context {
                let url = uri
                    .filter(|u| is_web_url(u))
                    .or_else(|| find_url(&text, index).map(str::to_owned));
                *self.link.borrow_mut() = url;
            }
            return FocusedInputResult::Handled;
        }

        match pointer.action {
            RawPointerAction::Wheel(delta) => {
                let lines = wheel_lines(delta, metrics);
                if lines != 0 {
                    // A program on the alternate screen that asked for
                    // alternate scroll gets cursor keys instead.
                    let button = if lines > 0 {
                        MouseButton::WheelUp
                    } else {
                        MouseButton::WheelDown
                    };
                    if !self.session.send_alt_scroll(button, lines.unsigned_abs()) {
                        self.scroll(lines);
                    }
                }
            }
            RawPointerAction::Button {
                button: RawPointerButton::Left,
                state: RawInputState::Pressed,
            } => self.press(point, pointer.position, pointer.modifiers.shift),
            RawPointerAction::Move if let Some(gesture) = self.gesture.get() => {
                let under = self.unit_at(gesture.unit, point);
                let next = extend_selection(gesture.anchor, under);
                if self.selection.get() != Some(next) {
                    self.selection.set(Some(next));
                    self.touch();
                }
            }
            RawPointerAction::Button {
                button: RawPointerButton::Left,
                state: RawInputState::Released,
            } => {
                let gesture = self.gesture.take();
                // A click without a drag selects nothing; a double or
                // triple click keeps its word or line.
                let by_cell = gesture.is_none_or(|g| g.unit == SelectUnit::Cell);
                if by_cell
                    && let Some((start, end)) = self.selection.get()
                    && start == end
                {
                    self.selection.set(None);
                    self.touch();
                }
            }
            _ => return FocusedInputResult::Ignored,
        }
        FocusedInputResult::Handled
    }

    /// A left press outside a program that grabs the mouse. One click
    /// starts a selection by cell, two by word, three by line. Shift
    /// extends the current selection to the press instead.
    fn press(&self, point: TerminalGridPoint, position: schnellui::scene::Point, shift: bool) {
        let now = Instant::now();
        let count = click_count(self.last_click.get(), now, position);
        self.last_click.set(Some(Click {
            at: now,
            position,
            count,
        }));
        let current = self.selection.get();
        let (gesture, selection) = match current {
            Some((start, _)) if shift && count == 1 => {
                let gesture = Gesture {
                    unit: SelectUnit::Cell,
                    anchor: (start, start),
                };
                (gesture, (start, point))
            }
            _ => {
                let unit = match count {
                    1 => SelectUnit::Cell,
                    2 => SelectUnit::Word,
                    _ => SelectUnit::Line,
                };
                let anchor = self.unit_at(unit, point);
                (Gesture { unit, anchor }, anchor)
            }
        };
        self.gesture.set(Some(gesture));
        if current != Some(selection) {
            self.selection.set(Some(selection));
            self.touch();
        }
    }

    /// The cells of `unit` at `point`, in order.
    fn unit_at(
        &self,
        unit: SelectUnit,
        point: TerminalGridPoint,
    ) -> (TerminalGridPoint, TerminalGridPoint) {
        match unit {
            SelectUnit::Cell => (point, point),
            SelectUnit::Word => {
                let term = self.session.lock();
                let (start, end) = self
                    .sync
                    .borrow()
                    .word_bounds(&term, point.row, point.column);
                (
                    TerminalGridPoint::new(start.0, start.1),
                    TerminalGridPoint::new(end.0, end.1),
                )
            }
            SelectUnit::Line => {
                let term = self.session.lock();
                let cols = term.size().cols;
                let (first, last) = self.sync.borrow().line_bounds(&term, point.row);
                (
                    TerminalGridPoint::new(first, 0),
                    TerminalGridPoint::new(last, cols.saturating_sub(1)),
                )
            }
        }
    }

    fn scroll(&self, lines: isize) {
        let term = self.session.lock();
        let changed = self.sync.borrow_mut().scroll_by(&term, lines);
        drop(term);
        if changed {
            self.selection.set(None);
            self.touch();
        }
    }

    /// Input snaps the viewport back to live output and drops the selection.
    fn follow_output(&self) {
        let scrolled = self.sync.borrow_mut().scroll_to_bottom();
        let had_selection = self.selection.replace(None).is_some();
        if scrolled || had_selection {
            self.touch();
        }
    }
}

/// Positive values scroll back into history.
/// Input that waits for a resuming agent: keys, text and pastes. Pointer,
/// focus and copy go through.
fn holds(event: &FocusedInputEvent) -> bool {
    match event {
        FocusedInputEvent::Key(key) => key.state != RawInputState::Released,
        FocusedInputEvent::Ime(RawImeEvent::Commit(_)) => true,
        FocusedInputEvent::Clipboard(clipboard) => {
            !matches!(clipboard, FocusedClipboardEvent::Copy)
        }
        _ => false,
    }
}

fn wheel_lines(delta: RawWheelDelta, metrics: TerminalCellMetrics) -> isize {
    match delta {
        RawWheelDelta::Lines { y, .. } => (y * WHEEL_LINES).round() as isize,
        RawWheelDelta::Pixels { y, .. } => (y / metrics.height).round() as isize,
    }
}

fn is_web_url(url: &str) -> bool {
    crate::open_url::is_http_url(url)
}

/// The http or https URL in `line` that contains the char index `column`.
/// The URL ends at whitespace, minus trailing punctuation.
pub fn find_url(line: &str, column: usize) -> Option<&str> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if chars.get(column)?.1.is_whitespace() {
        return None;
    }
    let mut start = column;
    while start > 0 && !chars[start - 1].1.is_whitespace() {
        start -= 1;
    }
    let mut end = column + 1;
    while end < chars.len() && !chars[end].1.is_whitespace() {
        end += 1;
    }
    let from = chars[start].0;
    let to = chars.get(end).map_or(line.len(), |c| c.0);
    let token = &line[from..to];
    // The token may carry a prefix such as `(` or `<`; find the scheme in it.
    let scheme = ["https://", "http://"]
        .iter()
        .filter_map(|s| token.find(s))
        .min()?;
    let url = token[scheme..].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '\'', '"']);
    let url_start = from + scheme;
    // The click must land inside the trimmed URL.
    let click = chars[column].0;
    if click < url_start || click >= url_start + url.len() {
        return None;
    }
    (url.len() > "http://".len() && is_web_url(url)).then_some(url)
}

/// `title` without the folders and extension when it is only the path of a
/// program, as the console title ConPTY reports on Windows
/// (`C:\Program Files\WindowsApps\...\pwsh.exe` becomes `pwsh`). Any other
/// title stays as it is.
pub(crate) fn program_title(title: &str) -> &str {
    let windows_path = title.len() > 3
        && title.as_bytes()[0].is_ascii_alphabetic()
        && title[1..].starts_with(":\\");
    if !windows_path {
        return title;
    }
    let name = title.rsplit('\\').next().unwrap_or(title);
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return title;
    };
    let program = ["exe", "com", "bat", "cmd"]
        .iter()
        .any(|known| ext.eq_ignore_ascii_case(known));
    if program && !stem.is_empty() {
        stem
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::{Click, click_count, extend_selection, find_url};
    use schnellui::scene::Point;
    use schnellui::widgets::TerminalGridPoint;
    use std::time::{Duration, Instant};

    #[test]
    fn presses_close_together_count_up_to_three() {
        let at = Instant::now();
        let here = Point { x: 10.0, y: 10.0 };
        assert_eq!(click_count(None, at, here), 1);
        let mut last = Click {
            at,
            position: here,
            count: 1,
        };
        let soon = at + Duration::from_millis(200);
        assert_eq!(click_count(Some(last), soon, Point { x: 13.0, y: 8.0 }), 2);
        last.count = 3;
        assert_eq!(click_count(Some(last), soon, here), 1);
        last.count = 1;
        assert_eq!(
            click_count(Some(last), at + Duration::from_secs(1), here),
            1
        );
        assert_eq!(click_count(Some(last), soon, Point { x: 30.0, y: 10.0 }), 1);
    }

    #[test]
    fn a_drag_keeps_the_word_it_started_on() {
        let p = TerminalGridPoint::new;
        let word = (p(1, 4), p(1, 8));
        // Forward: from the word's start to the end of the word under the pointer.
        assert_eq!(
            extend_selection(word, (p(2, 0), p(2, 3))),
            (p(1, 4), p(2, 3))
        );
        // Backward: from the word's end to the start of the word under it.
        assert_eq!(
            extend_selection(word, (p(0, 2), p(0, 5))),
            (p(1, 8), p(0, 2))
        );
        // Inside the word the selection stays the word.
        assert_eq!(extend_selection(word, (p(1, 4), p(1, 8))), word);
        // By cell it is a plain drag.
        let cell = (p(1, 4), p(1, 4));
        assert_eq!(
            extend_selection(cell, (p(1, 2), p(1, 2))),
            (p(1, 4), p(1, 2))
        );
    }

    #[test]
    fn finds_url_from_any_column() {
        let line = "see https://example.com/a?b=1 now";
        for col in 4..29 {
            assert_eq!(find_url(line, col), Some("https://example.com/a?b=1"));
        }
        assert_eq!(find_url(line, 2), None);
        assert_eq!(find_url(line, 30), None);
    }

    #[test]
    fn trims_trailing_punctuation() {
        assert_eq!(find_url("(http://a.org/x).", 5), Some("http://a.org/x"));
        assert_eq!(
            find_url("go to \"https://a.org\", ok", 8),
            Some("https://a.org")
        );
        assert_eq!(find_url("http://a.org/x.", 14), None);
    }

    #[test]
    fn rejects_lines_without_urls_and_other_schemes() {
        assert_eq!(find_url("nothing here", 3), None);
        assert_eq!(find_url("ftp://example.com/file", 8), None);
        assert_eq!(find_url("file:///etc/passwd", 4), None);
        assert_eq!(find_url("http://", 2), None);
        assert_eq!(find_url("", 0), None);
    }

    #[test]
    fn column_is_a_char_index() {
        let line = "日本 https://a.org";
        assert_eq!(find_url(line, 3), Some("https://a.org"));
        assert_eq!(find_url(line, 0), None);
    }

    #[test]
    fn program_paths_title_tabs_by_the_program() {
        let store = r"C:\Program Files\WindowsApps\Microsoft.PowerShell_7.7.0.0_x64__8wekyb3d8bbwe\pwsh.exe";
        assert_eq!(super::program_title(store), "pwsh");
        assert_eq!(super::program_title(r"C:\WINDOWS\system32\cmd.exe"), "cmd");
        assert_eq!(
            super::program_title(r"C:\Users\me\repo"),
            r"C:\Users\me\repo"
        );
        assert_eq!(super::program_title("vim notes.md"), "vim notes.md");
        assert_eq!(super::program_title("MINGW64:/c/Users"), "MINGW64:/c/Users");
    }
}
