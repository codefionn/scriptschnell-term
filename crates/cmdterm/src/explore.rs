//! The explore panel: the worktree's history and a review of its changes,
//! read-only like the file panel. It has two modes:
//!
//! - History lists the commits reachable from the checked-out revision on
//!   the left and shows the selected one, message and diff, on the right.
//! - Review diffs the worktree against the point where it left its base
//!   branch, the way a change request would show it: the changed files on
//!   the left, the whole diff on the right with old and new line numbers.
//! - Change request shows the change request of the worktree's branch, as
//!   its forge reports it. The workspace asks the forge and hands the page
//!   in ([`ExplorePanel::set_change`]); [`crate::discussion`] renders it and
//!   `ui/change.rs` shows it with native widgets instead of the grid.
//!
//! History and review draw into one terminal grid and rewrite only the rows whose
//! [`RowKey`] changed, like [`crate::files`]. Every VCS query runs on a
//! thread of its own through the project's [`Environment`]. While the
//! selection moves through the history at most one `show` runs; the newest
//! selection waits for it and follows.

mod diff;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::discussion::Rendered;

use crate::term::Wake;
use crate::vcs::Commit;
use cmdterm_env::Environment;
use schnellui::raw_keyboard::{Key, NamedKey};
use schnellui::scene::Color;
use schnellui::widgets::{
    CursorIcon, TerminalCell, TerminalCellAttrs, TerminalCellMetrics, TerminalCellWidth,
    TerminalGrid, TerminalGridPatch, TerminalGridPoint,
};
use schnellui::{
    FocusedClipboardEvent, FocusedInputEvent, FocusedInputResult, RawInputState, RawKeyEvent,
    RawPointerAction, RawPointerButton, RawPointerEvent, RawWheelDelta,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::files::document::columns;
use crate::files::highlight::{self, Token};
use crate::files::{dragged, draw_thumb, on_thumb, put, put_until, thumb, write_cell};
use crate::i18n::Strings;
use crate::terminal_panel::GridStyle;
use crate::ui::ReaderColors;
use crate::workspace::MAX_PANES;
use diff::{Diff, Kind, NO_FILE};

/// Accessible names of the explore panel grids, by pane position, like
/// [`crate::terminal_panel::TERMINAL_NAMES`].
pub const EXPLORE_NAMES: [&str; MAX_PANES] = [
    "Explore",
    "Explore (pane 2)",
    "Explore (pane 3)",
    "Explore (pane 4)",
    "Explore (pane 5)",
    "Explore (pane 6)",
    "Explore (pane 7)",
    "Explore (pane 8)",
];

/// Narrower panels show only the focused half.
const NARROW: usize = 60;
const WHEEL_LINES: f32 = 3.0;
/// Columns one Left or Right press scrolls the reader.
const SIDE_STEP: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    History,
    Review,
    Change,
}

impl Mode {
    /// The next mode `v` switches to.
    fn next(self) -> Self {
        match self {
            Self::History => Self::Review,
            Self::Review => Self::Change,
            Self::Change => Self::History,
        }
    }
}

/// Results the loader threads hand to the UI thread.
#[derive(Default)]
struct Inbox {
    messages: Mutex<Vec<Message>>,
    /// Bumped with every message, so the grid patches on the next frame.
    revision: AtomicU64,
}

enum Message {
    Log {
        request: u64,
        result: Result<Vec<Commit>, String>,
    },
    Show {
        request: u64,
        /// Index of the commit in the log.
        index: usize,
        result: Result<Diff, String>,
    },
    Review {
        request: u64,
        result: Result<Reviewed, ReviewError>,
    },
}

struct Reviewed {
    /// The base as the user gave it, or the detected default.
    base: String,
    merge_base: String,
    commits: usize,
    diff: Diff,
}

enum ReviewError {
    NoBase,
    /// The base shares no commits with HEAD.
    Unrelated(String),
    Failed(String),
}

impl Inbox {
    fn send(&self, message: Message, wake: &Wake) {
        if let Ok(mut messages) = self.messages.lock() {
            messages.push(message);
        }
        self.revision.fetch_add(1, Ordering::Release);
        wake();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    List,
    Reader,
}

/// What one grid row shows. `valid` is false for rows not drawn yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowKey {
    valid: bool,
    generation: u32,
    /// List row plus one; zero for none.
    item: u32,
    selected: bool,
    /// Reader line plus one; zero for none.
    line: u32,
    left: u32,
    list_thumb: bool,
    reader_thumb: bool,
    /// The row shows the divider's grip.
    grip: bool,
    /// First and last selected grid column plus one; zero for none.
    selected_columns: (u32, u32),
}

enum Body {
    Empty,
    Loading,
    Diff(Diff),
    /// An error or an empty result, already worded.
    Message(String),
}

/// What the reader shows, with what the list and the scrolling need of it.
#[derive(Clone, Copy)]
enum Doc<'a> {
    Diff(&'a Diff),
}

impl<'a> Doc<'a> {
    fn lines(self) -> usize {
        match self {
            Self::Diff(diff) => diff.lines(),
        }
    }

    /// Cells of the widest line.
    fn width(self) -> usize {
        match self {
            Self::Diff(diff) => diff.width,
        }
    }

    /// Columns before the text of every line: a diff's line numbers or a
    /// page's blank column.
    fn margin(self) -> usize {
        match self {
            Self::Diff(diff) => gutter(diff),
        }
    }

    /// Columns before the text of line `line`.
    fn indent(self, line: usize) -> usize {
        match self {
            Self::Diff(diff) if diff.info(line).kind.is_code() => gutter(diff),
            _ => 1,
        }
    }

    fn text(self, line: usize) -> &'a str {
        match self {
            Self::Diff(diff) => diff.line(line),
        }
    }

    /// Entries of the list: files.
    fn entries(self) -> usize {
        match self {
            Self::Diff(diff) => diff.files.len(),
        }
    }

    /// The first line of file `index`.
    fn entry_line(self, index: usize) -> Option<usize> {
        match self {
            Self::Diff(diff) => diff.files.get(index).map(|file| file.line),
        }
    }

    /// The entry line `line` belongs to.
    fn entry_at(self, line: usize) -> Option<usize> {
        match self {
            Self::Diff(diff) => {
                let file = diff.info(line).file;
                (file != NO_FILE).then_some(file as usize)
            }
        }
    }
}

/// A scrollbar thumb or the divider held by the mouse. `grab` is where on the thumb the
/// press landed, in cells from its start.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    List {
        grab: f32,
    },
    Reader {
        grab: f32,
    },
    /// The divider between the list and the reader.
    Split,
}

/// Columns of the two halves.
#[derive(Clone, Copy)]
struct Layout {
    list: usize,
    reader_x: usize,
    reader: usize,
}

/// The list and the reader of one mode.
struct Page {
    /// Shown in the list while it has no rows.
    message: Option<String>,
    selected: usize,
    list_top: usize,
    body: Body,
    top: usize,
    left: usize,
    header: String,
    /// The first load started.
    loaded: bool,
}

impl Page {
    fn new() -> Self {
        Self {
            message: None,
            selected: 0,
            list_top: 0,
            body: Body::Empty,
            top: 0,
            left: 0,
            header: String::new(),
            loaded: false,
        }
    }

    fn doc(&self) -> Option<Doc<'_>> {
        match &self.body {
            Body::Diff(diff) => Some(Doc::Diff(diff)),
            _ => None,
        }
    }
}

struct View {
    mode: Mode,
    /// History, review, change request.
    pages: [Page; 3],
    commits: Vec<Commit>,
    /// Columns the widest commit row wants: hash, subject, refs and date.
    commits_width: usize,
    /// The list's width once the user drags the divider; `None` sizes it
    /// to the mode's rule.
    split: Option<usize>,
    /// The commit the history reader shows, by index into `commits`.
    shown: Option<usize>,
    /// A `show` is running.
    showing: bool,
    /// The commit to show once the running `show` is done.
    pending: Option<usize>,
    focus: Side,
    cols: usize,
    rows: usize,
    /// Bumped by every change that redraws all rows: a load, a focus or
    /// mode switch.
    generation: u32,
    selection: Option<(TerminalGridPoint, TerminalGridPoint)>,
    selecting: bool,
    drag: Option<Drag>,
    /// The newest request of each kind; replies to older ones are dropped.
    log_request: u64,
    show_request: u64,
    review_request: u64,
}

