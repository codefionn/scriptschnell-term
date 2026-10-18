//! The file panel: the worktree's file tree on the left, the open file with
//! line numbers and syntax highlighting on the right. It only reads; editing
//! happens in an editor in a terminal. Markdown, pictures, SVG and PDF files
//! open rendered ([`render`]); `v` switches a file that is also text between
//! its rendered form and its source.
//!
//! Both halves draw into one terminal grid and use its retained rows. Each
//! row remembers what it shows ([`RowKey`]) and is rewritten only when that
//! changes: moving the tree selection rewrites two rows, scrolling rewrites
//! the rows that moved, an idle frame rewrites none and allocates nothing.
//! Listing the files and reading one run on threads of their own through the
//! project's [`Environment`], so a remote project works like a local one and
//! the UI never waits for either.
//!
//! The same panel shows the Markdown views of extensions (`term.view`):
//! then it has no tree, and the app hands it the text the extension rendered
//! ([`FilesPanel::extension_view`]).

pub(crate) mod document;
pub(crate) mod highlight;
pub(crate) mod markdown;
mod render;
mod tree;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::term::Wake;
use cmdterm_env::Environment;
use schnellui::raw_keyboard::{Key, NamedKey};
use schnellui::scene::Color;
use schnellui::widgets::{
    CursorIcon, TerminalCell, TerminalCellAttrs, TerminalCellMetrics, TerminalCellWidth,
    TerminalGrid, TerminalGridPatch, TerminalGridPoint, TerminalImage, TerminalImagePlacement,
};
use schnellui::{
    FocusedClipboardEvent, FocusedInputEvent, FocusedInputResult, RawInputState, RawKeyEvent,
    RawPointerAction, RawPointerButton, RawPointerEvent, RawWheelDelta,
};
use unicode_width::UnicodeWidthChar;

use crate::i18n::Strings;
use crate::terminal_panel::GridStyle;
use crate::ui::ReaderColors;
use crate::workspace::MAX_PANES;
use document::{Document, MAX_HIGHLIGHT};
use highlight::Token;
use markdown::{Ink, Laid};
use render::{Opened, Pdf, Picture, RASTER, Rendered};
use tree::{NodeId, Tree};

/// Accessible names of the file panel grids, by pane position, like
/// [`crate::terminal_panel::TERMINAL_NAMES`].
pub const FILES_NAMES: [&str; MAX_PANES] = [
    "Files",
    "Files (pane 2)",
    "Files (pane 3)",
    "Files (pane 4)",
    "Files (pane 5)",
    "Files (pane 6)",
    "Files (pane 7)",
    "Files (pane 8)",
];

/// Narrower panels show only the focused half.
const NARROW: usize = 60;
const WHEEL_LINES: f32 = 3.0;
/// Columns one Left or Right press scrolls the reader.
const SIDE_STEP: usize = 8;
/// Share of the ink, out of 256, in the hovered tree row's background.
const HOVER_SHARE: u16 = 28;
/// Id of the one image a panel places on its grid.
const PICTURE_ID: u64 = 1;

/// Results the loader threads hand to the UI thread.
#[derive(Default)]
struct Inbox {
    messages: Mutex<Vec<Message>>,
    /// Bumped with every message, so the grid patches on the next frame.
    revision: AtomicU64,
}

enum Message {
    Tree(Result<Tree, String>),
    /// The worktree's changes, as [`crate::vcs::Repository::changes`] lists
    /// them, for the tree already shown.
    Changes(Vec<u8>),
    File {
        /// Matches [`View::request`] unless a newer request replaced it.
        request: u64,
        result: Result<Box<Opened>, String>,
    },
    /// A PDF page, rendered for the file of `request`.
    Page {
        request: u64,
        page: usize,
        result: Result<Picture, String>,
    },
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
    Tree,
    Reader,
}

/// What one grid row shows. `valid` is false for rows not drawn yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RowKey {
    valid: bool,
    generation: u32,
    /// Tree node plus one; zero for none.
    node: u32,
    selected: bool,
    expanded: bool,
    /// Whether the pointer rests on the row's tree node.
    hovered: bool,
    /// File line plus one; zero for none.
    line: u32,
    left: u32,
    /// Whether the row shows the thumb of the tree's or the reader's
    /// vertical scrollbar.
    tree_thumb: bool,
    reader_thumb: bool,
    /// Start and length of the horizontal thumb on the reader's scrollbar
    /// row; zero length elsewhere.
    side_thumb: (u32, u32),
    /// First and last selected grid column plus one; zero for none.
    selected_columns: (u32, u32),
}

enum Body {
    /// No file chosen yet.
    Empty,
    Loading,
    /// A file with a source, a rendered form, or both.
    File(Box<Opened>),
    /// A binary file or a read error, already worded.
    Message(String),
}

/// The text the reader shows: a file's source with line numbers, or its
/// rendered Markdown.
#[derive(Clone, Copy)]
enum Text<'a> {
    Source(&'a Document),
    Rendered(&'a Laid),
}

impl<'a> Text<'a> {
    fn lines(self) -> usize {
        match self {
            Self::Source(document) => document.lines(),
            Self::Rendered(laid) => laid.lines(),
        }
    }

    fn line(self, line: usize) -> &'a str {
        match self {
            Self::Source(document) => document.line(line),
            Self::Rendered(laid) => laid.line(line),
        }
    }

    /// Cells of the widest line.
    fn width(self) -> usize {
        match self {
            Self::Source(document) => document.width,
            Self::Rendered(laid) => laid.width,
        }
    }

    /// Columns left of the text: the line number gutter of the source, one
    /// blank column for Markdown.
    fn gutter(self) -> usize {
        match self {
            Self::Source(document) => digits(document.lines()) + 2,
            Self::Rendered(_) => 1,
        }
    }
}

/// A scrollbar thumb held by the mouse. `grab` is where on the thumb the
/// press landed, in cells from its start.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Tree { grab: f32 },
    Reader { grab: f32 },
    Sideways { grab: f32 },
}

/// Columns of the two halves.
#[derive(Clone, Copy)]
struct Layout {
    tree: usize,
    reader_x: usize,
    reader: usize,
}

struct View {
    tree: Option<Tree>,
    /// Shown in the tree half while there is no tree to show.
    tree_message: Option<String>,
    visible: Vec<NodeId>,
    /// Index into `visible`.
    selected: usize,
    tree_top: usize,
    body: Body,
    top: usize,
    left: usize,
    focus: Side,
    cols: usize,
    rows: usize,
    /// Bumped by every change that redraws all rows: a load, a focus switch.
    generation: u32,
    tree_header: String,
    header: String,
    selection: Option<(TerminalGridPoint, TerminalGridPoint)>,
    selecting: bool,
    drag: Option<Drag>,
    /// The newest file request; replies to older ones are dropped.
    request: u64,
    /// Show a file's rendered form rather than its source, where it has both.
    rendered: bool,
    /// Wheel lines not yet turned into PDF pages.
    wheel: f32,
    /// Only the reader, for an extension's view.
    reader_only: bool,
    /// The body row of the tree under the pointer. SchnellUI reports no
    /// leave, so a move onto the grid's outer cells or the reader clears it.
    hover: Option<usize>,
    /// The tree's node of the file the reader shows, marked in the tree
    /// while the selection moves elsewhere.
    open_node: Option<NodeId>,
}

impl View {
    fn layout(&self) -> Layout {
        let cols = self.cols;
        if self.reader_only {
            return Layout {
                tree: 0,
                reader_x: 0,
                reader: cols,
            };
        }
        if cols < NARROW {
            return match self.focus {
                Side::Tree => Layout {
                    tree: cols,
                    reader_x: cols,
                    reader: 0,
                },
                Side::Reader => Layout {
                    tree: 0,
                    reader_x: 0,
                    reader: cols,
                },
            };
        }
        let tree = (cols * 3 / 10).clamp(20, 48);
        Layout {
            tree,
            reader_x: tree + 1,
            reader: cols - tree - 1,
        }
    }

    /// Rows below the header.
    fn body_rows(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    /// Rows of file text: the body rows minus the horizontal scrollbar.
    fn reader_rows(&self, layout: Layout) -> usize {
        let rows = self.body_rows();
        if rows > 1 && self.side_thumb(layout).is_some() {
            rows - 1
        } else {
            rows
        }
    }

    /// Columns of file text right of the gutter. The last reader column
    /// holds the vertical scrollbar.
    fn text_width(layout: Layout, text: Text<'_>) -> usize {
        layout.reader.saturating_sub(text.gutter() + 1)
    }

    /// The tree's vertical thumb, within the body rows.
    fn tree_thumb(&self, layout: Layout) -> Option<(usize, usize)> {
        if layout.tree == 0 || self.tree.is_none() {
            return None;
        }
        let rows = self.body_rows();
        thumb(self.visible.len(), rows, self.tree_top, rows)
    }

    /// The reader's vertical thumb, within the text rows.
    fn reader_thumb(&self, layout: Layout) -> Option<(usize, usize)> {
        let text = self.text()?;
        let rows = self.reader_rows(layout);
        thumb(text.lines(), rows, self.top, rows)
    }

    /// The reader's horizontal thumb, within the text columns.
    fn side_thumb(&self, layout: Layout) -> Option<(usize, usize)> {
        let text = self.text()?;
        if layout.reader == 0 {
            return None;
        }
        let width = Self::text_width(layout, text);
        thumb(text.width(), width, self.left, width)
    }

    fn opened(&self) -> Option<&Opened> {
        match &self.body {
            Body::File(opened) => Some(opened),
            _ => None,
        }
    }

    /// The rendered form, while the reader shows it.
    fn rendering(&self) -> Option<&Rendered> {
        let opened = self.opened()?;
        if self.rendered || opened.source.is_none() {
            opened.rendered.as_ref()
        } else {
            None
        }
    }

    /// The text the reader shows, unless it shows a picture.
    fn text(&self) -> Option<Text<'_>> {
        match self.rendering() {
            Some(Rendered::Markdown { laid, .. }) => Some(Text::Rendered(laid)),
            Some(_) => None,
            None => self.opened()?.source.as_ref().map(Text::Source),
        }
    }