impl View {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            pages: [Page::new(), Page::new(), Page::new()],
            commits: Vec::new(),
            commits_width: 0,
            split: None,
            shown: None,
            showing: false,
            pending: None,
            focus: Side::List,
            cols: 80,
            rows: 24,
            generation: 0,
            selection: None,
            selecting: false,
            drag: None,
            log_request: 0,
            show_request: 0,
            review_request: 0,
        }
    }

    fn page(&self) -> &Page {
        &self.pages[self.mode as usize]
    }

    fn page_mut(&mut self) -> &mut Page {
        &mut self.pages[self.mode as usize]
    }

    fn doc(&self) -> Option<Doc<'_>> {
        self.page().doc()
    }

    fn redraw(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn layout(&self) -> Layout {
        let cols = self.cols;
        if cols < NARROW {
            return match self.focus {
                Side::List => Layout {
                    list: cols,
                    reader_x: cols,
                    reader: 0,
                },
                Side::Reader => Layout {
                    list: 0,
                    reader_x: 0,
                    reader: cols,
                },
            };
        }
        // Commit subjects want more room than file names. History takes
        // what its widest row wants, up to 40 %, so the diff keeps
        // most of the width and common subjects still fit.
        let list = match (self.split, self.mode) {
            // A dragged divider wins, keeping some room on both sides.
            (Some(split), _) => split.clamp(16, cols.saturating_sub(24).max(16)),
            (None, Mode::History) => self.commits_width.max(28).min((cols * 2 / 5).max(28)),
            (None, Mode::Review) => (cols * 3 / 10).clamp(20, 48),
            (None, Mode::Change) => (cols * 3 / 10).clamp(24, 48),
        };
        Layout {
            list,
            reader_x: list + 1,
            reader: cols - list - 1,
        }
    }

    /// Whether grid row `row` shows the grip, three heavier rows in the
    /// middle of the divider that mark it as draggable.
    /// An empty list has nothing to resize against, so it shows no grip.
    fn on_grip(&self, row: usize) -> bool {
        let middle = 1 + self.body_rows() / 2;
        self.list_len() > 0 && self.body_rows() >= 6 && row + 1 >= middle && row <= middle + 1
    }

    /// Rows below the header.
    fn body_rows(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    fn list_len(&self) -> usize {
        match self.mode {
            Mode::History => self.commits.len(),
            Mode::Review | Mode::Change => self.doc().map_or(0, Doc::entries),
        }
    }

    fn list_thumb(&self, layout: Layout) -> Option<(usize, usize)> {
        if layout.list == 0 {
            return None;
        }
        let rows = self.body_rows();
        thumb(self.list_len(), rows, self.page().list_top, rows)
    }

    fn reader_thumb(&self, layout: Layout) -> Option<(usize, usize)> {
        let doc = self.doc()?;
        if layout.reader == 0 {
            return None;
        }
        let rows = self.body_rows();
        thumb(doc.lines(), rows, self.page().top, rows)
    }

    /// Grid column where the text of reader line `line` starts.
    fn origin(&self, doc: Doc<'_>, line: usize, layout: Layout) -> usize {
        layout.reader_x + doc.indent(line)
    }

    fn key(&self, row: usize, layout: Layout) -> RowKey {
        let mut key = RowKey {
            valid: true,
            generation: self.generation,
            ..RowKey::default()
        };
        if row == 0 {
            return key;
        }
        let body_row = row - 1;
        let page = self.page();
        if layout.list > 0 {
            let index = page.list_top + body_row;
            if index < self.list_len() {
                key.item = index as u32 + 1;
                key.selected = index == page.selected;
            }
            key.list_thumb = on_thumb(self.list_thumb(layout), body_row);
            key.grip = self.on_grip(row);
        }
        if layout.reader > 0
            && let Some(doc) = self.doc()
        {
            let line = page.top + body_row;
            if line < doc.lines() {
                key.line = line as u32 + 1;
                key.left = page.left as u32;
            }
            key.reader_thumb = on_thumb(self.reader_thumb(layout), body_row);
            if let Some((from, to)) = self.selected_columns(row, layout) {
                key.selected_columns = (from as u32 + 1, to as u32 + 1);
            }
        }
        key
    }

    /// Moves the list selection to `index` and scrolls the list to it.
    fn select(&mut self, index: usize) {
        let len = self.list_len();
        let rows = self.body_rows();
        let page = self.page_mut();
        page.selected = index.min(len.saturating_sub(1));
        if page.selected < page.list_top {
            page.list_top = page.selected;
        } else if page.selected >= page.list_top + rows {
            page.list_top = page.selected + 1 - rows;
        }
    }

    fn scroll_list(&mut self, lines: isize) {
        let max = self.list_len().saturating_sub(self.body_rows());
        let page = self.page_mut();
        page.list_top = (page.list_top as isize + lines).clamp(0, max as isize) as usize;
    }

    fn scroll_reader(&mut self, lines: isize) {
        let rows = self.body_rows();
        let Some(lines_total) = self.doc().map(Doc::lines) else {
            return;
        };
        let max = lines_total.saturating_sub(rows);
        let page = self.page_mut();
        page.top = (page.top as isize + lines).clamp(0, max as isize) as usize;
        self.selection = None;
        self.follow();
    }

    fn scroll_sideways(&mut self, columns: isize) {
        let layout = self.layout();
        let Some(doc) = self.doc() else {
            return;
        };
        let text = layout.reader.saturating_sub(doc.margin() + 1);
        let max = doc.width().saturating_sub(text.max(1) / 2);
        let page = self.page_mut();
        page.left = (page.left as isize + columns).clamp(0, max as isize) as usize;
        self.selection = None;
    }

    /// In review and on the change request page, the list selection
    /// follows the entry at the top of the reader while the reader has
    /// focus.
    fn follow(&mut self) {
        if self.mode == Mode::History || self.focus != Side::Reader {
            return;
        }
        let page = self.page();
        let Some(doc) = page.doc() else {
            return;
        };
        let entry = doc.entry_at(page.top.min(doc.lines().saturating_sub(1)));
        if let Some(entry) = entry
            && entry != page.selected
        {
            self.select(entry);
        }
    }

    /// Scrolls the reader to the selected file or section.
    fn reveal_selected_file(&mut self) {
        let rows = self.body_rows();
        let page = self.page();
        let Some(doc) = page.doc() else {
            return;
        };
        if let Some(line) = doc.entry_line(page.selected) {
            let max = doc.lines().saturating_sub(rows);
            let page = self.page_mut();
            page.top = line.min(max);
            page.left = 0;
        }
        self.selection = None;
    }

    /// Scrolls the reader to the start of the next (`delta` 1) or previous
    /// (-1) file or section.
    fn jump_file(&mut self, delta: isize) {
        let rows = self.body_rows();
        let page = self.page();
        let Some(doc) = page.doc() else {
            return;
        };
        let top = page.top;
        let starts = (0..doc.entries()).filter_map(|index| doc.entry_line(index));
        let target = if delta > 0 {
            starts.clone().find(|line| *line > top)
        } else {
            starts.rev().find(|line| *line < top)
        };
        let lines = doc.lines();
        if let Some(line) = target {
            self.page_mut().top = line.min(lines.saturating_sub(rows));
        }
        self.selection = None;
        if self.mode != Mode::History {
            // Select the entry even where the reader cannot scroll it to the top.
            let page = self.page();
            if let Some(doc) = page.doc()
                && let Some(index) = target_index(doc, page.top, delta)
            {
                self.select(index);
            }
        }
    }

    fn set_focus(&mut self, side: Side) {
        if self.focus != side {
            self.focus = side;
            self.selection = None;
            self.redraw();
        }
    }

    /// Which scrollbar the cell at `row`, `column` belongs to, as a drag
    /// that grabs its thumb at `at` rows along the track.
    fn scrollbar_at(&self, row: usize, column: usize, at: f32) -> Option<Drag> {
        let layout = self.layout();
        row.checked_sub(1)?;
        let grab = |(start, len): (usize, usize)| {
            let offset = at - start as f32;
            if (0.0..len as f32).contains(&offset) {
                offset
            } else {
                len as f32 / 2.0
            }
        };
        if layout.list > 0
            && column == layout.list - 1
            && let Some(thumb) = self.list_thumb(layout)
        {
            return Some(Drag::List { grab: grab(thumb) });
        }
        if layout.reader > 0
            && column == layout.reader_x + layout.reader - 1
            && let Some(thumb) = self.reader_thumb(layout)
        {
            return Some(Drag::Reader { grab: grab(thumb) });
        }
        None
    }

    fn drag_to(&mut self, drag: Drag, at: f32) {
        let layout = self.layout();
        let rows = self.body_rows();
        match drag {
            Drag::List { grab } => {
                if let Some((_, len)) = self.list_thumb(layout) {
                    let range = self.list_len().saturating_sub(rows);
                    self.page_mut().list_top = dragged(at - grab, rows, len, range);
                }
            }
            Drag::Reader { grab } => {
                if let (Some((_, len)), Some(doc)) = (self.reader_thumb(layout), self.doc()) {
                    let range = doc.lines().saturating_sub(rows);
                    self.page_mut().top = dragged(at - grab, rows, len, range);
                    self.selection = None;
                    self.follow();
                }
            }
            // Moved by column in `pointer`.
            Drag::Split => {}
        }
    }

    /// The selection with its start before its end.
    fn ordered_selection(&self) -> Option<(TerminalGridPoint, TerminalGridPoint)> {
        let (start, end) = self.selection?;
        Some(if (start.row, start.column) <= (end.row, end.column) {
            (start, end)
        } else {
            (end, start)
        })
    }

    /// The grid columns of `row` the text selection covers, inclusive. It
    /// stays within the reader's text: never the list, the line numbers,
    /// the scrollbar or rows past the end.
    fn selected_columns(&self, row: usize, layout: Layout) -> Option<(usize, usize)> {
        let (start, end) = self.ordered_selection()?;
        let doc = self.doc()?;
        let line = (self.page().top + row).checked_sub(1)?;
        if row == 0 || row < start.row || row > end.row || line >= doc.lines() {
            return None;
        }
        let first = self.origin(doc, line, layout);
        let last = (layout.reader_x + layout.reader).checked_sub(2)?;
        let from = if row == start.row {
            start.column
        } else {
            first
        };
        let to = if row == end.row { end.column } else { last };
        let (from, to) = (from.max(first), to.min(last));
        (from <= to).then_some((from, to))
    }

    /// The selected text, one line per row.
    fn selection_text(&self, layout: Layout) -> Option<String> {
        let (start, end) = self.ordered_selection()?;
        let doc = self.doc()?;
        let mut text = String::new();
        let mut first = true;
        for row in start.row.max(1)..=end.row {
            let line = self.page().top + row - 1;
            if line >= doc.lines() {
                break;
            }
            let Some((from, to)) = self.selected_columns(row, layout) else {
                continue;
            };
            let origin = self.origin(doc, line, layout);
            let left = self.page().left;
            if !first {
                text.push('\n');
            }
            first = false;
            let from = from - origin + left;
            let to = if row == end.row {
                to - origin + left + 1
            } else {
                usize::MAX
            };
            text.push_str(columns(doc.text(line), from, to).trim_end());
        }
        (!first).then_some(text)
    }
}

/// The entry `jump_file` moved to: the last entry starting at or before
/// `top`, or for a forward jump that cannot scroll further, the next one.
fn target_index(doc: Doc<'_>, top: usize, delta: isize) -> Option<usize> {
    let at = (0..doc.entries())
        .rev()
        .find(|index| doc.entry_line(*index).is_some_and(|line| line <= top));
    if delta > 0 && top + 1 >= doc.lines().saturating_sub(1) {
        return Some(doc.entries().saturating_sub(1));
    }
    at
}

/// Columns of the line number gutter: a blank, the old and the new
/// number, the marker.
fn gutter(diff: &Diff) -> usize {
    diff.number_width * 2 + 5
}

pub struct ExplorePanel {
    env: Environment,
    root: PathBuf,
    /// Read the worktree with Jujutsu where it has a jj repository.
    jj: bool,
    /// The git repository recorded for the project, see
    /// [`crate::vcs::open`].
    known: Option<crate::vcs::KnownRepo>,
    wake: Wake,
    inbox: Arc<Inbox>,
    view: RefCell<View>,
    keys: RefCell<Vec<RowKey>>,
    /// Bumped for changes made on the UI thread, such as a key press.
    view_revision: Cell<u64>,
    /// The base branch the user chose; `None` uses `project_base`.
    base: RefCell<Option<String>>,
    /// The project's reference branch; `None` detects it.
    project_base: RefCell<Option<String>>,
    /// Review diffs to the files on disk instead of the checked-out commit.
    uncommitted: Cell<bool>,
    /// Set when the mode or a review option changed, until the app saves
    /// the layout.
    changed: Cell<bool>,
    /// Set by `b`, until the app opens the palette to ask for a base.
    ask_base: Cell<bool>,
    /// Set when the change request page wants its page, `true` to ask the
    /// forge again, until the app hands it in.
    ask_change: Cell<Option<bool>>,
    /// What the change request page shows.
    change: RefCell<ChangePage>,
    /// The entry of the change request page's list that is shown.
    section: Cell<usize>,
    /// Set while no extension shows discussions for the repository's
    /// forge, so the page points to the extensions and has nothing to
    /// reload.
    change_unsupported: Cell<bool>,
    /// Set when the panel's native views must be built again: the bar
    /// above every mode shows another mode, or the change request page
    /// changed. Stays set until the app remounts the panel.
    remount: Cell<bool>,
    shown_title: Cell<&'static str>,
    tr: Cell<&'static Strings>,
    /// The pointer's cursor where it last moved over the panel: a pointing
    /// hand over the list's rows, a resize arrow on the divider.
    cursor: Cell<CursorIcon>,
}

/// What the change request page shows: the page, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChangePage {
    Message(String),
    Ready(Arc<Rendered>),
}

impl ExplorePanel {
    /// The pointer's cursor over the panel, for the app's cursor provider.
    pub fn cursor(&self) -> CursorIcon {
        self.cursor.get()
    }

    /// A panel for the worktree at `root` on `env`, starting in `mode`.
    /// `jj` reads it with Jujutsu where it has a jj repository; `known` is
    /// the git repository recorded for its project. Reviews compare against
    /// `base`, else `project_base`, else the detected base.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env: Environment,
        root: PathBuf,
        jj: bool,
        known: Option<crate::vcs::KnownRepo>,
        mode: Mode,
        base: Option<String>,
        project_base: Option<String>,
        uncommitted: bool,
        wake: Wake,
        tr: &'static Strings,
    ) -> Rc<Self> {
        let panel = Rc::new(Self {
            env,
            root,
            jj,
            known,
            wake,
            inbox: Arc::default(),
            view: RefCell::new(View::new(mode)),
            keys: RefCell::new(Vec::new()),
            view_revision: Cell::new(0),
            cursor: Cell::new(CursorIcon::Default),
            base: RefCell::new(base),
            project_base: RefCell::new(project_base),
            uncommitted: Cell::new(uncommitted),
            changed: Cell::new(false),
            ask_base: Cell::new(false),
            ask_change: Cell::new(None),
            change: RefCell::new(ChangePage::Message(tr.discussion_loading.to_owned())),
            section: Cell::new(0),
            change_unsupported: Cell::new(false),
            remount: Cell::new(false),
            shown_title: Cell::new(""),
            tr: Cell::new(tr),
        });
        panel.load(false);
        panel
    }

    pub fn mode(&self) -> Mode {
        self.view.borrow().mode
    }

    pub fn base(&self) -> Option<String> {
        self.base.borrow().clone()
    }

    pub fn uncommitted(&self) -> bool {
        self.uncommitted.get()
    }

    fn title_str(&self) -> &'static str {
        let tr = self.tr.get();
        match self.mode() {
            Mode::History => tr.explore_history,
            Mode::Review => tr.explore_review,
            Mode::Change => tr.explore_change,
        }
    }

    /// Tab label: the mode.
    pub fn title(&self) -> String {
        self.title_str().to_owned()
    }

    /// Whether the title changed since the last call. Allocation-free.
    pub fn title_changed(&self) -> bool {
        let title = self.title_str();
        if std::ptr::eq(self.shown_title.get(), title) {
            return false;
        }
        self.shown_title.set(title);
        true
    }

    /// True once after the mode or a review option changed, so the app
    /// saves them.
    pub fn take_changed(&self) -> bool {
        self.changed.replace(false)
    }

    /// True once after `b` asked for another base branch.
    pub fn take_ask_base(&self) -> bool {
        self.ask_base.replace(false)
    }

    /// Once after the change request page wants its page: `Some(true)`
    /// when the forge should be asked again. Allocation-free.
    pub fn take_ask_change(&self) -> Option<bool> {
        self.ask_change.take()
    }

    /// The list labels of the change request page.
    #[cfg(test)]
    pub fn change_labels_for_test(&self) -> Vec<String> {
        match &*self.change.borrow() {
            ChangePage::Ready(rendered) => (rendered.sections.iter())
                .map(|s| s.label.clone())
                .collect(),
            ChangePage::Message(_) => Vec::new(),
        }
    }

    /// Turns to the change request page.
    pub fn show_change(&self) {
        self.set_mode(Mode::Change);
    }

    /// Turns to `mode`, as a click on its tab does.
    pub fn switch_mode(&self, mode: Mode) {
        self.set_mode(mode);
    }

    /// Runs the shown mode's query again, or asks the forge for the change
    /// request page again.
    pub fn reload(&self) {
        self.load(true);
    }

    /// Whether the panel shows its change request page, which the app
    /// builds from native widgets instead of the grid.
    pub fn shows_change(&self) -> bool {
        self.mode() == Mode::Change
    }

    /// What the change request page shows.
    /// Whether no extension shows discussions for the repository's forge.
    pub fn change_unsupported(&self) -> bool {
        self.change_unsupported.get()
    }

    pub fn change_page(&self) -> ChangePage {
        self.change.borrow().clone()
    }

    /// The entry of the change request page's list that is shown.
    pub fn section(&self) -> usize {
        self.section.get()
    }

    /// Shows entry `index` of the change request page's list.
    pub fn select_section(&self, index: usize) {
        let last = match &*self.change.borrow() {
            ChangePage::Ready(rendered) => rendered.sections.len().saturating_sub(1),
            ChangePage::Message(_) => 0,
        };
        let index = index.min(last);
        if index != self.section.get() {
            self.section.set(index);
            self.remount.set(true);
            (self.wake)();
        }
    }

    /// True once after the change request page's view changed or the
    /// panel turned to or from it, so the app builds the panel again.
    /// Allocation-free.
    pub fn take_remount(&self) -> bool {
        self.remount.replace(false)
    }

    /// Shows `page` on the change request page. The same page again
    /// changes nothing; a page that is loading again stays until the new
    /// one comes.
    pub fn set_change(&self, page: &crate::discussion::Page, tr: &'static Strings) {
        use crate::discussion::Page as Shown;
        let unsupported = matches!(page, Shown::Unsupported);
        if self.change_unsupported.replace(unsupported) != unsupported
            && self.mode() == Mode::Change
        {
            self.remount.set(true);
        }
        let shown = match page {
            Shown::Ready(rendered) => ChangePage::Ready(rendered.clone()),
            Shown::Loading if matches!(*self.change.borrow(), ChangePage::Ready(_)) => return,
            other => {
                ChangePage::Message(crate::forges::page_message(tr, other).unwrap_or_default())
            }
        };
        let mut change = self.change.borrow_mut();
        let same = match (&*change, &shown) {
            (ChangePage::Ready(a), ChangePage::Ready(b)) => Arc::ptr_eq(a, b),
            (a, b) => a == b,
        };
        if same {
            return;
        }
        // A reload keeps the shown entry where it still exists.
        if let ChangePage::Ready(rendered) = &shown {
            let last = rendered.sections.len().saturating_sub(1);
            self.section.set(self.section.get().min(last));
        }
        *change = shown;
        drop(change);
        self.view.borrow_mut().pages[Mode::Change as usize].loaded = true;
        if self.mode() == Mode::Change {
            self.remount.set(true);
        }
        (self.wake)();
    }

    /// Reloads the review after the worktree's status changed: now when the
    /// review is shown, else the next time it is.
    pub fn files_changed(&self) {
        if self.mode() == Mode::Review {
            self.load(true);
        } else {
            self.view.borrow_mut().pages[Mode::Review as usize].loaded = false;
        }
    }

    /// Reviews against `base`, or the detected default for `None`.
    pub fn set_base(&self, base: Option<String>) {
        *self.base.borrow_mut() = base;
        self.set_mode(Mode::Review);
        self.load(true);
        self.changed.set(true);
    }

    /// Takes the project's new reference branch, and runs a review that
    /// compares against it again.
    pub fn set_project_base(&self, project_base: Option<String>) {
        if *self.project_base.borrow() == project_base {
            return;
        }
        *self.project_base.borrow_mut() = project_base;
        if self.mode() == Mode::Review && self.base.borrow().is_none() {
            self.load(true);
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut view = self.view.borrow_mut();
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        if (view.cols, view.rows) != (cols, rows) {
            view.cols = cols;
            view.rows = rows;
            let selected = view.page().selected;
            view.select(selected);
            view.scroll_list(0);
            view.scroll_reader(0);
            view.redraw();
            drop(view);
            self.touch();
        }
    }

    fn touch(&self) {
        self.view_revision
            .set(self.view_revision.get().wrapping_add(1));
    }

    fn revision(&self) -> u64 {
        self.inbox
            .revision
            .load(Ordering::Acquire)
            .wrapping_add(self.view_revision.get())
    }

    fn set_mode(&self, mode: Mode) {
        let mut view = self.view.borrow_mut();
        if view.mode != mode {
            // The bar above the panel marks the shown mode.
            self.remount.set(true);
            view.mode = mode;
            view.selection = None;
            view.redraw();
            self.changed.set(true);
        }
        drop(view);
        self.load(false);
        self.touch();
    }

    /// Starts the query of the shown mode: always with `again`, else only
    /// the first time.
    fn load(&self, again: bool) {
        let mut view = self.view.borrow_mut();
        let tr = self.tr.get();
        let mode = view.mode;
        let page = view.page_mut();
        if page.loaded && !again {
            return;
        }
        page.loaded = true;
        if page.doc().is_none() {
            page.body = Body::Loading;
        }
        if mode == Mode::Review || view.list_len() == 0 {
            view.page_mut().message = Some(tr.files_loading.to_owned());
        }
        view.redraw();
        if mode == Mode::Change {
            // The workspace asks the forge; the page comes through `set_change`.
            drop(view);
            let mut change = self.change.borrow_mut();
            if let ChangePage::Message(message) = &*change
                && message != tr.discussion_loading
            {
                *change = ChangePage::Message(tr.discussion_loading.to_owned());
                self.remount.set(true);
            }
            drop(change);
            self.ask_change.set(Some(again));
            self.touch();
            return;
        }
        let env = self.env.clone();
        let jj = self.jj;
        let known = self.known.clone();
        let root = self.root.clone();
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        match mode {
            Mode::History => {
                view.log_request += 1;
                view.shown = None;
                let request = view.log_request;
                spawn(move || {
                    let result = crate::vcs::open(&env, &root, jj, known.as_ref())
                        .and_then(|repo| repo.log(&root))
                        .map_err(|error| error.to_string());
                    inbox.send(Message::Log { request, result }, &wake);
                });
            }
            Mode::Change => {}
            Mode::Review => {
                view.review_request += 1;
                let request = view.review_request;
                let base = self.base().or_else(|| self.project_base.borrow().clone());
                let uncommitted = self.uncommitted.get();
                spawn(move || {
                    let result = review(&env, &root, jj, known.as_ref(), base, uncommitted);
                    inbox.send(Message::Review { request, result }, &wake);
                });
            }
        }
        drop(view);
        self.touch();
    }

    /// Shows commit `index` in the history reader, after the running
    /// `show` if there is one.
    fn show(&self, view: &mut View, index: usize) {
        if view.shown == Some(index) && !view.showing {
            return;
        }
        if view.showing {
            view.pending = Some(index);
            return;
        }
        let Some(commit) = view.commits.get(index) else {
            return;
        };
        view.showing = true;
        view.pending = None;
        view.show_request += 1;
        let request = view.show_request;
        let id = commit.id.clone();
        let env = self.env.clone();
        let jj = self.jj;
        let known = self.known.clone();
        let root = self.root.clone();
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        spawn(move || {
            let result = crate::vcs::open(&env, &root, jj, known.as_ref())
                .and_then(|repo| repo.show(&root, &id))
                .map(Diff::new)
                .map_err(|error| error.to_string());
            inbox.send(
                Message::Show {
                    request,
                    index,
                    result,
                },
                &wake,
            );
        });
    }

    /// Takes what the loader threads finished. Allocation-free when there
    /// is nothing.
    fn receive(&self, view: &mut View) {
        let Ok(mut messages) = self.inbox.messages.lock() else {
            return;
        };
        if messages.is_empty() {
            return;
        }
        let taken: Vec<Message> = messages.drain(..).collect();
        drop(messages);
        let tr = self.tr.get();
        for message in taken {
            match message {
                Message::Log { request, result } if request == view.log_request => {
                    let page = &mut view.pages[Mode::History as usize];
                    match result {
                        Ok(commits) => {
                            page.message =
                                commits.is_empty().then(|| tr.explore_no_commits.to_owned());
                            if commits.is_empty() {
                                page.body = Body::Empty;
                            }
                            view.commits_width =
                                commits.iter().map(commit_width).max().unwrap_or(0);
                            view.commits = commits;
                        }
                        Err(error) => {
                            page.message = Some((tr.explore_failed)(&error));
                            page.body = Body::Empty;
                            view.commits.clear();
                        }
                    }
                    let mode = view.mode;
                    view.mode = Mode::History;
                    let selected = view.page().selected;
                    view.select(selected);
                    view.scroll_list(0);
                    view.mode = mode;
                    if !view.commits.is_empty() {
                        let selected = view.pages[Mode::History as usize].selected;
                        self.show(view, selected);
                    }
                }
                Message::Show {
                    request,
                    index,
                    result,
                } => {
                    view.showing = false;
                    if request == view.show_request {
                        let header = view.commits.get(index).map(|commit| {
                            // The list shows only the age; the header has
                            // the date.
                            let mut header = commit.short.clone();
                            header.push(' ');
                            header.push_str(&commit.subject);
                            header.push_str(" · ");
                            header.extend(date(commit.time).iter().map(|&b| char::from(b)));
                            header
                        });
                        let page = &mut view.pages[Mode::History as usize];
                        page.body = match result {
                            Ok(diff) => Body::Diff(diff),
                            Err(error) => Body::Message((tr.explore_failed)(&error)),
                        };
                        page.top = 0;
                        page.left = 0;
                        page.header = header.unwrap_or_default();
                        view.shown = Some(index);
                        if view.mode == Mode::History {
                            view.selection = None;
                        }
                    }
                    if let Some(next) = view.pending.take() {
                        self.show(view, next);
                    }
                }
                Message::Review { request, result } if request == view.review_request => {
                    let page = &mut view.pages[Mode::Review as usize];
                    match result {
                        Ok(reviewed) => {
                            page.header = review_header(&reviewed, self.uncommitted.get(), tr);
                            // One empty state, on the reader side, where
                            // it has room to say what would show up.
                            page.message = None;
                            // A reload keeps the scroll position where it still fits.
                            page.body = if reviewed.diff.files.is_empty() {
                                Body::Message(format!(
                                    "{} {}",
                                    (tr.explore_no_changes)(&reviewed.base),
                                    (tr.explore_no_changes_hint)(&reviewed.base)
                                ))
                            } else {
                                Body::Diff(reviewed.diff)
                            };
                        }
                        Err(error) => {
                            let text = match error {
                                ReviewError::NoBase => tr.explore_no_base.to_owned(),
                                ReviewError::Unrelated(base) => (tr.explore_unrelated)(&base),
                                ReviewError::Failed(error) => (tr.explore_review_failed)(&error),
                            };
                            page.header.clear();
                            // Said once, on the reader side, where it has
                            // room; the list stays empty.
                            page.message = None;
                            page.body = Body::Message(text);
                        }
                    }
                    let mode = view.mode;
                    view.mode = Mode::Review;
                    let selected = view.page().selected;
                    view.select(selected);
                    view.scroll_list(0);
                    view.scroll_reader(0);
                    view.scroll_sideways(0);
                    view.mode = mode;
                }
                Message::Log { .. } | Message::Review { .. } => {}
            }
        }
        view.redraw();
    }

    pub fn view(
        self: &Rc<Self>,
        style: &GridStyle,
        colors: ReaderColors,
        tr: &'static Strings,
        label: &str,
    ) -> TerminalGrid {
        self.tr.set(tr);
        let revision = self.clone();
        let patch = self.clone();
        let selection = style.selection;
        let grid = TerminalGrid::patched(
            move || revision.revision(),
            move |grid| patch.draw(grid, &colors, selection),
        )
        .metrics(style.metrics)
        .label(label);
        match &style.font_family {
            Some(family) => grid.font_family(family.clone()),
            None => grid,
        }
    }

    fn draw(&self, grid: &mut TerminalGridPatch<'_>, colors: &ReaderColors, selection: Color) {
        let mut view = self.view.borrow_mut();
        self.receive(&mut view);
        let view = &*view;
        let mut keys = self.keys.borrow_mut();
        let (cols, rows) = (view.cols.max(1), view.rows.max(1));
        let model = grid.model();
        if model.columns != cols || model.rows != rows {
            grid.resize(cols, rows);
            keys.clear();
        }
        let model = grid.model();
        if model.foreground != colors.ink || model.background != colors.page {
            grid.set_colors(colors.ink, colors.page);
            keys.clear();
        }
        keys.resize(rows, RowKey::default());
        let layout = view.layout();
        let tr = self.tr.get();
        for (row, shown) in keys.iter_mut().enumerate() {
            let key = view.key(row, layout);
            if *shown == key {
                continue;
            }
            *shown = key;
            let cells = grid.row_mut(row);
            draw_row(cells, row, view, layout, colors, tr);
            if let Some((from, to)) = view.selected_columns(row, layout) {
                for cell in cells.iter_mut().take(to + 1).skip(from) {
                    cell.background = selection;
                }
            }
        }
        grid.set_cursor(None);
    }

    /// Handles one focused input event.
    pub fn input(
        &self,
        event: FocusedInputEvent,
        metrics: TerminalCellMetrics,
    ) -> FocusedInputResult {
        let handled = match event {
            FocusedInputEvent::Key(key) => self.key(&key),
            FocusedInputEvent::Pointer(pointer) => self.pointer(pointer, metrics),
            FocusedInputEvent::Clipboard(FocusedClipboardEvent::Copy) => {
                let view = self.view.borrow();
                return match view.selection_text(view.layout()) {
                    Some(text) => FocusedInputResult::CopyText(text),
                    None => FocusedInputResult::Ignored,
                };
            }
            FocusedInputEvent::Clipboard(
                FocusedClipboardEvent::Paste(_) | FocusedClipboardEvent::PasteImage(_),
            ) => false,
            FocusedInputEvent::Focus(_) | FocusedInputEvent::Ime(_) => true,
        };
        if handled {
            FocusedInputResult::Handled
        } else {
            FocusedInputResult::Ignored
        }
    }

    fn key(&self, key: &RawKeyEvent) -> bool {
        if key.state == RawInputState::Released {
            return true;
        }
        let mods = key.modifiers;
        if mods.control || mods.alt || mods.super_key {
            return false;
        }
        #[derive(Clone, Copy)]
        enum Do {
            Up,
            Down,
            PageUp,
            PageDown,
            Home,
            End,
            Left,
            Right,
            Enter,
            Switch,
            /// Shift+Tab.
            SwitchBack,
            Back,
            NextFile,
            PreviousFile,
            Mode,
            Uncommitted,
            Base,
            Reload,
        }
        let action = match &key.logical_key {
            Key::Named(NamedKey::ArrowUp) => Do::Up,
            Key::Named(NamedKey::ArrowDown) => Do::Down,
            Key::Named(NamedKey::PageUp) => Do::PageUp,
            Key::Named(NamedKey::PageDown | NamedKey::Space) => Do::PageDown,
            Key::Named(NamedKey::Home) => Do::Home,
            Key::Named(NamedKey::End) => Do::End,
            Key::Named(NamedKey::ArrowLeft) => Do::Left,
            Key::Named(NamedKey::ArrowRight) => Do::Right,
            Key::Named(NamedKey::Enter) => Do::Enter,
            Key::Named(NamedKey::Tab) if mods.shift => Do::SwitchBack,
            Key::Named(NamedKey::Tab) => Do::Switch,
            Key::Named(NamedKey::Escape) => Do::Back,
            Key::Named(NamedKey::F5) => Do::Reload,
            Key::Character(text) => match text.as_str() {
                "k" => Do::Up,
                "j" => Do::Down,
                "h" => Do::Left,
                "l" => Do::Right,
                "g" => Do::Home,
                "G" => Do::End,
                " " => Do::PageDown,
                "n" | "]" => Do::NextFile,
                "p" | "[" => Do::PreviousFile,
                "v" => Do::Mode,
                "w" => Do::Uncommitted,
                "b" => Do::Base,
                "r" => Do::Reload,
                _ => return false,
            },
            _ => return false,
        };
        match action {
            Do::Mode => {
                self.set_mode(self.mode().next());
                return true;
            }
            Do::Uncommitted => {
                if self.mode() == Mode::Review {
                    self.uncommitted.set(!self.uncommitted.get());
                    self.changed.set(true);
                    self.load(true);
                }
                return true;
            }
            Do::Base => {
                self.ask_base.set(true);
                return true;
            }
            Do::Reload => {
                self.load(true);
                return true;
            }
            _ => {}
        }
        // Tab walks list, reader, then on to the next control; Shift+Tab
        // walks back from the list to the mode tabs above the grid. The
        // keys the grid does not take go to the window's focus order.
        if self.mode() != Mode::Change {
            let focus = self.view.borrow().focus;
            match (focus, action) {
                (Side::Reader, Do::Switch) | (Side::List, Do::SwitchBack) => return false,
                _ => {}
            }
        }
        if self.mode() == Mode::Change {
            // The list moves; the page's scroll view scrolls itself.
            let section = self.section.get();
            match action {
                Do::Up | Do::PreviousFile => self.select_section(section.saturating_sub(1)),
                Do::Down | Do::NextFile => self.select_section(section + 1),
                Do::Home => self.select_section(0),
                Do::End => self.select_section(usize::MAX),
                _ => return false,
            }
            return true;
        }
        let mut view = self.view.borrow_mut();
        let page = view.body_rows().saturating_sub(1).max(1) as isize;
        let before = view.page().selected;
        match (view.focus, action) {
            (Side::List, Do::Up) => view.select(before.saturating_sub(1)),
            (Side::List, Do::Down) => view.select(before + 1),
            (Side::List, Do::PageUp) => view.select(before.saturating_sub(page as usize)),
            (Side::List, Do::PageDown) => view.select(before + page as usize),
            (Side::List, Do::Home) => view.select(0),
            (Side::List, Do::End) => view.select(usize::MAX),
            (Side::List, Do::Left) => {}
            (Side::List, Do::Right | Do::Enter | Do::Switch) => {
                if matches!(action, Do::Enter) && view.mode != Mode::History {
                    view.reveal_selected_file();
                }
                view.set_focus(Side::Reader);
            }
            (Side::List, Do::Back) => return false,
            (Side::Reader, Do::Up) => view.scroll_reader(-1),
            (Side::Reader, Do::Down) => view.scroll_reader(1),
            (Side::Reader, Do::PageUp) => view.scroll_reader(-page),
            (Side::Reader, Do::PageDown) => view.scroll_reader(page),
            (Side::Reader, Do::Home) => view.scroll_reader(isize::MIN / 2),
            (Side::Reader, Do::End) => view.scroll_reader(isize::MAX / 2),
            (Side::Reader, Do::Left) => view.scroll_sideways(-(SIDE_STEP as isize)),
            (Side::Reader, Do::Right) => view.scroll_sideways(SIDE_STEP as isize),
            (Side::Reader, Do::Enter) => {}
            (Side::Reader, Do::Switch | Do::SwitchBack | Do::Back) => view.set_focus(Side::List),
            (Side::List, Do::SwitchBack) => {}
            (_, Do::NextFile) => view.jump_file(1),
            (_, Do::PreviousFile) => view.jump_file(-1),
            (_, Do::Mode | Do::Uncommitted | Do::Base | Do::Reload) => {}
        }
        if view.focus == Side::List && view.page().selected != before {
            self.selected_moved(&mut view);
        }
        drop(view);
        self.touch();
        true
    }

    /// The list selection moved: show that commit, or scroll to that file.
    fn selected_moved(&self, view: &mut View) {
        match view.mode {
            Mode::History => {
                let selected = view.page().selected;
                self.show(view, selected);
            }
            Mode::Review | Mode::Change => view.reveal_selected_file(),
        }
    }

    fn pointer(&self, pointer: RawPointerEvent, metrics: TerminalCellMetrics) -> bool {
        if self.mode() == Mode::Change {
            return false;
        }
        let mut view = self.view.borrow_mut();
        let column = (pointer.position.x / metrics.width).max(0.0) as usize;
        let row = (pointer.position.y / metrics.height).max(0.0) as usize;
        let column = column.min(view.cols.saturating_sub(1));
        let row = row.min(view.rows.saturating_sub(1));
        let layout = view.layout();
        let in_list = column < layout.list;
        // Fractional rows along the scrollbar tracks, from the body top.
        let at = pointer.position.y / metrics.height - 1.0;
        if let RawPointerAction::Move = pointer.action {
            let divider = layout.list > 0 && layout.reader > 0 && column == layout.list;
            self.cursor
                .set(if divider || view.drag == Some(Drag::Split) {
                    CursorIcon::ColResize
                } else if in_list && row > 0 && view.page().list_top + row - 1 < view.list_len() {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Default
                });
        }
        match pointer.action {
            RawPointerAction::Wheel(delta) => {
                let (x, y) = match delta {
                    RawWheelDelta::Lines { x, y } => (x * WHEEL_LINES, y * WHEEL_LINES),
                    RawWheelDelta::Pixels { x, y } => (x / metrics.width, y / metrics.height),
                };
                let (x, y) = (x.round() as isize, y.round() as isize);
                if in_list {
                    view.scroll_list(-y);
                } else {
                    let focus = view.focus;
                    // Wheeling the reader moves the review selection too.
                    view.focus = Side::Reader;
                    view.scroll_reader(-y);
                    view.focus = focus;
                    if x != 0 {
                        view.scroll_sideways(-x);
                    }
                }
            }
            RawPointerAction::Button {
                button: RawPointerButton::Left,
                state: RawInputState::Pressed,
            } => {
                if row == 0 {
                    return true;
                }
                if layout.list > 0 && layout.reader > 0 && column == layout.list {
                    // The divider drags to resize the two sides.
                    view.drag = Some(Drag::Split);
                } else if let Some(drag) = view.scrollbar_at(row, column, at) {
                    view.set_focus(if in_list { Side::List } else { Side::Reader });
                    view.drag = Some(drag);
                    view.drag_to(drag, at);
                } else if in_list {
                    let index = view.page().list_top + row - 1;
                    if index < view.list_len() {
                        view.set_focus(Side::List);
                        let before = view.page().selected;
                        view.select(index);
                        if view.page().selected != before || view.mode != Mode::History {
                            self.selected_moved(&mut view);
                        }
                    }
                } else if column >= layout.reader_x {
                    view.set_focus(Side::Reader);
                    let point = TerminalGridPoint::new(row, column);
                    view.selection = Some((point, point));
                    view.selecting = true;
                }
            }
            RawPointerAction::Move if view.drag == Some(Drag::Split) => {
                if view.split == Some(column) {
                    return true;
                }
                view.split = Some(column);
                view.redraw();
            }
            RawPointerAction::Move if view.drag.is_some() => {
                if let Some(drag) = view.drag {
                    view.drag_to(drag, at);
                }
            }
            RawPointerAction::Move if view.selecting => {
                let point = TerminalGridPoint::new(row.max(1), column.max(layout.reader_x));
                match view.selection {
                    Some((start, end)) if end != point => view.selection = Some((start, point)),
                    _ => return true,
                }
            }
            RawPointerAction::Button {
                button: RawPointerButton::Left,
                state: RawInputState::Released,
            } => {
                view.selecting = false;
                view.drag = None;
                if let Some((start, end)) = view.selection
                    && start == end
                {
                    view.selection = None;
                }
            }
            _ => return false,
        }
        drop(view);
        self.touch();
        true
    }
}