    /// The picture the reader shows: an image, a drawing or a PDF page.
    fn picture(&self) -> Option<&Picture> {
        match self.rendering()? {
            Rendered::Picture { picture, .. } => Some(picture),
            Rendered::Pdf(pdf) => pdf.picture.as_ref(),
            Rendered::Markdown { .. } => None,
        }
    }

    fn pdf(&self) -> Option<&Pdf> {
        match self.rendering()? {
            Rendered::Pdf(pdf) => Some(pdf),
            _ => None,
        }
    }

    fn pdf_mut(&mut self) -> Option<&mut Pdf> {
        match &mut self.body {
            Body::File(opened) => match &mut opened.rendered {
                Some(Rendered::Pdf(pdf)) => Some(pdf),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether the file has both a source and a rendered form to switch
    /// between.
    fn can_toggle(&self) -> bool {
        self.opened()
            .is_some_and(|opened| opened.source.is_some() && opened.rendered.is_some())
    }

    /// Switches between the rendered form and the source.
    fn toggle(&mut self, tr: &Strings) {
        if !self.can_toggle() {
            return;
        }
        self.rendered = !self.rendered;
        self.top = 0;
        self.left = 0;
        self.selection = None;
        self.relayout();
        self.reword(tr);
    }

    /// Wraps rendered Markdown to the reader's width when that changed.
    /// Allocation-free while it stays the same.
    fn relayout(&mut self) {
        let layout = self.layout();
        if layout.reader == 0 {
            return;
        }
        // One blank column on the left; on the right a blank column, then
        // the scrollbar, so text never touches it.
        let columns = layout.reader.saturating_sub(3);
        if let Body::File(opened) = &mut self.body
            && let Some(Rendered::Markdown { markdown, laid }) = &mut opened.rendered
            && laid.columns != columns
        {
            *laid = markdown.layout(columns);
            self.generation = self.generation.wrapping_add(1);
            self.scroll_reader(0);
            self.scroll_sideways(0);
        }
    }

    /// Where the picture goes on the grid: centered in the reader, as large
    /// as fits, raster pictures no larger than their natural size. The
    /// image covers the whole raster square; the picture is its top left.
    fn placement(
        &self,
        layout: Layout,
        metrics: TerminalCellMetrics,
    ) -> Option<(u64, TerminalImagePlacement)> {
        let picture = self.picture()?;
        if layout.reader < 3 {
            return None;
        }
        let (cell_w, cell_h) = (metrics.width, metrics.height);
        let x = (layout.reader_x + 1) as f32 * cell_w;
        let y = cell_h;
        let width = (layout.reader - 2) as f32 * cell_w;
        let height = self.body_rows() as f32 * cell_h;
        let (natural_w, natural_h) = picture.natural;
        if natural_w <= 0.0 || natural_h <= 0.0 {
            return None;
        }
        let mut scale = (width / natural_w).min(height / natural_h);
        if !picture.scalable {
            scale = scale.min(1.0);
        }
        let (shown_w, shown_h) = (natural_w * scale, natural_h * scale);
        let raster = RASTER as f32;
        Some((
            picture.serial,
            TerminalImagePlacement::Pixels {
                x: (x + (width - shown_w) / 2.0).round(),
                y: (y + (height - shown_h) / 2.0).round(),
                width: raster * shown_w / picture.width as f32,
                height: raster * shown_h / picture.height as f32,
            },
        ))
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
        if layout.tree > 0
            && let Some(tree) = &self.tree
            && let Some(&node) = self.visible.get(self.tree_top + body_row)
        {
            key.node = node + 1;
            key.selected = self.tree_top + body_row == self.selected;
            key.expanded = tree.is_expanded(node);
            key.hovered = self.hover == Some(body_row);
        }
        key.tree_thumb = on_thumb(self.tree_thumb(layout), body_row);
        if layout.reader > 0
            && let Some(text) = self.text()
        {
            let rows = self.reader_rows(layout);
            let line = self.top + body_row;
            if body_row >= rows {
                if let Some((start, len)) = self.side_thumb(layout) {
                    key.side_thumb = (start as u32, len as u32);
                }
            } else if line < text.lines() {
                key.line = line as u32 + 1;
                key.left = self.left as u32;
            }
            key.reader_thumb = body_row < rows && on_thumb(self.reader_thumb(layout), body_row);
            if let Some((from, to)) = self.selected_columns(row, layout) {
                key.selected_columns = (from as u32 + 1, to as u32 + 1);
            }
        }
        key
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.visible.len().saturating_sub(1));
        let rows = self.body_rows();
        if self.selected < self.tree_top {
            self.tree_top = self.selected;
        } else if self.selected >= self.tree_top + rows {
            self.tree_top = self.selected + 1 - rows;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let next = (self.selected as isize + delta).max(0) as usize;
        self.select(next);
    }

    fn selected_node(&self) -> Option<NodeId> {
        self.visible.get(self.selected).copied()
    }

    fn refresh_visible(&mut self) {
        match &self.tree {
            Some(tree) => tree.visible(&mut self.visible),
            None => self.visible.clear(),
        }
        self.select(self.selected);
        let max = self.visible.len().saturating_sub(self.body_rows());
        self.tree_top = self.tree_top.min(max);
    }

    fn set_focus(&mut self, side: Side) {
        if self.focus != side && !(self.reader_only && side == Side::Tree) {
            self.focus = side;
            self.selection = None;
            self.generation = self.generation.wrapping_add(1);
            self.relayout();
        }
    }

    fn scroll_tree(&mut self, lines: isize) {
        let max = self.visible.len().saturating_sub(self.body_rows());
        self.tree_top = (self.tree_top as isize + lines).clamp(0, max as isize) as usize;
    }

    fn scroll_reader(&mut self, lines: isize) {
        let Some(text) = self.text() else {
            return;
        };
        let max = text.lines().saturating_sub(self.reader_rows(self.layout()));
        self.top = (self.top as isize + lines).clamp(0, max as isize) as usize;
        self.selection = None;
    }

    fn scroll_sideways(&mut self, columns: isize) {
        let layout = self.layout();
        let Some(text) = self.text() else {
            return;
        };
        let width = Self::text_width(layout, text);
        let max = text.width().saturating_sub(width.max(1) / 2);
        self.left = (self.left as isize + columns).clamp(0, max as isize) as usize;
        self.selection = None;
    }

    /// Which scrollbar the cell at `row`, `column` belongs to, as a drag
    /// that grabs its thumb at `at` cells along the track. A press on the
    /// track away from the thumb grabs the thumb's middle, so it jumps there.
    fn scrollbar_at(&self, row: usize, column: usize, at: (f32, f32)) -> Option<Drag> {
        let layout = self.layout();
        let body_row = row.checked_sub(1)?;
        let grab = |(start, len): (usize, usize), at: f32| {
            let offset = at - start as f32;
            if (0.0..len as f32).contains(&offset) {
                offset
            } else {
                len as f32 / 2.0
            }
        };
        if layout.tree > 0
            && column == layout.tree - 1
            && let Some(thumb) = self.tree_thumb(layout)
        {
            return Some(Drag::Tree {
                grab: grab(thumb, at.1),
            });
        }
        let text = self.text()?;
        let rows = self.reader_rows(layout);
        if layout.reader > 0
            && column == layout.reader_x + layout.reader - 1
            && body_row < rows
            && let Some(thumb) = self.reader_thumb(layout)
        {
            return Some(Drag::Reader {
                grab: grab(thumb, at.1),
            });
        }
        let origin = layout.reader_x + text.gutter();
        if body_row == rows
            && column >= origin
            && column < origin + Self::text_width(layout, text)
            && let Some(thumb) = self.side_thumb(layout)
        {
            return Some(Drag::Sideways {
                grab: grab(thumb, at.0),
            });
        }
        None
    }

    /// Scrolls so the held thumb sits under the pointer, `at` cells along
    /// the tracks: (columns from the text start, rows from the body top).
    fn drag_to(&mut self, drag: Drag, at: (f32, f32)) {
        let layout = self.layout();
        match drag {
            Drag::Tree { grab } => {
                let rows = self.body_rows();
                if let Some((_, len)) = self.tree_thumb(layout) {
                    let range = self.visible.len().saturating_sub(rows);
                    self.tree_top = dragged(at.1 - grab, rows, len, range);
                }
            }
            Drag::Reader { grab } => {
                let rows = self.reader_rows(layout);
                if let (Some((_, len)), Some(text)) = (self.reader_thumb(layout), self.text()) {
                    let range = text.lines().saturating_sub(rows);
                    self.top = dragged(at.1 - grab, rows, len, range);
                    self.selection = None;
                }
            }
            Drag::Sideways { grab } => {
                if let (Some((_, len)), Some(text)) = (self.side_thumb(layout), self.text()) {
                    let width = Self::text_width(layout, text);
                    let range = text.width().saturating_sub(width);
                    self.left = dragged(at.0 - grab, width, len, range);
                    self.selection = None;
                }
            }
        }
    }

    /// The header texts, worded once per load.
    fn reword(&mut self, tr: &Strings) {
        self.tree_header = match &self.tree {
            Some(tree) => (tr.files_count)(tree.files()),
            None => tr.files.to_owned(),
        };
        let header = match &self.body {
            Body::File(opened) => {
                let mut header = opened.path.display().to_string();
                let mut part = |part: &str| {
                    header.push_str(" · ");
                    header.push_str(part);
                };
                match self.rendering() {
                    Some(Rendered::Markdown { .. }) => part("Markdown"),
                    Some(Rendered::Picture { picture, format }) => {
                        part(format);
                        if !picture.scalable {
                            let (width, height) = picture.natural;
                            part(&(tr.files_pixels)(width as u32, height as u32));
                        }
                    }
                    Some(Rendered::Pdf(pdf)) => {
                        part("PDF");
                        part(&(tr.files_page)(pdf.page + 1, pdf.pages));
                    }
                    None => {
                        if let Some(document) = &opened.source {
                            if let Some(language) = document.language {
                                part(language.name);
                            }
                            part(&(tr.files_lines)(document.lines()));
                            if document.truncated {
                                part(tr.files_truncated);
                            }
                        }
                        if let Some(error) = &opened.error {
                            part(&(tr.files_unrendered)(error));
                        }
                    }
                }
                if self.can_toggle() {
                    part(match self.rendering() {
                        Some(_) => tr.files_show_source,
                        None => tr.files_show_rendered,
                    });
                }
                header
            }
            _ => String::new(),
        };
        self.header = header;
        self.generation = self.generation.wrapping_add(1);
    }

    /// The grid columns of `row` the text selection covers, inclusive. The
    /// selection stays within the reader's text: never the tree, the line
    /// numbers, the scrollbars or rows past the end of the file.
    fn selected_columns(&self, row: usize, layout: Layout) -> Option<(usize, usize)> {
        let (start, end) = self.selection?;
        let (start, end) = if (start.row, start.column) <= (end.row, end.column) {
            (start, end)
        } else {
            (end, start)
        };
        let text = self.text()?;
        let line = (self.top + row).checked_sub(1)?;
        if row == 0
            || row < start.row
            || row > end.row
            || row > self.reader_rows(layout)
            || line >= text.lines()
        {
            return None;
        }
        let first = layout.reader_x + text.gutter();
        let last = (first + Self::text_width(layout, text)).checked_sub(1)?;
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
        let (start, end) = self.selection?;
        let (start, end) = if (start.row, start.column) <= (end.row, end.column) {
            (start, end)
        } else {
            (end, start)
        };
        let shown = self.text()?;
        let origin = layout.reader_x + shown.gutter();
        let column = |grid: usize| grid.saturating_sub(origin) + self.left;
        let rows = self.reader_rows(layout);
        let mut text = String::new();
        for row in start.row.max(1)..=end.row {
            let line = self.top + row - 1;
            if row > rows || line >= shown.lines() {
                break;
            }
            let from = if row == start.row {
                column(start.column)
            } else {
                0
            };
            let to = if row == end.row {
                column(end.column) + 1
            } else {
                usize::MAX
            };
            if !text.is_empty() || row != start.row.max(1) {
                text.push('\n');
            }
            text.push_str(document::columns(shown.line(line), from, to).trim_end());
        }
        Some(text)
    }
}

pub struct FilesPanel {
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
    /// The file the panel shows, relative to the worktree root.
    open: RefCell<Option<PathBuf>>,
    /// Set when `open` changed, until the app saves the layout.
    opened: Cell<bool>,
    shown_title: RefCell<String>,
    /// The picture placed on the grid: its serial and where it sits.
    shown_picture: Cell<Option<(u64, TerminalImagePlacement)>>,
    tr: Cell<&'static Strings>,
    /// The extension view and key the panel shows, instead of files.
    extension_view: Option<(String, String)>,
    /// The title the extension's view rendered.
    view_title: RefCell<String>,
    /// Set when the user asked to render the view again.
    rerender: Cell<bool>,
    /// The pointer's cursor where it last moved over the panel: a pointing
    /// hand over the tree's rows and the reader's mode toggle.
    cursor: Cell<CursorIcon>,
}

impl FilesPanel {
    /// The pointer's cursor over the panel, for the app's cursor provider.
    pub fn cursor(&self) -> CursorIcon {
        self.cursor.get()
    }

    /// A panel for the worktree at `root` on `env`, showing `file` once read.
    /// `jj` reads it with Jujutsu where it has a jj repository; `known` is
    /// the git repository recorded for its project.
    pub fn new(
        env: Environment,
        root: PathBuf,
        jj: bool,
        known: Option<crate::vcs::KnownRepo>,
        file: Option<PathBuf>,
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
            view: RefCell::new(View {
                tree: None,
                tree_message: Some(tr.files_loading.to_owned()),
                visible: Vec::new(),
                selected: 0,
                tree_top: 0,
                body: Body::Empty,
                top: 0,
                left: 0,
                focus: Side::Tree,
                cols: 80,
                rows: 24,
                generation: 0,
                tree_header: tr.files.to_owned(),
                header: String::new(),
                selection: None,
                selecting: false,
                drag: None,
                request: 0,
                rendered: true,
                wheel: 0.0,
                reader_only: false,
                hover: None,
                open_node: None,
            }),
            keys: RefCell::new(Vec::new()),
            view_revision: Cell::new(0),
            cursor: Cell::new(CursorIcon::Default),
            open: RefCell::new(None),
            opened: Cell::new(false),
            shown_title: RefCell::new(String::new()),
            shown_picture: Cell::new(None),
            tr: Cell::new(tr),
            extension_view: None,
            view_title: RefCell::new(String::new()),
            rerender: Cell::new(false),
        });
        panel.load_tree();
        if let Some(file) = file {
            panel.open_file(file);
            panel.opened.set(false);
        }
        panel
    }

    /// A panel for view `view` of an extension with `key`. It shows that it
    /// loads until the app hands it the rendered text with
    /// [`Self::show_markdown`].
    pub fn extension_view(view: String, key: String, wake: Wake, tr: &'static Strings) -> Rc<Self> {
        Rc::new(Self {
            env: Environment::Local,
            root: PathBuf::new(),
            jj: false,
            known: None,
            wake,
            inbox: Arc::default(),
            view: RefCell::new(View {
                tree: None,
                tree_message: None,
                visible: Vec::new(),
                selected: 0,
                tree_top: 0,
                body: Body::Loading,
                top: 0,
                left: 0,
                focus: Side::Reader,
                cols: 80,
                rows: 24,
                generation: 0,
                tree_header: String::new(),
                header: String::new(),
                selection: None,
                selecting: false,
                drag: None,
                request: 0,
                rendered: true,
                wheel: 0.0,
                reader_only: true,
                hover: None,
                open_node: None,
            }),
            keys: RefCell::new(Vec::new()),
            view_revision: Cell::new(0),
            cursor: Cell::new(CursorIcon::Default),
            open: RefCell::new(None),
            opened: Cell::new(false),
            shown_title: RefCell::new(String::new()),
            shown_picture: Cell::new(None),
            tr: Cell::new(tr),
            view_title: RefCell::new(key.clone()),
            extension_view: Some((view, key)),
            rerender: Cell::new(false),
        })
    }

    /// The extension view and key this panel shows, if it shows one.
    pub fn view_source(&self) -> Option<(&str, &str)> {
        self.extension_view
            .as_ref()
            .map(|(view, key)| (view.as_str(), key.as_str()))
    }

    /// Shows the Markdown an extension's view rendered, keeping the scroll
    /// position where it still fits.
    pub fn show_markdown(&self, title: &str, markdown: &str) {
        let mut opened = render::open(
            PathBuf::from("view.md"),
            markdown.as_bytes().to_vec(),
            self.tr.get(),
        );
        opened.path = PathBuf::from(title);
        self.show(title, Body::File(Box::new(opened)));
    }

    /// Shows a message in place of a view, such as for a key that is gone.
    pub fn show_message(&self, title: &str, message: String) {
        self.show(title, Body::Message(message));
    }

    fn show(&self, title: &str, body: Body) {
        {
            let mut shown = self.view_title.borrow_mut();
            shown.clear();
            shown.push_str(title);
        }
        let tr = self.tr.get();
        let mut view = self.view.borrow_mut();
        view.body = body;
        view.selection = None;
        view.relayout();
        view.scroll_reader(0);
        view.scroll_sideways(0);
        view.reword(tr);
        drop(view);
        self.touch();
        (self.wake)();
    }

    /// True once after the user asked to render the view again (F5 or r).
    pub fn take_rerender(&self) -> bool {
        self.rerender.replace(false)
    }

    /// The open file, relative to the worktree root.
    pub fn file(&self) -> Option<PathBuf> {
        self.open.borrow().clone()
    }

    fn title_str<'a>(&self, open: &'a Option<PathBuf>) -> &'a str {
        open.as_deref()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or(self.tr.get().files)
    }

    /// Tab label: the open file's name, else "Files". A view's title.
    pub fn title(&self) -> String {
        if self.extension_view.is_some() {
            return self.view_title.borrow().clone();
        }
        self.title_str(&self.open.borrow()).to_owned()
    }

    /// Whether the title changed since the last call. Allocation-free while
    /// it stays the same.
    pub fn title_changed(&self) -> bool {
        let open = self.open.borrow();
        let view_title = self.view_title.borrow();
        let title = if self.extension_view.is_some() {
            view_title.as_str()
        } else {
            self.title_str(&open)
        };
        let mut shown = self.shown_title.borrow_mut();
        if shown.as_str() == title {
            return false;
        }
        shown.clear();
        shown.push_str(title);
        true
    }

    /// True once after the panel opened another file, so the app saves it.
    pub fn take_opened(&self) -> bool {
        self.opened.replace(false)
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let mut view = self.view.borrow_mut();
        let (cols, rows) = (usize::from(cols), usize::from(rows));
        if (view.cols, view.rows) != (cols, rows) {
            view.cols = cols;
            view.rows = rows;
            view.refresh_visible();
            view.relayout();
            view.scroll_reader(0);
            view.generation = view.generation.wrapping_add(1);
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

    fn load_tree(&self) {
        let env = self.env.clone();
        let root = self.root.clone();
        let jj = self.jj;
        let known = self.known.clone();
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        spawn(move || {
            let result = crate::vcs::open(&env, &root, jj, known.as_ref())
                .and_then(|repo| {
                    let mut tree = Tree::from_listing(repo.files(&root)?);
                    // A tree without its changes is still worth showing.
                    if let Ok(changes) = repo.changes(&root) {
                        tree.mark_changes(&changes);
                    }
                    Ok(tree)
                })
                .map_err(|error| error.to_string());
            inbox.send(Message::Tree(result), &wake);
        });
    }

    /// Reads the worktree's changes again after its status changed, and
    /// marks them on the tree shown, which keeps its folds and scroll.
    pub fn files_changed(&self) {
        if self.extension_view.is_some() {
            return;
        }
        let env = self.env.clone();
        let root = self.root.clone();
        let jj = self.jj;
        let known = self.known.clone();
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        spawn(move || {
            let changes = crate::vcs::open(&env, &root, jj, known.as_ref())
                .and_then(|repo| repo.changes(&root));
            if let Ok(changes) = changes {
                inbox.send(Message::Changes(changes), &wake);
            }
        });
    }

    /// Starts reading `path`, relative to the worktree root.
    fn open_file(&self, path: PathBuf) {
        // A path inside the worktree is kept relative, so the header names
        // the file the way the tree does.
        let path = match path.strip_prefix(&self.root) {
            Ok(inner) if path.is_absolute() && !inner.as_os_str().is_empty() => inner.to_path_buf(),
            _ => path,
        };
        let request = {
            let mut view = self.view.borrow_mut();
            view.request += 1;
            let same = view.opened().is_some_and(|opened| opened.path == path);
            if !same {
                view.body = Body::Loading;
                view.top = 0;
                view.left = 0;
                view.selection = None;
                view.header.clear();
                view.generation = view.generation.wrapping_add(1);
                view.open_node = view.tree.as_ref().and_then(|tree| tree.find(&path));
            }
            view.request
        };
        if self.open.borrow().as_ref() != Some(&path) {
            *self.open.borrow_mut() = Some(path.clone());
            self.opened.set(true);
        }
        let env = self.env.clone();
        let full = self.root.join(&path);
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        let tr = self.tr.get();
        spawn(move || {
            let result = env
                .read_file(&full, render::read_limit(&path))
                .map(|bytes| Box::new(render::open(path, bytes, tr)))
                .map_err(|error| error.to_string());
            inbox.send(Message::File { request, result }, &wake);
        });
        self.touch();
    }

    /// Shows page `page` of the open PDF, clamped to its pages. One page
    /// renders at a time; the newest wanted page renders next.
    fn turn_page(&self, view: &mut View, page: usize) {
        let request = view.request;
        let Some(pdf) = view.pdf_mut() else {
            return;
        };
        let page = page.min(pdf.pages.saturating_sub(1));
        if page == pdf.page {
            return;
        }
        pdf.page = page;
        if pdf.rendering.is_none() {
            pdf.rendering = Some(page);
            self.render_page(pdf.bytes.clone(), page, request);
        }
        view.reword(self.tr.get());
    }

    fn render_page(&self, bytes: Arc<Vec<u8>>, page: usize, request: u64) {
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        spawn(move || {
            let result = render::render_page(&bytes, page);
            inbox.send(
                Message::Page {
                    request,
                    page,
                    result,
                },
                &wake,
            );
        });
    }

    /// Reads the file list and the open file again. A view asks the app to
    /// render it again.
    fn reload(&self) {
        if self.extension_view.is_some() {
            self.rerender.set(true);
            (self.wake)();
            return;
        }
        self.load_tree();
        if let Some(path) = self.file() {
            self.open_file(path);
        }
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
        let tr = self.tr.get();
        for message in messages.drain(..) {
            match message {
                Message::Tree(Ok(tree)) => {
                    // Node ids repeat across loads; their changes may not.
                    view.generation = view.generation.wrapping_add(1);
                    let selected = view
                        .selected_node()
                        .map(|id| view.tree.as_ref().map(|t| t.path(id)));
                    view.tree_message = (tree.len() == 0).then(|| tr.files_empty.to_owned());
                    view.tree = Some(tree);
                    // Keep the selection on reload; reveal the open file at first.
                    let target = selected.flatten().or_else(|| self.file());
                    let found = target.and_then(|path| {
                        let tree = view.tree.as_mut()?;
                        let id = tree.find(&path)?;
                        tree.reveal(id);
                        Some(id)
                    });
                    view.open_node = self.file().and_then(|path| view.tree.as_ref()?.find(&path));
                    view.refresh_visible();
                    if let Some(id) = found
                        && let Some(index) = view.visible.iter().position(|v| *v == id)
                    {
                        view.select(index);
                        let rows = view.body_rows();
                        view.tree_top = index.saturating_sub(rows / 2);
                        view.scroll_tree(0);
                    }
                }
                Message::Changes(changes) => {
                    if let Some(tree) = view.tree.as_mut() {
                        tree.clear_changes();
                        tree.mark_changes(&changes);
                        view.generation = view.generation.wrapping_add(1);
                    }
                }
                Message::Tree(Err(error)) => {
                    view.tree = None;
                    view.tree_message = Some(error);
                    view.refresh_visible();
                }
                Message::File { request, result } if request == view.request => {
                    // A reload stays on the PDF page it showed.
                    let page = view.pdf().map_or(0, |pdf| pdf.page);
                    view.body = match result {
                        Ok(opened) if opened.source.is_none() && opened.rendered.is_none() => {
                            Body::Message(match &opened.error {
                                Some(error) => (tr.files_unrendered)(error),
                                None => (tr.files_binary)(opened.size),
                            })
                        }
                        Ok(opened) => Body::File(opened),
                        Err(error) => Body::Message((tr.files_unreadable)(&error)),
                    };
                    view.wheel = 0.0;
                    view.relayout();
                    // A reload keeps the scroll position where it still fits.
                    view.scroll_reader(0);
                    view.scroll_sideways(0);
                    if page > 0 {
                        self.turn_page(view, page);
                    }
                }
                Message::Page {
                    request,
                    page,
                    result,
                } if request == view.request => {
                    let Some(pdf) = view.pdf_mut() else {
                        continue;
                    };
                    pdf.rendering = None;
                    let error = match result {
                        Ok(picture) if page == pdf.page => {
                            pdf.picture = Some(picture);
                            None
                        }
                        Ok(_) => None,
                        Err(error) => {
                            pdf.picture = None;
                            Some(error)
                        }
                    };
                    if page != pdf.page {
                        let (bytes, wanted) = (pdf.bytes.clone(), pdf.page);
                        pdf.rendering = Some(wanted);
                        self.render_page(bytes, wanted, request);
                    }
                    if let Body::File(opened) = &mut view.body {
                        opened.error = error;
                    }
                }
                Message::File { .. } | Message::Page { .. } => {}
            }
        }
        drop(messages);
        view.reword(tr);
    }

    pub fn view(
        self: &Rc<Self>,
        style: &GridStyle,
        colors: ReaderColors,
        tr: &'static Strings,
        label: &str,
    ) -> TerminalGrid {
        self.tr.set(tr);
        // A new grid holds no image yet.
        self.shown_picture.set(None);
        let revision = self.clone();
        let patch = self.clone();
        let selection = style.selection;
        let metrics = style.metrics;
        let grid = TerminalGrid::patched(
            move || revision.revision(),
            move |grid| patch.draw(grid, &colors, selection, metrics),
        )
        .metrics(style.metrics)
        .label(label);
        match &style.font_family {
            Some(family) => grid.font_family(family.clone()),
            None => grid,
        }
    }

    fn draw(
        &self,
        grid: &mut TerminalGridPatch<'_>,
        colors: &ReaderColors,
        selection: Color,
        metrics: TerminalCellMetrics,
    ) {
        let mut view = self.view.borrow_mut();
        self.receive(&mut view);
        view.relayout();
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
            // The grid's own selection runs row-major across the whole
            // width, so the reader paints its selection into its cells.
            if let Some((from, to)) = view.selected_columns(row, layout) {
                for cell in cells.iter_mut().take(to + 1).skip(from) {
                    cell.background = selection;
                }
            }
        }
        grid.set_cursor(None);
        self.place_picture(grid, view, layout, metrics);
    }

    /// Puts the shown picture on the grid, moves it, or takes it off. Does
    /// nothing, and allocates nothing, while neither picture nor place
    /// changed.
    fn place_picture(
        &self,
        grid: &mut TerminalGridPatch<'_>,
        view: &View,
        layout: Layout,
        metrics: TerminalCellMetrics,
    ) {
        let wanted = view.placement(layout, metrics);
        let shown = self.shown_picture.get();
        if wanted == shown {
            return;
        }
        match (wanted, view.picture()) {
            (Some((serial, placement)), _) if shown.is_some_and(|(s, _)| s == serial) => {
                if let Some(image) = grid.images_mut().first_mut() {
                    image.placement = placement;
                }
            }
            (Some((_, placement)), Some(picture)) => {
                grid.set_images(vec![TerminalImage::new(
                    PICTURE_ID,
                    RASTER,
                    RASTER,
                    picture.rgba.clone(),
                    placement,
                )]);
            }
            _ => grid.set_images(Vec::new()),
        }
        self.shown_picture.set(wanted);
    }

    /// Handles one focused input event.
    pub fn input(
        &self,
        event: FocusedInputEvent,
        metrics: TerminalCellMetrics,
    ) -> FocusedInputResult {
        let result = match event {
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
        if result {
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
            SwitchBack,
            Back,
            Reload,
            Toggle,
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
                "r" => Do::Reload,
                "v" => Do::Toggle,
                _ => return false,
            },
            _ => return false,
        };
        if let Do::Reload = action {
            self.reload();
            return true;
        }
        let mut view = self.view.borrow_mut();
        if let Do::Toggle = action {
            view.toggle(self.tr.get());
            drop(view);
            self.touch();
            return true;
        }
        if view.focus == Side::Reader
            && let Some(pdf) = view.pdf()
        {
            let page = match action {
                Do::Up | Do::PageUp => Some(pdf.page.saturating_sub(1)),
                Do::Down | Do::PageDown => Some(pdf.page + 1),
                Do::Home => Some(0),
                Do::End => Some(usize::MAX),
                _ => None,
            };
            if let Some(page) = page {
                self.turn_page(&mut view, page);
                drop(view);
                self.touch();
                return true;
            }
        }
        let page = match view.focus {
            Side::Tree => view.body_rows(),
            Side::Reader => view.reader_rows(view.layout()),
        };
        let page = page.saturating_sub(1).max(1) as isize;
        let mut open = None;
        match (view.focus, action) {
            (_, Do::Reload | Do::Toggle) => {}
            (Side::Tree, Do::Up) => view.move_selection(-1),
            (Side::Tree, Do::Down) => view.move_selection(1),
            (Side::Tree, Do::PageUp) => view.move_selection(-page),
            (Side::Tree, Do::PageDown) => view.move_selection(page),
            (Side::Tree, Do::Home) => view.select(0),
            (Side::Tree, Do::End) => view.select(usize::MAX),
            (Side::Tree, Do::Left) => self.collapse(&mut view),
            (Side::Tree, Do::Right) => open = self.expand(&mut view),
            (Side::Tree, Do::Enter) => {
                open = self.activate(&mut view);
                if open.is_some() {
                    view.set_focus(Side::Reader);
                }
            }
            // Tab walks tree, reader, then on to the next control, and
            // Shift+Tab walks back, as in the history and review panel.
            (Side::Tree, Do::Switch) if matches!(view.body, Body::Empty) => return false,
            (Side::Tree, Do::Switch) => view.set_focus(Side::Reader),
            (Side::Tree, Do::Back | Do::SwitchBack) => return false,
            (Side::Reader, Do::Up) => view.scroll_reader(-1),
            (Side::Reader, Do::Down) => view.scroll_reader(1),
            (Side::Reader, Do::PageUp) => view.scroll_reader(-page),
            (Side::Reader, Do::PageDown) => view.scroll_reader(page),
            (Side::Reader, Do::Home) => view.scroll_reader(isize::MIN / 2),
            (Side::Reader, Do::End) => view.scroll_reader(isize::MAX / 2),
            (Side::Reader, Do::Left) => view.scroll_sideways(-(SIDE_STEP as isize)),
            (Side::Reader, Do::Right) => view.scroll_sideways(SIDE_STEP as isize),
            (Side::Reader, Do::Enter) => {}
            (Side::Reader, Do::Switch) => return false,
            (Side::Reader, Do::SwitchBack | Do::Back) if view.reader_only => return false,
            (Side::Reader, Do::SwitchBack | Do::Back) => view.set_focus(Side::Tree),
        }
        drop(view);
        if let Some(path) = open {
            self.open_file(path);
        }
        self.touch();
        true
    }

    /// Left in the tree: collapses an open folder, else selects the parent.
    fn collapse(&self, view: &mut View) {
        let Some(node) = view.selected_node() else {
            return;
        };
        let Some(tree) = view.tree.as_mut() else {
            return;
        };
        if tree.is_folder(node) && tree.is_expanded(node) {
            tree.set_expanded(node, false);
            view.refresh_visible();
        } else if let Some(parent) = tree.parent(node)
            && let Some(index) = view.visible.iter().position(|id| *id == parent)
        {
            view.select(index);
        }
    }

    /// Right in the tree: opens a folder, enters an open one, or shows a
    /// file while the tree keeps focus. Returns a file to read.
    fn expand(&self, view: &mut View) -> Option<PathBuf> {
        let node = view.selected_node()?;
        let tree = view.tree.as_mut()?;
        if !tree.is_folder(node) {
            return Some(tree.path(node));
        }
        if tree.is_expanded(node) {
            view.move_selection(1);
        } else {
            tree.set_expanded(node, true);
            view.refresh_visible();
        }
        None
    }

    /// Enter or a click in the tree: toggles a folder, or returns a file to
    /// read.
    fn activate(&self, view: &mut View) -> Option<PathBuf> {
        let node = view.selected_node()?;
        let tree = view.tree.as_mut()?;
        if !tree.is_folder(node) {
            return Some(tree.path(node));
        }
        let expanded = tree.is_expanded(node);
        tree.set_expanded(node, !expanded);
        view.refresh_visible();
        None
    }

    fn pointer(&self, pointer: RawPointerEvent, metrics: TerminalCellMetrics) -> bool {
        let mut view = self.view.borrow_mut();
        let column = (pointer.position.x / metrics.width).max(0.0) as usize;
        let row = (pointer.position.y / metrics.height).max(0.0) as usize;
        let column = column.min(view.cols.saturating_sub(1));
        let row = row.min(view.rows.saturating_sub(1));
        let layout = view.layout();
        let in_tree = column < layout.tree;
        // Fractional cells along the scrollbar tracks: columns from the
        // reader's text start, rows from the top of the body.
        let text_x = view
            .text()
            .map_or(0, |text| layout.reader_x + text.gutter());
        let at = (
            pointer.position.x / metrics.width - text_x as f32,
            pointer.position.y / metrics.height - 1.0,
        );
        let mut open = None;
        if let RawPointerAction::Move = pointer.action {
            let clickable = if row == 0 {
                column >= layout.reader_x && view.can_toggle()
            } else {
                in_tree && row + 1 < view.rows && view.tree_top + row - 1 < view.visible.len()
            };
            self.cursor.set(if clickable {
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
                if !in_tree && let Some(pdf) = view.pdf() {
                    // Wheel lines add up to pages, so a touchpad does not
                    // fly through the file.
                    let page = pdf.page;
                    view.wheel += y;
                    let turns = (view.wheel / WHEEL_LINES).trunc();
                    if turns != 0.0 {
                        view.wheel -= turns * WHEEL_LINES;
                        let page = page.saturating_add_signed(-(turns as isize));
                        self.turn_page(&mut view, page);
                    }
                    drop(view);
                    self.touch();
                    return true;
                }
                let (x, y) = (x.round() as isize, y.round() as isize);
                if in_tree {
                    view.scroll_tree(-y);
                } else {
                    view.scroll_reader(-y);
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
                    if column >= layout.reader_x && view.can_toggle() {
                        view.toggle(self.tr.get());
                        drop(view);
                        self.touch();
                    }
                    return true;
                }
                if let Some(drag) = view.scrollbar_at(row, column, at) {
                    view.set_focus(if in_tree { Side::Tree } else { Side::Reader });
                    view.drag = Some(drag);
                    view.drag_to(drag, at);
                } else if in_tree {
                    let index = view.tree_top + row - 1;
                    if index < view.visible.len() {
                        view.set_focus(Side::Tree);
                        view.select(index);
                        open = self.activate(&mut view);
                    }
                } else if column >= layout.reader_x {
                    view.set_focus(Side::Reader);
                    let point = TerminalGridPoint::new(row, column);
                    view.selection = Some((point, point));
                    view.selecting = true;
                }
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
            RawPointerAction::Leave => {
                if view.hover.take().is_none() {
                    return false;
                }
                drop(view);
                self.touch();
                (self.wake)();
                return false;
            }
            RawPointerAction::Move => {
                // Hover marks a tree row; the grid's outer cells and the
                // reader clear it, and so does the pointer leaving.
                let hover = (in_tree
                    && row > 0
                    && column > 0
                    && row + 1 < view.rows
                    && view.tree_top + row - 1 < view.visible.len())
                .then(|| row - 1);
                if hover == view.hover {
                    return false;
                }
                view.hover = hover;
                drop(view);
                self.touch();
                (self.wake)();
                // Ignored, so the pointer's other work (drags, hover of
                // other widgets) goes on.
                return false;
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
        if let Some(path) = open {
            self.open_file(path);
        }
        self.touch();
        true
    }
}

fn spawn(job: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name("cmdterm-files".to_owned())
        .spawn(job);
}

/// Start and length of a scrollbar thumb on a track of `track` cells, for
/// `visible` of `total` items scrolled to `offset`. None while all fit.
pub(crate) fn thumb(
    total: usize,
    visible: usize,
    offset: usize,
    track: usize,
) -> Option<(usize, usize)> {
    if total <= visible || track == 0 {
        return None;
    }
    let len = (track * visible / total).clamp(1, track);
    let range = total - visible;
    let start = (track - len) * offset.min(range) / range;
    Some((start, len))
}

/// The scroll offset that puts a thumb of `len` cells at `start` on a track
/// of `track` cells, for `range` scrollable items.
pub(crate) fn dragged(start: f32, track: usize, len: usize, range: usize) -> usize {
    let free = track.saturating_sub(len).max(1) as f32;
    let fraction = (start / free).clamp(0.0, 1.0);
    (fraction * range as f32).round() as usize
}

pub(crate) fn on_thumb(thumb: Option<(usize, usize)>, cell: usize) -> bool {
    thumb.is_some_and(|(start, len)| (start..start + len).contains(&cell))
}

pub(crate) fn digits(mut n: usize) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

fn draw_row(
    cells: &mut [TerminalCell],
    row: usize,
    view: &View,
    layout: Layout,
    colors: &ReaderColors,
    tr: &Strings,
) {
    let header = row == 0;
    let page = if header { colors.header } else { colors.page };
    for cell in cells.iter_mut() {
        cell.grapheme.clear();
        cell.foreground = colors.ink;
        cell.background = page;
        cell.attrs = TerminalCellAttrs::default();
        cell.width = TerminalCellWidth::Single;
        cell.hyperlink = None;
    }
    if layout.tree > 0 && layout.reader > 0 && layout.tree < cells.len() {
        let divider = &mut cells[layout.tree];
        divider.grapheme.push('│');
        divider.foreground = colors.line;
    }
    let (tree, reader) = cells.split_at_mut(layout.reader_x.min(cells.len()));
    let tree_width = layout.tree.min(tree.len());
    let tree = &mut tree[..tree_width];
    // A focused reader carries an accent edge down its left side, as the
    // tree's chosen row carries a bar while the tree has focus.
    if view.focus == Side::Reader
        && layout.tree > 0
        && !matches!(view.body, Body::Empty)
        && let Some(edge) = reader.first_mut()
    {
        edge.grapheme.push('▎');
        edge.foreground = colors.accent;
    }
    // With no file picked the reader has no header, so its part of the band
    // stays the page color and the band belongs to the tree alone.
    if header && matches!(view.body, Body::Empty) {
        for cell in reader.iter_mut() {
            cell.background = colors.page;
        }
    }
    if header {
        let bold = TerminalCellAttrs {
            bold: true,
            ..TerminalCellAttrs::default()
        };
        put(tree, 1, &view.tree_header, colors.muted, bold);
        // The path in bold ink, what follows it muted, so the header reads
        // apart from the file's first line under it.
        let (path, rest) = view
            .header
            .split_once(" · ")
            .unwrap_or((view.header.as_str(), ""));
        let end = reader.len().saturating_sub(1);
        let x = put_until(reader, 1, end, path, colors.ink, bold);
        if !rest.is_empty() {
            let plain = TerminalCellAttrs::default();
            let x = put_until(reader, x, end, " · ", colors.muted, plain);
            put_until(reader, x, end, rest, colors.muted, plain);
        }
        return;
    }
    let body_row = row - 1;
    let plain = TerminalCellAttrs::default();
    if !tree.is_empty() {
        let index = view.tree_top + body_row;
        match (&view.tree, view.visible.get(index)) {
            (Some(nodes), Some(&node)) => {
                draw_node(
                    tree,
                    nodes,
                    node,
                    NodeMarks {
                        selected: index == view.selected,
                        hovered: view.hover == Some(body_row),
                        open: view.open_node == Some(node),
                    },
                    view.focus,
                    colors,
                );
            }
            _ if body_row == 0 => {
                if let Some(message) = &view.tree_message {
                    put(tree, 1, message, colors.muted, plain);
                }
            }
            _ => {}
        }
        if on_thumb(view.tree_thumb(layout), body_row)
            && let Some(cell) = tree.last_mut()
        {
            draw_thumb(cell, '┃', colors);
        }
    }
    if reader.is_empty() {
        return;
    }
    if let Some(shown) = view.text() {
        let rows = view.reader_rows(layout);
        let (text, bar) = reader.split_at_mut(reader.len() - 1);
        if body_row >= rows {
            if let Some((start, len)) = view.side_thumb(layout) {
                // A faint track under the thumb, so a short thumb reads as
                // a scrollbar and not as a stray mark.
                for cell in text.iter_mut().skip(shown.gutter()) {
                    draw_thumb(cell, '─', colors);
                    cell.foreground = colors.line;
                }
                let from = shown.gutter() + start;
                for cell in text.iter_mut().skip(from).take(len) {
                    draw_thumb(cell, '━', colors);
                }
            }
            return;
        }
        let line = view.top + body_row;
        if line < shown.lines() {
            match shown {
                Text::Source(document) => draw_line(text, document, line, view.left, colors),
                Text::Rendered(laid) => draw_laid(text, laid, line, view.left, colors),
            }
        }
        if on_thumb(view.reader_thumb(layout), body_row) {
            draw_thumb(&mut bar[0], '┃', colors);
        }
        return;
    }
    let message = match &view.body {
        // The picture lies on the grid as an image above blank rows.
        Body::File(_) if view.picture().is_some() => return,
        Body::File(opened) => opened.error.as_deref().unwrap_or(tr.files_loading),
        Body::Empty => tr.files_pick,
        Body::Loading => tr.files_loading,
        Body::Message(text) => text.as_str(),
    };
    // With nothing picked, the hint sits in the middle of the empty side;
    // other messages report on the file and stay at the top.
    if matches!(view.body, Body::Empty) {
        let width = message.chars().count();
        if body_row == view.body_rows() / 2 && width + 2 <= reader.len() {
            put(
                reader,
                (reader.len() - width) / 2,
                message,
                colors.muted,
                plain,
            );
            return;
        }
        if width + 2 <= reader.len() {
            return;
        }
    }
    if body_row == 0 {
        put(reader, 1, message, colors.muted, plain);
    }
}

pub(crate) fn draw_thumb(cell: &mut TerminalCell, glyph: char, colors: &ReaderColors) {
    cell.grapheme.clear();
    cell.grapheme.push(glyph);
    cell.foreground = colors.muted;
    cell.width = TerminalCellWidth::Single;
}

/// What sets a tree row apart from the rest.
#[derive(Clone, Copy)]
struct NodeMarks {
    selected: bool,
    hovered: bool,
    /// The reader shows this file.
    open: bool,
}

fn draw_node(
    cells: &mut [TerminalCell],
    tree: &Tree,
    node: NodeId,
    marks: NodeMarks,
    focus: Side,
    colors: &ReaderColors,
) {
    let NodeMarks {
        selected,
        hovered,
        open,
    } = marks;
    if hovered && !selected {
        let wash = hover_wash(colors);
        for cell in cells.iter_mut() {
            cell.background = wash;
        }
    }
    if selected {
        let background = match focus {
            Side::Tree => colors.selected,
            Side::Reader => colors.unfocused,
        };
        for cell in cells.iter_mut() {
            cell.background = background;
        }
        // The focused side's choice carries the rail's accent bar.
        if focus == Side::Tree
            && let Some(cell) = cells.first_mut()
        {
            cell.grapheme.push('┃');
            cell.foreground = colors.accent;
        }
    }
    // The open file keeps a quieter bar of its own, so the tree still
    // shows which file the reader holds while the selection moves on.
    if open
        && !(selected && focus == Side::Tree)
        && let Some(cell) = cells.first_mut()
    {
        cell.grapheme.push('▎');
        cell.foreground = colors.ink;
    }
    let indent = 1 + tree.depth(node) * 2;
    let folder = tree.is_folder(node);
    if folder && indent < cells.len() {
        let marker = if tree.is_expanded(node) { '▾' } else { '▸' };
        let cell = &mut cells[indent];
        cell.grapheme.push(marker);
        cell.foreground = colors.muted;
    }
    let change = tree.change(node);
    let (color, attrs) = if folder {
        (
            // On the chosen row the name takes the ink, which keeps its
            // contrast on the tint.
            if selected { colors.ink } else { colors.folder },
            TerminalCellAttrs {
                bold: true,
                ..TerminalCellAttrs::default()
            },
        )
    } else {
        let color = match change {
            Some(letter) if !selected => change_color(char::from(letter), colors),
            _ => colors.ink,
        };
        // The open file's name is bold, beside its bar.
        (
            color,
            TerminalCellAttrs {
                bold: open,
                ..TerminalCellAttrs::default()
            },
        )
    };
    // The status letter sits right, one blank cell before the scrollbar's
    // column, as the Review list puts it before each file.
    let mut end = cells.len().saturating_sub(1);
    if let Some(letter) = change
        && cells.len() > indent + 8
    {
        let at = cells.len() - 3;
        let fg = change_color(char::from(letter), colors);
        let bold = TerminalCellAttrs {
            bold: true,
            ..TerminalCellAttrs::default()
        };
        write_cell(cells, at, char::from(letter), 1, fg, bold);
        end = at - 1;
    }
    let mut column = indent + 2;
    for chunk in tree.name(node).utf8_chunks() {
        column = put_until(cells, column, end, chunk.valid(), color, attrs);
        if !chunk.invalid().is_empty() {
            column = put_until(cells, column, end, "\u{fffd}", color, attrs);
        }
    }
}

/// The color of a VCS status letter, in the file tree and the Review list:
/// new files green, deleted and conflicted ones red, renames blue,
/// modified ones brass.
pub(crate) fn change_color(status: char, colors: &ReaderColors) -> Color {
    match status {
        'A' | '?' => colors.added,
        'D' | 'U' => colors.removed,
        'R' | 'C' => colors.hunk,
        _ => colors.syntax[Token::Number as usize],
    }
}

/// The tree row under the pointer: a small step of ink over the page,
/// lighter than the chosen row's tint.
fn hover_wash(colors: &ReaderColors) -> Color {
    let channel = |page: u8, ink: u8| {
        ((u16::from(page) * (256 - HOVER_SHARE) + u16::from(ink) * HOVER_SHARE) / 256) as u8
    };
    Color::rgb(
        channel(colors.page.r, colors.ink.r),
        channel(colors.page.g, colors.ink.g),
        channel(colors.page.b, colors.ink.b),
    )
}

/// Writes `text` from `column`, ending in `…` when it does not fit.
pub(crate) fn put(
    cells: &mut [TerminalCell],
    column: usize,
    text: &str,
    fg: Color,
    attrs: TerminalCellAttrs,
) {
    let end = cells.len().saturating_sub(1);
    put_until(cells, column, end, text, fg, attrs);
}

/// [`put`] up to column `end`. Returns the column after the text.
pub(crate) fn put_until(
    cells: &mut [TerminalCell],
    mut column: usize,
    end: usize,
    text: &str,
    fg: Color,
    attrs: TerminalCellAttrs,
) -> usize {
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if column + width > end {
            if end > 0 && column <= end {
                let cell = &mut cells[end - 1];
                cell.grapheme.clear();
                cell.grapheme.push('…');
                cell.foreground = fg;
                cell.width = TerminalCellWidth::Single;
            }
            return end;
        }
        write_cell(cells, column, ch, width, fg, attrs);
        column += width;
    }
    column
}

pub(crate) fn write_cell(
    cells: &mut [TerminalCell],
    column: usize,
    ch: char,
    width: usize,
    fg: Color,
    attrs: TerminalCellAttrs,
) {
    let wide = width == 2 && column + 1 < cells.len();
    let cell = &mut cells[column];
    cell.grapheme.clear();
    cell.grapheme.push(ch);
    cell.foreground = fg;
    cell.attrs = attrs;
    if wide {
        cell.width = TerminalCellWidth::Wide;
        let background = cell.background;
        let next = &mut cells[column + 1];
        next.width = TerminalCellWidth::Continuation;
        next.background = background;
    }
}

/// Draws line `line` of `document`, scrolled `left` columns, into `cells`:
/// the line number, then the highlighted text.
fn draw_line(
    cells: &mut [TerminalCell],
    document: &Document,
    line: usize,
    left: usize,
    colors: &ReaderColors,
) {
    let gutter = Text::Source(document).gutter();
    if cells.len() <= gutter {
        return;
    }
    // Right-aligned number, one blank column on either side.
    let mut n = line + 1;
    let mut at = gutter - 2;
    loop {
        let cell = &mut cells[at];
        cell.grapheme.push(char::from(b'0' + (n % 10) as u8));
        cell.foreground = colors.muted;
        n /= 10;
        if n == 0 || at == 0 {
            break;
        }
        at -= 1;
    }
    let (_, text_cells) = cells.split_at_mut(gutter);
    let text = document.line(line);
    let mut column = 0usize;
    let mut paint = |part: &str, token: Token| {
        let attrs = TerminalCellAttrs {
            italic: token == Token::Comment,
            bold: token == Token::Heading,
            ..TerminalCellAttrs::default()
        };
        paint_part(
            text_cells,
            &mut column,
            left,
            part,
            colors.syntax[token as usize],
            attrs,
        );
    };
    match document.language {
        Some(language) if text.len() <= MAX_HIGHLIGHT => {
            let mut state = document.state(line);
            highlight::highlight(language, &mut state, text, |start, end, token| {
                paint(&text[start..end], token);
            });
        }
        _ => paint(text, Token::Plain),
    }
}

/// Writes `part` from display column `column` of a line scrolled `left`
/// columns, and advances `column`. Returns false once past the right edge.
fn paint_part(
    cells: &mut [TerminalCell],
    column: &mut usize,
    left: usize,
    part: &str,
    fg: Color,
    attrs: TerminalCellAttrs,
) -> bool {
    let width = cells.len();
    for ch in part.chars() {
        if *column >= left + width {
            return false;
        }
        let wide = ch.width().unwrap_or(0);
        if wide == 0 {
            continue;
        }
        if *column >= left && *column + wide <= left + width {
            write_cell(cells, *column - left, ch, wide, fg, attrs);
        }
        *column += wide;
    }
    *column < left + width
}

/// Draws line `line` of rendered Markdown, scrolled `left` columns, after
/// one blank column.
pub(crate) fn draw_laid(
    cells: &mut [TerminalCell],
    laid: &Laid,
    line: usize,
    left: usize,
    colors: &ReaderColors,
) {
    let Some((_, text_cells)) = cells.split_first_mut() else {
        return;
    };
    if laid.shaded(line) {
        for cell in text_cells.iter_mut() {
            cell.background = colors.header;
        }
    }
    let mut column = 0usize;
    for (part, style) in laid.runs(line) {
        let fg = match style.ink {
            Ink::Plain => colors.ink,
            Ink::Heading => colors.syntax[Token::Heading as usize],
            Ink::Code => colors.syntax[Token::String as usize],
            Ink::Link => colors.syntax[Token::Function as usize],
            Ink::Muted => colors.muted,
            Ink::Token(token) => colors.syntax[token as usize],
        };
        if !paint_part(text_cells, &mut column, left, part, fg, style.attrs) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use document::Loaded;

    fn source(document: Document) -> Body {
        Body::File(Box::new(Opened {
            path: PathBuf::from("a.txt"),
            source: Some(document),
            rendered: None,
            error: None,
            size: 0,
        }))
    }

    #[test]
    fn lines_draw_numbers_and_token_colors() {
        let colors = crate::ui::resolve_scheme("spruce", true).reader_colors();
        let Loaded::Text(document) = Loaded::new(
            Path::new("lib.rs"),
            b"pub fn answer() -> u32 {\n    42 // the answer\n}\n".to_vec(),
        ) else {
            panic!("binary");
        };
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 40];
        draw_line(&mut cells, &document, 1, 0, &colors);
        let text: String = cells
            .iter()
            .map(|cell| match cell.grapheme.as_str() {
                "" => " ",
                grapheme => grapheme,
            })
            .collect();
        assert_eq!(text.trim_end(), " 2     42 // the answer");
        let syntax = |token: Token| colors.syntax[token as usize];
        assert_eq!(cells[1].foreground, colors.muted);
        assert_eq!(cells[7].foreground, syntax(Token::Number));
        assert_eq!(cells[10].foreground, syntax(Token::Comment));
        assert!(cells[10].attrs.italic);

        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 40];
        draw_line(&mut cells, &document, 0, 4, &colors);
        let text: String = cells.iter().map(|cell| cell.grapheme.as_str()).collect();
        assert!(text.starts_with("1fn answer()"), "{text}");
        assert_eq!(cells[3].foreground, syntax(Token::Keyword));
        assert_eq!(cells[6].foreground, syntax(Token::Function));
        let mut cells = vec![TerminalCell::blank(colors.ink, colors.page); 40];
        draw_line(&mut cells, &document, 0, 0, &colors);
        assert_eq!(cells[3].foreground, syntax(Token::Keyword));
        assert_eq!(cells[22].foreground, syntax(Token::Type));
    }

    #[test]
    fn thumbs_follow_the_scroll_position() {
        assert_eq!(thumb(10, 10, 0, 10), None);
        assert_eq!(thumb(5, 10, 0, 10), None);
        assert_eq!(thumb(100, 10, 0, 10), Some((0, 1)));
        assert_eq!(thumb(100, 10, 90, 10), Some((9, 1)));
        assert_eq!(thumb(20, 10, 0, 10), Some((0, 5)));
        assert_eq!(thumb(20, 10, 10, 10), Some((5, 5)));
        assert_eq!(thumb(20, 10, 5, 10), Some((2, 5)));
        // The reader may scroll past the widest line; the thumb stays at the end.
        assert_eq!(thumb(20, 10, 15, 10), Some((5, 5)));
    }

    #[test]
    fn dragging_a_thumb_maps_back_to_the_offset() {
        assert_eq!(dragged(0.0, 10, 5, 10), 0);
        assert_eq!(dragged(5.0, 10, 5, 10), 10);
        assert_eq!(dragged(2.5, 10, 5, 10), 5);
        assert_eq!(dragged(-3.0, 10, 5, 10), 0);
        assert_eq!(dragged(40.0, 10, 5, 10), 10);
        for offset in 0..=90 {
            let (start, len) = thumb(100, 10, offset, 10).unwrap();
            let back = dragged(start as f32, 10, len, 90);
            assert!(back.abs_diff(offset) <= 10, "{offset} -> {back}");
        }
    }

    #[test]
    fn pressing_and_dragging_the_reader_scrollbars_scrolls() {
        let mut text = Vec::new();
        for n in 0..200 {
            text.extend_from_slice(format!("line {n} {}\n", "x".repeat(n)).as_bytes());
        }
        let Loaded::Text(document) = Loaded::new(Path::new("a.txt"), text) else {
            panic!("binary");
        };
        let mut view = View {
            tree: None,
            tree_message: None,
            visible: Vec::new(),
            selected: 0,
            tree_top: 0,
            body: source(document),
            top: 0,
            left: 0,
            focus: Side::Reader,
            cols: 100,
            rows: 22,
            generation: 0,
            tree_header: String::new(),
            header: String::new(),
            selection: None,
            selecting: false,
            drag: None,
            request: 0,
            rendered: true,
            wheel: 0.0,
            reader_only: false,
            hover: None,
            open_node: None,
        };
        let layout = view.layout();
        let rows = view.reader_rows(layout);
        assert_eq!(rows, 20, "one row goes to the horizontal scrollbar");
        let bar = layout.reader_x + layout.reader - 1;
        // A press at the bottom of the vertical track jumps to the end.
        let drag = view
            .scrollbar_at(rows, bar, (0.0, rows as f32 - 0.5))
            .unwrap();
        view.drag_to(drag, (0.0, rows as f32 - 0.5));
        assert_eq!(view.top, 200 - rows);
        // Dragging the held thumb back up scrolls to the top.
        view.drag_to(drag, (0.0, -5.0));
        assert_eq!(view.top, 0);
        // The horizontal bar sits on the row below the text.
        let origin = layout.reader_x + view.text().unwrap().gutter();
        let drag = view
            .scrollbar_at(rows + 1, origin + 2, (2.0, rows as f32))
            .unwrap();
        assert!(matches!(drag, Drag::Sideways { .. }));
        view.drag_to(drag, (1000.0, rows as f32));
        let width = View::text_width(layout, view.text().unwrap());
        assert_eq!(view.left, view.text().unwrap().width() - width);
        // Text cells are not scrollbars.
        assert_eq!(view.scrollbar_at(3, origin + 2, (2.0, 2.0)), None);
    }

    #[test]
    fn the_selection_stays_in_the_reader_text() {
        let Loaded::Text(document) =
            Loaded::new(Path::new("a.txt"), b"one\ntwo\nthree\nfour\n".to_vec())
        else {
            panic!("binary");
        };
        let mut view = View {
            tree: None,
            tree_message: None,
            visible: Vec::new(),
            selected: 0,
            tree_top: 0,
            body: source(document),
            top: 0,
            left: 0,
            focus: Side::Reader,
            cols: 100,
            rows: 22,
            generation: 0,
            tree_header: String::new(),
            header: String::new(),
            selection: None,
            selecting: false,
            drag: None,
            request: 0,
            rendered: true,
            wheel: 0.0,
            reader_only: false,
            hover: None,
            open_node: None,
        };
        let layout = view.layout();
        let first = layout.reader_x + view.text().unwrap().gutter();
        let last = layout.reader_x + layout.reader - 2;
        // From the middle of line 1 to the middle of line 3.
        view.selection = Some((
            TerminalGridPoint::new(1, first + 1),
            TerminalGridPoint::new(3, first + 2),
        ));
        assert_eq!(view.selected_columns(0, layout), None);
        assert_eq!(view.selected_columns(1, layout), Some((first + 1, last)));
        // Middle rows start at the text, not at the tree or line numbers.
        assert_eq!(view.selected_columns(2, layout), Some((first, last)));
        assert_eq!(view.selected_columns(3, layout), Some((first, first + 2)));
        assert_eq!(view.selected_columns(4, layout), None);
        // Dragged up and past the end of the file, in either direction.
        view.selection = Some((
            TerminalGridPoint::new(10, layout.reader_x),
            TerminalGridPoint::new(2, 0),
        ));
        assert_eq!(view.selected_columns(2, layout), Some((first, last)));
        assert_eq!(view.selected_columns(4, layout), Some((first, last)));
        assert_eq!(view.selected_columns(5, layout), None);
        assert_eq!(
            view.selection_text(layout).as_deref(),
            Some("two\nthree\nfour")
        );
    }

    fn view_of(body: Body) -> View {
        View {
            tree: None,
            tree_message: None,
            visible: Vec::new(),
            selected: 0,
            tree_top: 0,
            body,
            top: 0,
            left: 0,
            focus: Side::Reader,
            cols: 100,
            rows: 22,
            generation: 0,
            tree_header: String::new(),
            header: String::new(),
            selection: None,
            selecting: false,
            drag: None,
            request: 0,
            rendered: true,
            wheel: 0.0,
            reader_only: false,
            hover: None,
            open_node: None,
        }
    }

    #[test]
    fn markdown_opens_rendered_and_v_shows_its_source() {
        let opened = render::open(
            PathBuf::from("a.md"),
            b"# Title\n\nSome words.\n".to_vec(),
            &crate::i18n::EN,
        );
        let mut view = view_of(Body::File(Box::new(opened)));
        view.relayout();
        assert!(view.can_toggle());
        let Some(Text::Rendered(laid)) = view.text() else {
            panic!("not rendered");
        };
        assert_eq!(laid.columns, view.layout().reader - 3);
        assert_eq!(laid.line(0), "Title");
        view.toggle(&crate::i18n::EN);
        let Some(Text::Source(document)) = view.text() else {
            panic!("not the source");
        };
        assert_eq!(document.line(0), "# Title");
        assert!(
            view.header.ends_with("press v for the rendered view"),
            "{}",
            view.header
        );
    }

    #[test]
    fn pictures_center_and_keep_their_size() {
        let mut png = Vec::new();
        image::RgbaImage::new(40, 20)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let opened = render::open(PathBuf::from("a.png"), png, &crate::i18n::EN);
        let view = view_of(Body::File(Box::new(opened)));
        assert!(view.text().is_none() && !view.can_toggle());
        let metrics = TerminalCellMetrics {
            width: 10.0,
            height: 20.0,
            font_size: 14.0,
        };
        let layout = view.layout();
        let Some((
            _,
            TerminalImagePlacement::Pixels {
                x,
                y,
                width,
                height,
            },
        )) = view.placement(layout, metrics)
        else {
            panic!("no placement");
        };
        // The raster square scales by one: the picture shows at 40 × 20.
        assert_eq!((width, height), (RASTER as f32, RASTER as f32));
        let left = (layout.reader_x + 1) as f32 * 10.0;
        let reader = (layout.reader - 2) as f32 * 10.0;
        assert_eq!(x, (left + (reader - 40.0) / 2.0).round());
        assert_eq!(y, 220.0);
    }

    #[test]
    fn digits_counts_decimal_places() {
        assert_eq!(digits(0), 1);
        assert_eq!(digits(9), 1);
        assert_eq!(digits(10), 2);
        assert_eq!(digits(12345), 5);
    }
}