/// The review query, run on a loader thread.
fn review(
    env: &Environment,
    root: &std::path::Path,
    jj: bool,
    known: Option<&crate::vcs::KnownRepo>,
    base: Option<String>,
    uncommitted: bool,
) -> Result<Reviewed, ReviewError> {
    let failed = |error: crate::vcs::VcsError| match error {
        crate::vcs::VcsError::NoCommonHistory(base) => ReviewError::Unrelated(base),
        error => ReviewError::Failed(error.to_string()),
    };
    let repo = crate::vcs::open(env, root, jj, known).map_err(failed)?;
    let base = match base {
        Some(base) => base,
        None => repo
            .default_base(root)
            .map_err(failed)?
            .ok_or(ReviewError::NoBase)?,
    };
    let review = repo.review(root, &base, uncommitted).map_err(failed)?;
    Ok(Reviewed {
        base,
        merge_base: review.merge_base,
        commits: review.commits.len(),
        diff: Diff::new(review.patch),
    })
}

/// "against main · from 1a2b3c4 · 3 commits · 5 files · +120 −30".
fn review_header(reviewed: &Reviewed, uncommitted: bool, tr: &Strings) -> String {
    let short = reviewed.merge_base.get(..8).unwrap_or(&reviewed.merge_base);
    let diff = &reviewed.diff;
    // What changes the meaning of the diff comes first, so a narrow reader
    // cuts the counts rather than it.
    let mut parts = vec![(tr.explore_against)(&reviewed.base)];
    if uncommitted {
        parts.push(tr.explore_uncommitted.to_owned());
    }
    if diff.truncated {
        parts.push(tr.explore_truncated.to_owned());
    }
    parts.extend([
        (tr.files_count)(diff.files.len()),
        format!("+{} −{}", diff.added, diff.removed),
        (tr.explore_commits)(reviewed.commits),
        (tr.explore_from)(short),
    ]);
    parts.join(" · ")
}

fn spawn(job: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("cmdterm-explore".to_owned())
        .spawn(job);
}

fn clear(cells: &mut [TerminalCell], fg: Color, bg: Color) {
    for cell in cells.iter_mut() {
        cell.grapheme.clear();
        cell.foreground = fg;
        cell.background = bg;
        cell.attrs = TerminalCellAttrs::default();
        cell.width = TerminalCellWidth::Single;
        cell.hyperlink = None;
    }
}

const PLAIN: TerminalCellAttrs = TerminalCellAttrs {
    bold: false,
    italic: false,
    underline: false,
    strike: false,
    faint: false,
    inverse: false,
};

const BOLD: TerminalCellAttrs = TerminalCellAttrs {
    bold: true,
    ..PLAIN
};

fn draw_row(
    cells: &mut [TerminalCell],
    row: usize,
    view: &View,
    layout: Layout,
    colors: &ReaderColors,
    tr: &Strings,
) {
    let header = row == 0;
    // An empty header, as under an error, keeps the page color rather
    // than a band with nothing in it.
    let band = header && !view.page().header.is_empty();
    clear(cells, colors.ink, colors.page);
    if layout.list > 0 && layout.reader > 0 && layout.list < cells.len() {
        let divider = &mut cells[layout.list];
        if !header && view.on_grip(row) {
            divider.grapheme.push('┃');
            divider.foreground = colors.grip;
        } else {
            divider.grapheme.push('│');
            divider.foreground = colors.line;
        }
    }
    let (list, reader) = cells.split_at_mut(layout.reader_x.min(cells.len()));
    let list_width = layout.list.min(list.len());
    let list = &mut list[..list_width];
    // A focused reader carries an accent edge down its left side, as the
    // list's chosen row carries a bar while the list has focus, so the
    // keys' side always shows.
    if view.focus == Side::Reader
        && layout.list > 0
        && let Some(edge) = reader.first_mut()
    {
        edge.grapheme.clear();
        edge.grapheme.push('▎');
        edge.foreground = colors.accent;
    }
    let page = view.page();
    if header {
        // The band is the reader's: over the list it reads as an empty
        // chosen row. A pane too narrow for the reader shows the header
        // over the list instead.
        let narrow = layout.reader == 0 || reader.is_empty();
        // History's header repeats the chosen commit, which the list right
        // under it already shows; a narrow pane names the list instead.
        if narrow && view.mode == Mode::History {
            put(list, 1, tr.explore_list_commits, colors.muted, PLAIN);
            return;
        }
        // Beside the reader's band the list's first row names what it
        // lists, so the two columns start on the same row.
        if !narrow && !list.is_empty() {
            let heading = match view.mode {
                Mode::History => tr.explore_list_commits,
                _ => tr.explore_list_files,
            };
            put(list, 1, heading, colors.muted, PLAIN);
        }
        let reader = if narrow { list } else { reader };
        if band {
            for cell in reader.iter_mut() {
                cell.background = colors.header;
            }
        }
        // The modes are tabs in the native bar above the grid. When lines
        // run past the reader's edge, the header's right end says how to
        // see the rest, if the header leaves room. Allocation-free: both are written into cells.
        let wide = view
            .doc()
            .is_some_and(|doc| doc.width() > layout.reader.saturating_sub(doc.margin() + 1));
        let hint = tr.explore_sideways;
        let hint_at = reader
            .len()
            .saturating_sub(unicode_width::UnicodeWidthStr::width(hint) + 1);
        // The subject comes first: the hint shows only where the whole
        // header still fits before it. The header names the commit or the
        // counts of a review, which the hint must never cut.
        let header_width = unicode_width::UnicodeWidthStr::width(page.header.as_str());
        if wide && hint_at > header_width + 3 {
            put_until(
                reader,
                1,
                hint_at.saturating_sub(2),
                &page.header,
                colors.ink,
                PLAIN,
            );
            put_until(reader, hint_at, reader.len(), hint, colors.muted, PLAIN);
        } else {
            put(reader, 1, &page.header, colors.ink, PLAIN);
        }
        return;
    }
    let body_row = row - 1;
    if !list.is_empty() {
        let index = page.list_top + body_row;
        let selected = index == page.selected;
        if index < view.list_len() {
            if selected {
                let background = match view.focus {
                    Side::List => colors.selected,
                    Side::Reader => colors.unfocused,
                };
                for cell in list.iter_mut() {
                    cell.background = background;
                }
            }
            match view.mode {
                Mode::History => draw_commit(list, &view.commits[index], colors, selected),
                Mode::Review => {
                    if let Body::Diff(diff) = &page.body {
                        draw_file(list, &diff.files[index], diff.name_width, colors);
                    }
                }
                // Shown with native widgets instead.
                Mode::Change => {}
            }
            // The focused side's choice carries the rail's accent bar.
            if selected
                && view.focus == Side::List
                && let Some(cell) = list.first_mut()
                && cell.grapheme.is_empty()
            {
                cell.grapheme.push('┃');
                cell.foreground = colors.accent;
            }
        } else if body_row == 0
            && let Some(message) = &page.message
        {
            put(list, 1, message, colors.muted, PLAIN);
        }
        if on_thumb(view.list_thumb(layout), body_row)
            && let Some(cell) = list.last_mut()
        {
            draw_thumb(cell, '┃', colors);
        }
    }
    if reader.is_empty() {
        return;
    }
    let message = match &page.body {
        Body::Diff(diff) => {
            let (text, bar) = reader.split_at_mut(reader.len() - 1);
            let line = page.top + body_row;
            if line < diff.lines() {
                draw_line(text, diff, line, page.left, colors);
            }
            if on_thumb(view.reader_thumb(layout), body_row) {
                draw_thumb(&mut bar[0], '┃', colors);
            }
            return;
        }
        Body::Empty => match view.mode {
            Mode::History if view.commits.is_empty() => "",
            _ => tr.explore_pick,
        },
        Body::Loading => tr.files_loading,
        Body::Message(text) => text.as_str(),
    };
    // A message wraps at word breaks, so a long one stays whole, and sits
    // in the middle of the empty side, as the file panel's hint does.
    let width = reader.len().saturating_sub(4).max(1);
    let count = (0..)
        .take_while(|n| wrapped_line(message, width, *n).is_some())
        .count();
    let first = (view.body_rows() / 2).saturating_sub(count / 2);
    if let Some(line) = body_row
        .checked_sub(first)
        .and_then(|n| wrapped_line(message, width, n))
    {
        let cells = unicode_width::UnicodeWidthStr::width(line);
        put(
            reader,
            (reader.len().saturating_sub(cells) / 2).max(1),
            line,
            colors.muted,
            PLAIN,
        );
    }
}

/// Line `n` of `text` broken at spaces into lines of at most `width`
/// characters; a word longer than `width` gets a line of its own. Walks the
/// text without allocating, as it runs once per drawn row.
fn wrapped_line(text: &str, width: usize, n: usize) -> Option<&str> {
    let mut rest = text.trim_start();
    let mut index = 0;
    while !rest.is_empty() {
        let mut end = 0;
        let mut chars = 0;
        for (at, word) in rest.split_inclusive(' ').scan(0, |at, word| {
            let start = *at;
            *at += word.len();
            Some((start, word))
        }) {
            let len = word.trim_end().chars().count();
            if chars > 0 && chars + len > width {
                break;
            }
            end = at + word.len();
            chars += word.chars().count();
        }
        let (line, next) = rest.split_at(end.max(rest.len().min(1)));
        if index == n {
            return Some(line.trim_end());
        }
        rest = next.trim_start();
        index += 1;
    }
    None
}

/// `1a2b3c4 (main) Subject      2026-10-20`.
/// The columns `draw_commit` needs to show `commit` whole.
fn commit_width(commit: &Commit) -> usize {
    let names = commit.refs.strip_prefix("HEAD -> ").unwrap_or(&commit.refs);
    let refs = if names.is_empty() {
        0
    } else {
        names.chars().count() + 3
    };
    // A space, the hash, a space, the subject, the refs, then a gap and the age.
    1 + commit.short.chars().count() + 1 + commit.subject.chars().count() + refs + 2 + AGE + 2
}

fn draw_commit(cells: &mut [TerminalCell], commit: &Commit, colors: &ReaderColors, selected: bool) {
    let end = cells.len().saturating_sub(1);
    // The age goes right when there is room for it and some subject: three
    // cells such as `2d`, so the subject keeps the width a full date took.
    // The reader's header has the full date.
    let date_x = end.saturating_sub(AGE + 1);
    let text_end = if date_x >= 20 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        let age = age(commit.time, now);
        for (i, byte) in age.iter().enumerate() {
            write_cell(cells, date_x + i, char::from(*byte), 1, colors.muted, PLAIN);
        }
        date_x - 1
    } else {
        end
    };
    // On the selection's tint the brass hash falls under 4.5:1; the
    // selected row takes it in ink.
    let hash = if selected {
        colors.ink
    } else {
        colors.syntax[Token::Number as usize]
    };
    let column = put_until(cells, 1, text_end, &commit.short, hash, PLAIN) + 1;
    // `HEAD -> ` only says the branch is checked out; the list has no room
    // to spare for it.
    let names = commit.refs.strip_prefix("HEAD -> ").unwrap_or(&commit.refs);
    if names.is_empty() {
        put_until(cells, column, text_end, &commit.subject, colors.ink, PLAIN);
        return;
    }
    // The subject comes first so it reads at a glance. The refs follow it
    // only when they fit whole; the reader's commit line names them too.
    let refs_width = names.chars().count() + 2;
    let subject_width = commit.subject.chars().count();
    let after = put_until(cells, column, text_end, &commit.subject, colors.ink, PLAIN);
    if column + subject_width + 1 + refs_width > text_end {
        return;
    }
    // The sage refs share their hue with the selection's tint and fall to
    // 4:1 on it; the selected row takes them muted, which keeps 4.5:1.
    let refs = if selected {
        colors.muted
    } else {
        colors.syntax[Token::Type as usize]
    };
    let mut x = put_until(cells, after + 1, text_end, "(", refs, PLAIN);
    x = put_until(cells, x, text_end, names, refs, PLAIN);
    put_until(cells, x, text_end, ")", refs, PLAIN);
}

/// Columns a Review list needs before its rows show their counts.
const COUNTS_LIST: usize = 26;

/// `M lib.rs   src      +12 −3`. Long paths keep their end. In a narrow
/// list the counts go first, then the folder, so the file's name stays
/// whole as long as it can. The folders start in one column, after the
/// longest name of `name_width` cells, while that leaves them room.
fn draw_file(
    cells: &mut [TerminalCell],
    file: &diff::DiffFile,
    name_width: usize,
    colors: &ReaderColors,
) {
    let end = cells.len().saturating_sub(1);
    let status = status_color(file.status, colors);
    if end > 1 {
        write_cell(cells, 1, file.status, 1, status, BOLD);
    }
    let (folder, name) = match file.path.rsplit_once('/') {
        Some((folder, name)) if !name.is_empty() => (Some(folder), name),
        _ => (None, file.path.as_str()),
    };
    let counts_width =
        crate::files::digits(file.added as usize) + crate::files::digits(file.removed as usize) + 3;
    // A list narrower than COUNTS_LIST drops the counts on every row,
    // so they do not come and go from row to row; the reader shows them.
    let mut counts_x = end;
    let counts = cells.len() >= COUNTS_LIST && 3 + name.width() + 2 + counts_width <= end;
    if counts {
        counts_x = put_count(cells, counts_x, '−', file.removed, colors.removed);
        counts_x = put_count(
            cells,
            counts_x.saturating_sub(1),
            '+',
            file.added,
            colors.added,
        );
    }
    let path_end = if counts {
        counts_x.saturating_sub(2).max(3)
    } else {
        end
    };
    // The file's name first, its folder after it in the muted color: in a
    // narrow list the names, which tell rows apart, stay whole.
    match folder {
        Some(folder) => {
            let after = put_until(cells, 3, path_end, name, colors.ink, PLAIN);
            // One column for every folder while it leaves them a third of
            // the room; past that each folder follows its own name.
            let shared = 3 + name_width + 2;
            let column = if shared <= path_end.saturating_sub(path_end / 3) {
                shared.max(after + 2)
            } else {
                after + 2
            };
            if column < path_end {
                put_folder(cells, column, path_end, folder, colors.muted);
            }
        }
        None => put_tail(cells, 3, path_end, &file.path, colors.ink),
    }
}

/// Writes `folder` between `column` and `end`; a folder too long loses
/// whole leading segments behind `…`, so `crates/app/src/ui` becomes
/// `…/src/ui` and never `…p/src/ui`. When not even its last segment fits,
/// it writes nothing: two letters of a folder tell nothing.
fn put_folder(cells: &mut [TerminalCell], column: usize, end: usize, folder: &str, fg: Color) {
    let room = end.saturating_sub(column);
    if folder.width() <= room {
        put_until(cells, column, end, folder, fg, PLAIN);
        return;
    }
    for (at, _) in folder.match_indices('/') {
        let tail = &folder[at..];
        if tail.width() < room {
            write_cell(cells, column, '…', 1, fg, PLAIN);
            put_until(cells, column + 1, end, tail, fg, PLAIN);
            return;
        }
    }
    let last = folder.rsplit('/').next().unwrap_or(folder);
    if last.width() < room {
        write_cell(cells, column, '…', 1, fg, PLAIN);
        put_until(cells, column + 1, end, last, fg, PLAIN);
    }
}

fn status_color(status: char, colors: &ReaderColors) -> Color {
    crate::files::change_color(status, colors)
}

/// Writes `sign` and `n` so they end before column `end`. Returns the
/// first column written. Writes nothing when it does not fit.
fn put_count(cells: &mut [TerminalCell], end: usize, sign: char, n: u32, fg: Color) -> usize {
    let width = crate::files::digits(n as usize) + 1;
    let Some(start) = end.checked_sub(width) else {
        return end;
    };
    if start < 6 {
        return end;
    }
    write_cell(cells, start, sign, 1, fg, PLAIN);
    let mut n = n;
    for column in (start + 1..end).rev() {
        write_cell(
            cells,
            column,
            char::from(b'0' + (n % 10) as u8),
            1,
            fg,
            PLAIN,
        );
        n /= 10;
    }
    start
}

/// Writes `text` from `column` up to `end`. When it does not fit, its end
/// stays and its start becomes `…`.
fn put_tail(cells: &mut [TerminalCell], column: usize, end: usize, text: &str, fg: Color) {
    let room = end.saturating_sub(column);
    if text.width() <= room {
        put_until(cells, column, end, text, fg, PLAIN);
        return;
    }
    if room < 2 {
        return;
    }
    let mut skip = text.width() - (room - 1);
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        if skip == 0 {
            start = index;
            break;
        }
        skip = skip.saturating_sub(ch.width().unwrap_or(0));
        start = index + ch.len_utf8();
    }
    write_cell(cells, column, '…', 1, fg, PLAIN);
    put_until(cells, column + 1, end, &text[start..], fg, PLAIN);
}

/// Cells of a commit's age in the list.
const AGE: usize = 3;

/// How long before `now` the commit at `time` was made, right-aligned in
/// [`AGE`] cells: `now`, `12m`, ` 5h`, ` 3d`, ` 6w`, ` 2y`.
fn age(time: i64, now: i64) -> [u8; AGE] {
    let seconds = (now - time).max(0);
    let (value, unit) = match seconds {
        0..60 => return *b"now",
        60..3_600 => (seconds / 60, b'm'),
        3_600..86_400 => (seconds / 3_600, b'h'),
        86_400..604_800 => (seconds / 86_400, b'd'),
        604_800..31_536_000 => (seconds / 604_800, b'w'),
        _ => ((seconds / 31_536_000).min(99), b'y'),
    };
    let mut out = [b' '; AGE];
    out[2] = unit;
    out[1] = b'0' + (value % 10) as u8;
    if value >= 10 {
        out[0] = b'0' + (value / 10 % 10) as u8;
    }
    out
}

/// `YYYY-MM-DD` of `time`, seconds since the Unix epoch, in UTC.
fn date(time: i64) -> [u8; 10] {
    // Howard Hinnant's days-to-civil algorithm.
    let days = time.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (yoe + era * 400 + i64::from(month <= 2)).clamp(0, 9999);
    let mut out = *b"0000-00-00";
    let mut put = |at: usize, mut value: i64, digits: usize| {
        for i in (0..digits).rev() {
            out[at + i] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    };
    put(0, year, 4);
    put(5, month, 2);
    put(8, day, 2);
    out
}

/// Draws reader line `line` of `diff`, scrolled `left` columns, into
/// `cells`. Code gets line numbers, a marker and the file's highlighting;
/// file lines get a bar with the path and counts.
fn draw_line(
    cells: &mut [TerminalCell],
    diff: &Diff,
    line: usize,
    left: usize,
    colors: &ReaderColors,
) {
    let info = diff.info(line);
    let text = diff.line(line);
    match info.kind {
        Kind::Context | Kind::Added | Kind::Removed => {
            let (page, marker, mark) = match info.kind {
                Kind::Added => (colors.added_page, '+', colors.added),
                Kind::Removed => (colors.removed_page, '-', colors.removed),
                _ => (colors.page, ' ', colors.muted),
            };
            let gutter = gutter(diff);
            if cells.len() <= gutter {
                return;
            }
            for cell in cells.iter_mut() {
                cell.background = page;
            }
            let width = diff.number_width;
            put_number(cells, width + 1, info.old, colors.muted);
            put_number(cells, width * 2 + 2, info.new, colors.muted);
            write_cell(cells, width * 2 + 3, marker, 1, mark, PLAIN);
            let language = diff.file(line).and_then(|file| file.language);
            let code = &mut cells[gutter..];
            match language {
                Some(language) if text.len() <= crate::files::document::MAX_HIGHLIGHT => {
                    let mut state = info.state;
                    let mut column = 0;
                    highlight::highlight(language, &mut state, text, |start, end, token| {
                        let attrs = TerminalCellAttrs {
                            italic: token == Token::Comment,
                            bold: token == Token::Heading,
                            ..TerminalCellAttrs::default()
                        };
                        let fg = colors.syntax[token as usize];
                        column = paint(code, column, &text[start..end], left, fg, attrs);
                    });
                    mark_clipped(code, column, left, colors.muted);
                }
                _ => {
                    let column = paint(code, 0, text, left, colors.ink, PLAIN);
                    mark_clipped(code, column, left, colors.muted);
                }
            }
        }
        Kind::File => {
            for cell in cells.iter_mut() {
                cell.background = colors.header;
            }
            let Some(file) = diff.file(line) else {
                return;
            };
            let end = cells.len();
            if end > 2 {
                write_cell(
                    cells,
                    1,
                    file.status,
                    1,
                    status_color(file.status, colors),
                    BOLD,
                );
            }
            let column = put_until(cells, 3, end, &file.path, colors.ink, BOLD);
            let mut counts = [0u8; 24];
            let counts = counts_text(&mut counts, file.added, file.removed);
            let column = put_until(cells, column + 2, end, counts.0, colors.added, PLAIN);
            put_until(cells, column + 1, end, counts.1, colors.removed, PLAIN);
        }
        Kind::Hunk => {
            let column = paint(&mut cells[1..], 0, text, left, colors.hunk, PLAIN);
            mark_clipped(&mut cells[1..], column, left, colors.muted);
        }
        Kind::Meta => {
            let column = paint(&mut cells[1..], 0, text, left, colors.muted, PLAIN);
            mark_clipped(&mut cells[1..], column, left, colors.muted);
        }
        Kind::Text => {
            // The lines about a commit (hash, refs, author, date) step back
            // in the muted color, so its message reads first.
            let about = ["commit ", "changeset:", "Refs:", "Author:", "Date:", "---"]
                .iter()
                .any(|prefix| text.starts_with(prefix));
            let color = if about { colors.muted } else { colors.ink };
            let column = paint(&mut cells[1..], 0, text, left, color, PLAIN);
            // A diffstat row, `path | 12 ++++--`: its bar takes the added
            // and removed colors. Allocation-free: it recolors cells.
            if text.starts_with(' ') && text.contains(" | ") {
                let mut bar = false;
                for cell in &mut cells[1..] {
                    match cell.grapheme.as_str() {
                        "|" => bar = true,
                        "+" if bar => cell.foreground = colors.added,
                        "-" if bar => cell.foreground = colors.removed,
                        _ => {}
                    }
                }
            }
            mark_clipped(&mut cells[1..], column, left, colors.muted);
        }
    }
}

/// Ends a line that runs past the right edge with `…`, so a cut line reads
/// as cut; scrolling right shows the rest.
fn mark_clipped(cells: &mut [TerminalCell], end: usize, left: usize, color: Color) {
    if end > left + cells.len()
        && let Some(last) = cells.last_mut()
    {
        last.grapheme.clear();
        last.grapheme.push('…');
        last.foreground = color;
        last.width = TerminalCellWidth::Single;
    }
}

/// `+12` and `−3` written into `buffer` without allocating.
fn counts_text(buffer: &mut [u8; 24], added: u32, removed: u32) -> (&str, &str) {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buffer[..]);
    let _ = write!(cursor, "+{added}");
    let split = cursor.position() as usize;
    let _ = write!(cursor, "−{removed}");
    let end = cursor.position() as usize;
    let text = std::str::from_utf8(&buffer[..end]).unwrap_or("");
    text.split_at(split)
}

/// Writes `n` right-aligned so it ends before column `end`; nothing for 0.
fn put_number(cells: &mut [TerminalCell], end: usize, n: u32, fg: Color) {
    if n == 0 {
        return;
    }
    let mut n = n;
    let mut at = end;
    while n > 0 && at > 0 {
        at -= 1;
        write_cell(cells, at, char::from(b'0' + (n % 10) as u8), 1, fg, PLAIN);
        n /= 10;
    }
}

/// Paints `part`, which starts `column` cells into the line, into `cells`
/// scrolled `left` cells. Returns the column after `part`.
fn paint(
    cells: &mut [TerminalCell],
    mut column: usize,
    part: &str,
    left: usize,
    fg: Color,
    attrs: TerminalCellAttrs,
) -> usize {
    let width = cells.len();
    for ch in part.chars() {
        let wide = ch.width().unwrap_or(0);
        if wide == 0 {
            continue;
        }
        if column >= left && column + wide <= left + width {
            write_cell(cells, column - left, ch, wide, fg, attrs);
        }
        column += wide;
    }
    column
}

#[cfg(test)]
mod wrap_tests {
    use super::wrapped_line;

    #[test]
    fn messages_wrap_at_spaces() {
        let text = "one two three four";
        assert_eq!(wrapped_line(text, 9, 0), Some("one two"));
        assert_eq!(wrapped_line(text, 9, 1), Some("three"));
        assert_eq!(wrapped_line(text, 9, 2), Some("four"));
        assert_eq!(wrapped_line(text, 9, 3), None);
        assert_eq!(wrapped_line("short", 40, 0), Some("short"));
        assert_eq!(wrapped_line("averylongword x", 4, 0), Some("averylongword"));
        assert_eq!(wrapped_line("averylongword x", 4, 1), Some("x"));
        assert_eq!(wrapped_line("", 4, 0), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cells: &[TerminalCell]) -> String {
        cells
            .iter()
            .map(|cell| match cell.grapheme.as_str() {
                "" => " ",
                grapheme => grapheme,
            })
            .collect()
    }

    #[test]
    fn a_review_against_a_base_with_no_shared_commits_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(repo)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("README"), "hi\n").unwrap();
        git(&["add", "README"]);
        git(&["commit", "-qm", "init"]);
        // A branch rewritten from its root shares nothing with main.
        git(&["checkout", "-q", "--orphan", "fresh"]);
        git(&["commit", "-qm", "fresh start"]);
        let env = cmdterm_env::Environment::Local;
        match review(&env, repo, false, None, Some("main".to_owned()), false) {
            Err(ReviewError::Unrelated(base)) => assert_eq!(base, "main"),
            Err(ReviewError::Failed(error)) => panic!("failed: {error}"),
            Err(ReviewError::NoBase) => panic!("no base"),
            Ok(_) => panic!("reviewed unrelated histories"),
        }
        // A base that does not exist is still an error of its own.
        assert!(matches!(
            review(&env, repo, false, None, Some("nowhere".to_owned()), false),
            Err(ReviewError::Failed(_))
        ));
    }

    #[test]
    fn dates_are_civil_utc() {
        assert_eq!(&age(100, 130), b"now");
        assert_eq!(&age(0, 12 * 60), b"12m");
        assert_eq!(&age(0, 5 * 3_600), b" 5h");
        assert_eq!(&age(0, 3 * 86_400), b" 3d");
        assert_eq!(&age(0, 52 * 604_800), b"52w");
        assert_eq!(&age(0, 400 * 31_536_000), b"99y");
        assert_eq!(&date(0), b"1970-01-01");
        assert_eq!(&date(951_782_400), b"2000-02-29");
        assert_eq!(&date(1_792_454_400), b"2026-10-20");
        assert_eq!(&date(-86_400), b"1969-12-31");
    }

    #[test]
    fn code_lines_draw_numbers_marker_and_wash() {
        let colors = crate::ui::resolve_scheme("spruce", true).reader_colors();
        let diff = Diff::new(
            b"diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -9,2 +9,2 @@\n-let x = 1;\n+let x = 2;\n fn f() {}\n"
                .to_vec(),
        );
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 40];
        draw_line(&mut cells, &diff, 3, 0, &colors);
        assert_eq!(text(&cells).trim_end(), "     9 + let x = 2;");
        assert_eq!(cells[7].foreground, colors.added);
        assert_eq!(cells[0].background, colors.added_page);
        assert_eq!(cells[9].foreground, colors.syntax[Token::Keyword as usize]);
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 40];
        draw_line(&mut cells, &diff, 4, 0, &colors);
        assert_eq!(text(&cells).trim_end(), " 10 10   fn f() {}");
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 40];
        draw_line(&mut cells, &diff, 0, 0, &colors);
        assert_eq!(text(&cells).trim_end(), " M a.rs  +1 −1");
    }

    #[test]
    fn a_diffstat_bar_takes_the_added_and_removed_colors() {
        let colors = crate::ui::resolve_scheme("spruce", true).reader_colors();
        let diff = Diff::new(b" a.rs | 2 +-\n".to_vec());
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 20];
        draw_line(&mut cells, &diff, 0, 0, &colors);
        assert_eq!(text(&cells).trim_end(), "  a.rs | 2 +-");
        assert_eq!(cells[11].foreground, colors.added);
        assert_eq!(cells[12].foreground, colors.removed);
        assert_eq!(cells[2].foreground, colors.ink);
    }

    #[test]
    fn a_dragged_divider_sizes_the_list_within_bounds() {
        let mut view = View::new(Mode::History);
        view.cols = 120;
        view.commits_width = 90;
        assert_eq!(view.layout().list, 48);
        view.split = Some(70);
        assert_eq!(view.layout().list, 70);
        assert_eq!(view.layout().reader_x, 71);
        // Both sides keep some room.
        view.split = Some(119);
        assert_eq!(view.layout().list, 96);
        view.split = Some(2);
        assert_eq!(view.layout().list, 16);
    }

    #[test]
    fn commit_rows_put_the_age_right_when_there_is_room() {
        let colors = crate::ui::resolve_scheme("spruce", true).reader_colors();
        let commit = Commit {
            id: "18cb7d1abc".to_owned(),
            short: "18cb7d1".to_owned(),
            author: "t".to_owned(),
            time: 1_792_454_400,
            refs: "HEAD -> feature".to_owned(),
            subject: "say bye".to_owned(),
        };
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 26];
        draw_commit(&mut cells, &commit, &colors, false);
        // Refs that do not fit whole stay out; the reader names them.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let age = String::from_utf8(age(commit.time, now).to_vec()).unwrap();
        assert_eq!(text(&cells), format!(" 18cb7d1 say bye     {age}  "));
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 60];
        draw_commit(&mut cells, &commit, &colors, false);
        assert!(text(&cells).starts_with(" 18cb7d1 say bye (feature) "));
    }

    #[test]
    fn file_rows_put_the_name_before_the_end_of_its_folder() {
        let colors = crate::ui::resolve_scheme("spruce", true).reader_colors();
        let file = diff::DiffFile {
            path: "some/deeply/nested/folder/file.rs".to_owned(),
            status: 'A',
            added: 12,
            removed: 0,
            line: 0,
            language: None,
        };
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 28];
        draw_file(&mut cells, &file, 7, &colors);
        assert_eq!(text(&cells), " A file.rs  …folder  +12 −0 ");
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 36];
        draw_file(&mut cells, &file, 7, &colors);
        assert_eq!(text(&cells), " A file.rs  …/nested/folder  +12 −0 ");
        // Too narrow for the counts: they go, the name stays whole, and a
        // folder with no room for its last segment is left out.
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 16];
        draw_file(&mut cells, &file, 7, &colors);
        assert_eq!(text(&cells), " A file.rs      ");
    }

    #[test]
    fn the_review_selection_follows_the_reader() {
        let mut view = View::new(Mode::Review);
        let patch =
            "diff --git a/a b/a\n@@ -1 +1 @@\n-x\n+y\ndiff --git a/b b/b\n@@ -1 +1 @@\n-x\n+y\n";
        view.pages[1].body = Body::Diff(Diff::new(patch.as_bytes().to_vec()));
        view.rows = 3;
        view.focus = Side::Reader;
        view.scroll_reader(4);
        assert_eq!(view.page().top, 4);
        assert_eq!(view.page().selected, 1);
        view.scroll_reader(-4);
        assert_eq!(view.page().selected, 0);
        view.jump_file(1);
        assert_eq!((view.page().top, view.page().selected), (4, 1));
        view.jump_file(-1);
        assert_eq!((view.page().top, view.page().selected), (0, 0));
    }
}
