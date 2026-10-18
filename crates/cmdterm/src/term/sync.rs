//! Copies changed rows of the cmdterm-vt model into SchnellUI's retained
//! terminal grid.
//!
//! Each visible row remembers which stable line it last showed and that
//! line's sequence number. A row is rewritten only when either changed, so an
//! echoed keystroke touches one row and an idle terminal touches none. When
//! output scrolls, a line that another row already shows is copied from that
//! row instead of being converted again.

use cmdterm_vt::{Cell, CursorShape, Palette, Rgb, Row, Style, StyleId, Terminal, Width, flags};
use schnellui::scene::Color;
use schnellui::widgets::{
    TerminalCell, TerminalCellAttrs, TerminalCellWidth, TerminalCursor, TerminalCursorStyle,
    TerminalGridPatch, TerminalGridPoint, TerminalSelection,
};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct RowKey {
    stable: u64,
    seqno: u64,
}

/// Most hyperlink targets kept by id. A program that writes a new link on
/// every line would otherwise grow the cache without end.
const LINK_CACHE_MAX: usize = 1024;

/// One shared `Arc<str>` per hyperlink id, so a run of cells with the same
/// OSC 8 target shares one allocation across rows and frames.
#[derive(Default)]
struct Links(HashMap<u32, Arc<str>>);

impl Links {
    /// The shared target for `id`. The text is compared, so an id the
    /// terminal reused for another target never returns a stale one.
    fn get(&mut self, id: u32, uri: &str) -> Arc<str> {
        if let Some(known) = self.0.get(&id)
            && **known == *uri
        {
            return known.clone();
        }
        if self.0.len() >= LINK_CACHE_MAX {
            self.0.clear();
        }
        let shared: Arc<str> = uri.into();
        self.0.insert(id, shared.clone());
        shared
    }
}

/// Per-terminal sync state, owned by the UI side.
#[derive(Default)]
pub struct GridSync {
    rows: Vec<Option<RowKey>>,
    /// `rows` as the last sync left them: where each line was shown.
    previous: Vec<Option<RowKey>>,
    /// The line each row shows after this sync. Scratch for [`Self::sync`].
    wanted: Vec<RowKey>,
    /// Lines scrolled back from the live bottom. Zero follows output.
    scroll_offset: usize,
    /// The rows shown while the terminal is frozen, instead of the model's.
    frozen: Option<Frozen>,
    links: Links,
    /// The 256 palette colors the rows were converted with.
    palette: Vec<Rgb>,
    /// The grid lacks keyboard focus, so a block cursor shows as its
    /// outline, as terminals mark a pane that keys do not reach.
    unfocused: bool,
}

/// The rows a terminal showed when it froze, already converted, so later
/// output cannot change them.
struct Frozen {
    lines: Vec<Vec<TerminalCell>>,
    foreground: Color,
    background: Color,
    /// Whether the grid shows them already.
    drawn: bool,
}

/// What a double-click groups into one word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CharClass {
    Space,
    Word,
    /// Brackets, quotes and separators: selected one at a time.
    Other,
}

/// Characters that end a word. Paths, URLs, `file.rs:12:3` and `KEY=value`
/// stay one word; brackets, quotes and list separators split them.
const WORD_BREAKS: &str = "{}[]()<>\"'`|,;\u{2502}";

fn char_class(text: &str) -> CharClass {
    match text.chars().next() {
        None => CharClass::Space,
        Some(c) if c.is_whitespace() => CharClass::Space,
        Some(c) if WORD_BREAKS.contains(c) => CharClass::Other,
        Some(_) => CharClass::Word,
    }
}

/// The stable index of the first visible line.
fn top_stable(term: &Terminal, scroll_offset: usize) -> u64 {
    term.stable_index(0) - scroll_offset.min(term.scrollback_len()) as u64
}

impl GridSync {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Scrolls the viewport into scrollback; positive values go back in time.
    /// Returns whether the offset changed. The caller bumps its revision so the
    /// grid is patched on the next frame.
    pub fn scroll_by(&mut self, term: &Terminal, lines: isize) -> bool {
        let max = term.scrollback_len();
        let next = (self.scroll_offset as isize + lines).clamp(0, max as isize) as usize;
        let changed = next != self.scroll_offset;
        self.scroll_offset = next;
        changed
    }

    pub fn scroll_to_bottom(&mut self) -> bool {
        std::mem::replace(&mut self.scroll_offset, 0) != 0
    }

    /// Text of the inclusive viewport selection from `start` to `end`
    /// (row, column pairs in either order), one line per row.
    pub fn selection_text(
        &self,
        term: &Terminal,
        start: (usize, usize),
        end: (usize, usize),
    ) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let size = term.size();
        let mut text = String::new();
        let mut line = String::new();
        // Whether the row before wrapped into this one: a long line the
        // terminal broke at its edge copies as one line.
        let mut joined = false;
        for row in start.0..=end.0.min(size.rows.saturating_sub(1)) {
            let Some(source) = self.viewport_row(term, row) else {
                break;
            };
            let wraps = source.wrapped() && row != end.0;
            let first = if row == start.0 { start.1 } else { 0 };
            let last = if row == end.0 {
                end.1 + 1
            } else if wraps {
                source.text_cols(size.cols)
            } else {
                size.cols
            };
            line.clear();
            for column in first..last.max(first).min(size.cols) {
                let cell = source.get(column);
                if cell.width != Width::Continuation {
                    line.push_str(term.cell_text(&cell));
                }
            }
            if row != start.0 && !joined {
                text.push('\n');
            }
            if wraps {
                text.push_str(&line);
            } else {
                text.push_str(line.trim_end());
            }
            joined = wraps;
        }
        text
    }

    /// The model row shown at viewport `row`.
    fn viewport_row<'a>(&self, term: &'a Terminal, row: usize) -> Option<&'a Row> {
        if row >= term.size().rows {
            return None;
        }
        term.row_by_stable(top_stable(term, self.scroll_offset) + row as u64)
    }

    /// The columns of viewport `row` that hold text: all of them, or fewer
    /// on a row whose last wide character moved to the next row.
    fn text_end(&self, term: &Terminal, row: usize) -> usize {
        let cols = term.size().cols;
        match self.viewport_row(term, row) {
            Some(source) if source.wrapped() => source.text_cols(cols).clamp(1, cols),
            _ => cols,
        }
    }

    fn wraps(&self, term: &Terminal, row: usize) -> bool {
        self.viewport_row(term, row).is_some_and(Row::wrapped)
    }

    fn class_at(&self, term: &Terminal, row: usize, column: usize) -> CharClass {
        let Some(source) = self.viewport_row(term, row) else {
            return CharClass::Space;
        };
        let mut column = column;
        // The right half of a wide character belongs to its left half.
        while column > 0 && source.get(column).width == Width::Continuation {
            column -= 1;
        }
        char_class(term.cell_text(&source.get(column)))
    }

    /// The viewport cells a double-click at (`row`, `column`) selects: the
    /// run of word characters or blanks around it, or the one punctuation
    /// character under it. Runs continue across rows the terminal wrapped.
    /// Inclusive (row, column) corners. Does not allocate.
    pub fn word_bounds(
        &self,
        term: &Terminal,
        row: usize,
        column: usize,
    ) -> ((usize, usize), (usize, usize)) {
        let size = term.size();
        if size.rows == 0 || size.cols == 0 {
            return ((row, column), (row, column));
        }
        let row = row.min(size.rows - 1);
        let column = column.min(size.cols - 1);
        let class = self.class_at(term, row, column);
        let mut start = (row, column);
        let mut end = (row, column);
        if class != CharClass::Other {
            loop {
                let previous = if start.1 > 0 {
                    (start.0, start.1 - 1)
                } else if start.0 > 0 && self.wraps(term, start.0 - 1) {
                    (start.0 - 1, self.text_end(term, start.0 - 1) - 1)
                } else {
                    break;
                };
                if self.class_at(term, previous.0, previous.1) != class {
                    break;
                }
                start = previous;
            }
            loop {
                let next = if end.1 + 1 < self.text_end(term, end.0) {
                    (end.0, end.1 + 1)
                } else if end.0 + 1 < size.rows && self.wraps(term, end.0) {
                    (end.0 + 1, 0)
                } else {
                    break;
                };
                if self.class_at(term, next.0, next.1) != class {
                    break;
                }
                end = next;
            }
        }
        // Start on the left half of a wide character, end on its right half.
        if let Some(source) = self.viewport_row(term, start.0) {
            while start.1 > 0 && source.get(start.1).width == Width::Continuation {
                start.1 -= 1;
            }
        }
        if let Some(source) = self.viewport_row(term, end.0) {
            while end.1 + 1 < size.cols && source.get(end.1 + 1).width == Width::Continuation {
                end.1 += 1;
            }
        }
        (start, end)
    }

    /// The viewport rows a triple-click on `row` selects: the line it is
    /// part of, with the rows the terminal wrapped it onto.
    pub fn line_bounds(&self, term: &Terminal, row: usize) -> (usize, usize) {
        let rows = term.size().rows;
        if rows == 0 {
            return (row, row);
        }
        let row = row.min(rows - 1);
        let mut first = row;
        while first > 0 && self.wraps(term, first - 1) {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < rows && self.wraps(term, last) {
            last += 1;
        }
        (first, last)
    }

    /// The OSC 8 hyperlink at a viewport cell, the row's text, and the char
    /// index in that text that the column falls on. Cold path: allocates.
    pub fn link_context(
        &self,
        term: &Terminal,
        row: usize,
        column: usize,
    ) -> Option<(Option<String>, String, usize)> {
        let size = term.size();
        if row >= size.rows {
            return None;
        }
        let source = term.row_by_stable(top_stable(term, self.scroll_offset) + row as u64)?;
        let uri = term.hyperlink(&source.get(column)).map(str::to_owned);
        let mut text = String::new();
        let mut index = 0;
        for x in 0..size.cols {
            if x == column {
                index = text.chars().count();
            }
            let cell = source.get(x);
            if cell.width != Width::Continuation {
                text.push_str(term.cell_text(&cell));
            }
        }
        if column >= size.cols {
            index = text.chars().count();
        }
        Some((uri, text, index))
    }

    /// Drops every row key, forcing a full rewrite on the next sync.
    pub fn invalidate(&mut self) {
        self.rows.iter_mut().for_each(|row| *row = None);
        if let Some(frozen) = &mut self.frozen {
            frozen.drawn = false;
        }
    }

    /// Keeps showing the rows in view now, whatever the program writes,
    /// until [`Self::thaw`]. Cold: converts and copies the rows.
    pub fn freeze(&mut self, term: &Terminal) {
        let size = term.size();
        let palette = term.palette();
        let foreground = color(palette.fg);
        let background = color(palette.bg);
        let top = top_stable(term, self.scroll_offset);
        let lines = (0..size.rows)
            .map_while(|row| term.row_by_stable(top + row as u64))
            .map(|source| {
                let mut cells = vec![TerminalCell::blank(foreground, background); size.cols];
                write_row(
                    term,
                    source,
                    &mut cells,
                    foreground,
                    background,
                    &mut self.links,
                );
                cells
            })
            .collect();
        self.frozen = Some(Frozen {
            lines,
            foreground,
            background,
            drawn: false,
        });
    }

    /// Shows the model's rows again, all rewritten on the next sync.
    pub fn thaw(&mut self) {
        if self.frozen.take().is_some() {
            self.rows.clear();
        }
    }

    pub fn frozen(&self) -> bool {
        self.frozen.is_some()
    }

    /// Records whether the grid has keyboard focus, which the next sync
    /// shows in the cursor. Returns whether it changed.
    pub fn set_focused(&mut self, focused: bool) -> bool {
        let changed = self.unfocused == focused;
        self.unfocused = !focused;
        changed
    }

    /// Writes rows that changed since the last call into `patch`.
    pub fn sync(
        &mut self,
        term: &Terminal,
        patch: &mut TerminalGridPatch<'_>,
        selection: Option<TerminalSelection>,
    ) {
        if self.frozen.is_some() {
            self.sync_frozen(term, patch, selection);
            return;
        }
        let palette = term.palette();
        let size = term.size();
        let (rows, cols) = (size.rows, size.cols);

        let model = patch.model();
        if model.columns != cols || model.rows != rows {
            patch.resize(cols, rows);
            self.rows.clear();
        }
        self.rows.resize(rows, None);
        self.wanted.resize(rows, RowKey::default());

        let foreground = color(palette.fg);
        let background = color(palette.bg);
        let model = patch.model();
        if model.foreground != foreground || model.background != background {
            patch.set_colors(foreground, background);
            self.invalidate();
        }
        // A program can change a palette entry without touching any row.
        if self.palette_changed(palette) {
            self.invalidate();
        }

        self.scroll_offset = self.scroll_offset.min(term.scrollback_len());
        let top = top_stable(term, self.scroll_offset);

        // Rows whose line moved (output scrolled) are copied from the row that
        // showed it before; only lines the grid never showed are converted.
        // Upward moves run top to bottom and downward moves bottom to top, so
        // a source row is copied before it is overwritten.
        self.previous.clone_from(&self.rows);
        let mut end = rows;
        for row in 0..rows {
            let Some(source) = term.row_by_stable(top + row as u64) else {
                end = row;
                break;
            };
            let key = RowKey {
                stable: top + row as u64,
                seqno: source.seqno(),
            };
            self.wanted[row] = key;
            if self.rows[row] == Some(key) {
                continue;
            }
            if let Some(from) = self.shown_at(key).filter(|&from| from > row) {
                patch.copy_row(from, row);
                self.rows[row] = Some(key);
            }
        }
        for row in (0..end).rev() {
            let key = self.wanted[row];
            if self.rows[row] == Some(key) {
                continue;
            }
            if let Some(from) = self.shown_at(key).filter(|&from| from < row) {
                patch.copy_row(from, row);
                self.rows[row] = Some(key);
            }
        }
        for row in 0..end {
            let key = self.wanted[row];
            if self.rows[row] == Some(key) {
                continue;
            }
            self.rows[row] = Some(key);
            let Some(source) = term.row_by_stable(key.stable) else {
                continue;
            };
            write_row(
                term,
                source,
                patch.row_mut(row),
                foreground,
                background,
                &mut self.links,
            );
        }

        patch.set_cursor(self.cursor(term, rows, cols, palette));
        patch.set_selection(selection);
    }

    /// Whether `palette` differs from the one the rows were converted with,
    /// remembering it. Allocation-free once the copy exists.
    fn palette_changed(&mut self, palette: &Palette) -> bool {
        let same = self.palette.len() == 256
            && self
                .palette
                .iter()
                .enumerate()
                .all(|(i, known)| *known == palette.get(i as u8));
        if same {
            return false;
        }
        self.palette.clear();
        self.palette.extend((0..=255u8).map(|i| palette.get(i)));
        true
    }

    /// Writes the frozen rows once, without a cursor. Blank cells fill a
    /// grid that grew since.
    fn sync_frozen(
        &mut self,
        term: &Terminal,
        patch: &mut TerminalGridPatch<'_>,
        selection: Option<TerminalSelection>,
    ) {
        let Some(frozen) = &mut self.frozen else {
            return;
        };
        let size = term.size();
        let (rows, cols) = (size.rows, size.cols);
        let model = patch.model();
        if model.columns != cols || model.rows != rows {
            patch.resize(cols, rows);
            frozen.drawn = false;
        }
        if !frozen.drawn {
            patch.set_colors(frozen.foreground, frozen.background);
            for row in 0..rows {
                let saved = frozen.lines.get(row);
                for (column, cell) in patch.row_mut(row).iter_mut().enumerate() {
                    match saved.and_then(|line| line.get(column)) {
                        Some(saved) => cell.clone_from(saved),
                        None => {
                            *cell = TerminalCell::blank(frozen.foreground, frozen.background);
                        }
                    }
                }
            }
            frozen.drawn = true;
        }
        patch.set_cursor(None);
        patch.set_selection(selection);
    }

    /// The row that showed `key` when the last sync ended, if it still shows
    /// it. The rows of a sync are consecutive stable lines, so the row follows
    /// from the first row's line without a search.
    fn shown_at(&self, key: RowKey) -> Option<usize> {
        let first = self.previous.first().copied().flatten()?;
        let row = usize::try_from(key.stable.checked_sub(first.stable)?).ok()?;
        (self.previous.get(row) == Some(&Some(key)) && self.rows[row] == Some(key)).then_some(row)
    }

    fn cursor(
        &self,
        term: &Terminal,
        rows: usize,
        cols: usize,
        palette: &Palette,
    ) -> Option<TerminalCursor> {
        let cursor = term.cursor();
        if self.scroll_offset != 0 || !cursor.visible || cursor.row >= rows {
            return None;
        }
        let column = cursor.col.min(cols.saturating_sub(1));
        // Blinking cursors draw steady: a blink phase would cost a repaint
        // every half second while the terminal is otherwise idle.
        let style = match cursor.shape {
            CursorShape::Bar => TerminalCursorStyle::Bar,
            CursorShape::Underline => TerminalCursorStyle::Underline,
            CursorShape::Block if self.unfocused => TerminalCursorStyle::Hollow,
            CursorShape::Block => TerminalCursorStyle::Block,
        };
        let mut cursor = TerminalCursor::new(
            TerminalGridPoint::new(cursor.row, column),
            color(palette.cursor),
        );
        cursor.style = style;
        Some(cursor)
    }
}

fn write_row(
    term: &Terminal,
    source: &Row,
    cells: &mut [TerminalCell],
    foreground: Color,
    background: Color,
    links: &mut Links,
) {
    for cell in cells.iter_mut() {
        cell.grapheme.clear();
        cell.foreground = foreground;
        cell.background = background;
        cell.attrs = TerminalCellAttrs::default();
        cell.width = TerminalCellWidth::Single;
        cell.hyperlink = None;
    }
    // Neighboring cells nearly always share their style, so the resolved
    // style is reused until the id changes. That also shares one `Arc` per
    // hyperlink run.
    let palette = term.palette();
    let mut style: Option<(StyleId, CellStyle)> = None;
    for (index, src) in source.cells().iter().enumerate() {
        let Some(cell) = cells.get_mut(index) else {
            break;
        };
        let current = match &style {
            Some((last, current)) if *last == src.style => current,
            _ => {
                &style
                    .insert((src.style, CellStyle::new(term, src, palette, links)))
                    .1
            }
        };
        cell.foreground = current.foreground;
        cell.background = current.background;
        match src.width {
            Width::Continuation => {
                cell.width = TerminalCellWidth::Continuation;
                continue;
            }
            Width::Wide => cell.width = TerminalCellWidth::Wide,
            Width::Single => {}
        }
        cell.attrs = current.attrs;
        let text = term.cell_text(src);
        if !current.invisible && text != " " {
            cell.grapheme.push_str(text);
        }
        cell.hyperlink.clone_from(&current.hyperlink);
    }
}

/// The resolved look of a run of cells that share their style.
struct CellStyle {
    foreground: Color,
    background: Color,
    attrs: TerminalCellAttrs,
    invisible: bool,
    hyperlink: Option<Arc<str>>,
}

impl CellStyle {
    fn new(term: &Terminal, cell: &Cell, palette: &Palette, links: &mut Links) -> Self {
        let style: &Style = term.style(cell.style);
        let bold = style.has(flags::BOLD);
        let mut fg = palette.resolve(style.fg, true);
        // Bold text in one of the first eight palette colors uses its bright twin.
        if bold && let cmdterm_vt::Color::Palette(index @ 0..8) = style.fg {
            fg = palette.get(index + 8);
        }
        let hyperlink = if style.hyperlink == 0 {
            None
        } else {
            term.hyperlink(cell)
                .map(|uri| links.get(style.hyperlink, uri))
        };
        Self {
            foreground: color(fg),
            background: color(palette.resolve(style.bg, false)),
            attrs: TerminalCellAttrs {
                bold,
                italic: style.has(flags::ITALIC),
                underline: style.underline != cmdterm_vt::Underline::None,
                strike: style.has(flags::STRIKE),
                faint: style.has(flags::FAINT),
                inverse: style.has(flags::INVERSE),
            },
            invisible: style.has(flags::INVISIBLE),
            hyperlink,
        }
    }
}

fn color(rgb: Rgb) -> Color {
    Color::rgba(rgb.r, rgb.g, rgb.b, 255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::TermColors;
    use cmdterm_vt::Size;
    use schnellui::App;
    use schnellui::widgets::{TerminalCellMetrics, TerminalGrid};
    use std::cell::RefCell;
    use std::rc::Rc;

    type Shared<T> = Rc<RefCell<T>>;

    /// A grid kept in step with `term` by `sync`, and a copy of its cells after
    /// each frame. `fresh` makes every frame start from a new `GridSync`, which
    /// converts every row and is the reference for the reusing one.
    fn mount(
        term: &Shared<Terminal>,
        revision: &Rc<std::cell::Cell<u64>>,
        fresh: bool,
    ) -> (App, Shared<Vec<TerminalCell>>) {
        let term = term.clone();
        let revision = revision.clone();
        let cells: Shared<Vec<TerminalCell>> = Rc::default();
        let out = cells.clone();
        let mut sync = GridSync::new();
        let grid = TerminalGrid::patched(
            move || revision.get(),
            move |patch| {
                if fresh {
                    sync = GridSync::new();
                }
                sync.sync(&term.borrow(), patch, None);
                out.borrow_mut().clone_from(&patch.model().cells);
            },
        )
        .metrics(TerminalCellMetrics::new(8.0, 16.0, 13.0));
        (App::mount_with_size(grid, 160, 128), cells)
    }

    fn terminal(rows: usize, cols: usize) -> Shared<Terminal> {
        Rc::new(RefCell::new(Terminal::new(
            Size { rows, cols },
            TermColors::default().config(),
        )))
    }

    #[test]
    fn scrolled_rows_reuse_matches_a_full_conversion() {
        let term = terminal(8, 20);
        let revision = Rc::new(std::cell::Cell::new(0));
        let (mut reusing, reused) = mount(&term, &revision, false);
        let (mut converting, converted) = mount(&term, &revision, true);
        let steps: [&[u8]; 6] = [
            b"\x1b[31mred\x1b[0m one\r\ntwo\r\nthree",
            b"\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\r\nnine",
            b"\r\n\x1b[1;32mten\x1b[0m\r\neleven",
            // Scroll down: reverse index at the top row.
            b"\x1b[H\x1bM\x1bMnew top",
            // Change a line in place without moving it.
            b"\x1b[4;1Hedited",
            b"\x1b[2J\x1b[Hcleared\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8",
        ];
        for step in steps {
            term.borrow_mut().advance(step);
            revision.set(revision.get() + 1);
            reusing.frame();
            converting.frame();
            assert_eq!(*reused.borrow(), *converted.borrow());
        }
    }

    fn text_of(cells: &[TerminalCell], row: usize, cols: usize) -> String {
        cells[row * cols..(row + 1) * cols]
            .iter()
            .map(|cell| {
                if cell.grapheme.is_empty() {
                    " "
                } else {
                    cell.grapheme.as_str()
                }
            })
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn styles_resolve_through_the_palette() {
        let term = terminal(3, 20);
        let revision = Rc::new(std::cell::Cell::new(0));
        let (mut app, cells) = mount(&term, &revision, false);
        // Bold red uses the bright twin, a 256-color index and truecolor
        // resolve through the palette, underline and invisible set flags.
        term.borrow_mut().advance(
            b"\x1b[1;31mA\x1b[0m\x1b[38;5;196mB\x1b[0m\x1b[38;2;1;2;3mC\x1b[0m\x1b[4mD\x1b[0m\x1b[8mE",
        );
        revision.set(1);
        app.frame();
        let cells = cells.borrow();
        let term = term.borrow();
        let palette = term.palette();
        assert_eq!(cells[0].foreground, color(palette.get(9)));
        assert!(cells[0].attrs.bold);
        assert_eq!(cells[1].foreground, color(palette.get(196)));
        assert_eq!(cells[2].foreground, Color::rgba(1, 2, 3, 255));
        assert!(cells[3].attrs.underline);
        assert_eq!(cells[4].grapheme.as_str(), "");
        assert_eq!(cells[10].foreground, color(palette.fg));
        assert_eq!(cells[10].background, color(palette.bg));
    }

    #[test]
    fn wide_cells_and_clusters_fill_the_grid() {
        let term = terminal(2, 10);
        let revision = Rc::new(std::cell::Cell::new(0));
        let (mut app, cells) = mount(&term, &revision, false);
        term.borrow_mut().advance("a\u{4e16}e\u{301}".as_bytes());
        revision.set(1);
        app.frame();
        let cells = cells.borrow();
        assert_eq!(cells[1].grapheme.as_str(), "\u{4e16}");
        assert_eq!(cells[1].width, TerminalCellWidth::Wide);
        assert_eq!(cells[2].width, TerminalCellWidth::Continuation);
        assert_eq!(cells[3].grapheme.as_str(), "e\u{301}");
    }

    #[test]
    fn hyperlinks_share_one_target_per_run() {
        let term = terminal(3, 30);
        let revision = Rc::new(std::cell::Cell::new(0));
        let (mut app, cells) = mount(&term, &revision, false);
        term.borrow_mut().advance(
            b"\x1b]8;;https://a.example/x\x07link\x1b]8;;\x07 plain\r\n\x1b]8;;https://a.example/x\x07again\x1b]8;;\x07",
        );
        revision.set(1);
        app.frame();
        let cells = cells.borrow();
        let first = cells[0].hyperlink.clone().unwrap();
        assert_eq!(&*first, "https://a.example/x");
        assert!(Arc::ptr_eq(&first, cells[3].hyperlink.as_ref().unwrap()));
        assert!(cells[4].hyperlink.is_none());
        // The same target on another row shares the allocation too.
        assert!(Arc::ptr_eq(&first, cells[30].hyperlink.as_ref().unwrap()));
        let sync = GridSync::new();
        let term = term.borrow();
        let (uri, text, index) = sync.link_context(&term, 0, 2).unwrap();
        assert_eq!(uri.as_deref(), Some("https://a.example/x"));
        assert_eq!(text.trim_end(), "link plain");
        assert_eq!(index, 2);
        assert_eq!(sync.link_context(&term, 0, 6).unwrap().0, None);
    }

    #[test]
    fn selection_text_walks_viewport_and_scrollback() {
        let term = terminal(3, 12);
        let mut sync = GridSync::new();
        term.borrow_mut()
            .advance(b"one\r\ntwo\r\n\xe4\xb8\x96x\r\nfour\r\nfive");
        let term = term.borrow();
        assert_eq!(
            sync.selection_text(&term, (0, 0), (2, 11)),
            "\u{4e16}x\nfour\nfive"
        );
        // The right half of a wide character adds no text.
        assert_eq!(sync.selection_text(&term, (0, 1), (0, 2)), "x");
        // Either order of the corners.
        assert_eq!(
            sync.selection_text(&term, (1, 3), (0, 0)),
            "\u{4e16}x\nfour"
        );
        assert!(sync.scroll_by(&term, 2));
        assert_eq!(sync.selection_text(&term, (0, 0), (1, 5)), "one\ntwo");
        // Already at the oldest line.
        assert!(!sync.scroll_by(&term, 100));
        assert_eq!(sync.scroll_offset(), term.scrollback_len());
    }

    #[test]
    fn double_click_words_follow_their_class() {
        let term = terminal(3, 30);
        let sync = GridSync::new();
        term.borrow_mut()
            .advance("ls src/main.rs:12 (x)  \u{4e16}\u{754c}!".as_bytes());
        let term = term.borrow();
        // A path with a line number is one word.
        assert_eq!(sync.word_bounds(&term, 0, 6), ((0, 3), (0, 16)));
        assert_eq!(sync.word_bounds(&term, 0, 0), ((0, 0), (0, 1)));
        // A bracket is one character, the blanks between words one run.
        assert_eq!(sync.word_bounds(&term, 0, 18), ((0, 18), (0, 18)));
        assert_eq!(sync.word_bounds(&term, 0, 21), ((0, 21), (0, 22)));
        // Wide characters: either half selects both, with their right halves.
        assert_eq!(sync.word_bounds(&term, 0, 24), ((0, 23), (0, 27)));
        // Past the text the trailing blanks are one run.
        assert_eq!(sync.word_bounds(&term, 0, 29), ((0, 28), (0, 29)));
    }

    #[test]
    fn words_lines_and_copies_cross_wrapped_rows() {
        let term = terminal(4, 10);
        let sync = GridSync::new();
        term.borrow_mut().advance(b"echo abcdefghij done\r\nnext");
        let term = term.borrow();
        assert_eq!(sync.word_bounds(&term, 0, 7), ((0, 5), (1, 4)));
        assert_eq!(sync.word_bounds(&term, 1, 0), ((0, 5), (1, 4)));
        assert_eq!(sync.line_bounds(&term, 1), (0, 1));
        assert_eq!(sync.line_bounds(&term, 2), (2, 2));
        assert_eq!(
            sync.selection_text(&term, (0, 0), (2, 9)),
            "echo abcdefghij done\nnext"
        );
        assert_eq!(sync.selection_text(&term, (0, 5), (1, 4)), "abcdefghij");
    }

    #[test]
    fn a_frozen_grid_ignores_later_output() {
        let term = terminal(3, 10);
        let revision = Rc::new(std::cell::Cell::new(0));
        let sync = Rc::new(RefCell::new(GridSync::new()));
        let cells: Shared<Vec<TerminalCell>> = Rc::default();
        let grid = {
            let (term, revision, sync, out) =
                (term.clone(), revision.clone(), sync.clone(), cells.clone());
            TerminalGrid::patched(
                move || revision.get(),
                move |patch| {
                    sync.borrow_mut().sync(&term.borrow(), patch, None);
                    out.borrow_mut().clone_from(&patch.model().cells);
                },
            )
            .metrics(TerminalCellMetrics::new(8.0, 16.0, 13.0))
        };
        let mut app = App::mount_with_size(grid, 80, 48);
        term.borrow_mut().advance(b"\x1b[31mfrozen");
        sync.borrow_mut().freeze(&term.borrow());
        term.borrow_mut().advance(b"\x1b[2J\x1b[Hnew text");
        revision.set(1);
        app.frame();
        assert_eq!(text_of(&cells.borrow(), 0, 10), "frozen");
        sync.borrow_mut().thaw();
        revision.set(2);
        app.frame();
        assert_eq!(text_of(&cells.borrow(), 0, 10), "new text");
    }

    /// Frames of scrolling, colored, linked and wide output convert rows
    /// without touching the heap once the grid and caches are warm.
    #[cfg(feature = "count-allocations")]
    #[test]
    fn syncing_allocates_nothing_in_steady_state() {
        let term = terminal(8, 30);
        let revision = Rc::new(std::cell::Cell::new(0));
        let allocated = Rc::new(std::cell::Cell::new(0u64));
        let mut sync = GridSync::new();
        let grid = {
            let (term, revision, allocated) = (term.clone(), revision.clone(), allocated.clone());
            TerminalGrid::patched(
                move || revision.get(),
                move |patch| {
                    let term = term.borrow();
                    let count = allocation_counter::measure(|| {
                        sync.sync(&term, patch, None);
                    });
                    allocated.set(allocated.get() + count.count_total);
                },
            )
            .metrics(TerminalCellMetrics::new(8.0, 16.0, 13.0))
        };
        let mut app = App::mount_with_size(grid, 240, 128);
        let line: &[u8] =
            b"\x1b[1;31mred\x1b[0m \x1b]8;;https://a.example/x\x07link\x1b]8;;\x07 \xe4\xb8\x96 e\xcc\x81\r\n";
        for _ in 0..40 {
            term.borrow_mut().advance(line);
            revision.set(revision.get() + 1);
            app.frame();
        }
        allocated.set(0);
        for _ in 0..40 {
            term.borrow_mut().advance(line);
            revision.set(revision.get() + 1);
            app.frame();
        }
        assert_eq!(allocated.get(), 0, "sync allocated while following output");
    }

    /// Colored text the way `ls --color`, a pager or a TUI fills a screen: many
    /// short style runs per row, some wide cells, one hyperlink.
    fn busy_screen(rows: usize, cols: usize, salt: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 0..rows {
            out.extend_from_slice(b"\x1b[0m");
            let mut col = 0;
            let mut run = 0;
            while col + 12 < cols {
                let color = 16 + (row * 7 + run * 13 + salt) % 200;
                let bold = if run % 3 == 0 { "1;" } else { "" };
                out.extend_from_slice(format!("\x1b[{bold}38;5;{color}m").as_bytes());
                if run % 4 == 1 {
                    out.extend_from_slice(format!("\x1b[48;5;{}m", 232 + run % 20).as_bytes());
                }
                if run % 5 == 2 {
                    out.extend_from_slice("\u{6f22}\u{5b57}\u{304b}\u{306a}x".as_bytes());
                    col += 9;
                } else {
                    out.extend_from_slice(b"text/line");
                    col += 9;
                }
                out.extend_from_slice(b"  ");
                col += 2;
                run += 1;
            }
            if row + 1 < rows {
                out.extend_from_slice(b"\r\n");
            }
        }
        out
    }

    /// Time of `GridSync::sync` alone, per full-screen conversion and per
    /// scrolled frame. Run with
    /// `cargo test -p cmdterm --release sync_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing, run by hand in release"]
    fn sync_cost_per_screen() {
        for (rows, cols) in [(50, 80), (60, 200)] {
            let term = terminal(rows, cols);
            term.borrow_mut().advance(&busy_screen(rows, cols, 0));
            let spent = Rc::new(std::cell::Cell::new(std::time::Duration::ZERO));
            let revision = Rc::new(std::cell::Cell::new(0u64));
            let mount_timed = |fresh: bool| {
                let term = term.clone();
                let revision = revision.clone();
                let spent = spent.clone();
                let mut sync = GridSync::new();
                let grid = TerminalGrid::patched(
                    move || revision.get(),
                    move |patch| {
                        if fresh {
                            sync = GridSync::new();
                        }
                        let term = term.borrow();
                        let start = std::time::Instant::now();
                        sync.sync(&term, patch, None);
                        spent.set(spent.get() + start.elapsed());
                    },
                )
                .metrics(TerminalCellMetrics::new(8.0, 16.0, 13.0));
                App::mount_with_size(grid, cols as u32 * 8, rows as u32 * 16)
            };
            let mut full = mount_timed(true);
            let mut scrolling = mount_timed(false);
            const FRAMES: u32 = 400;
            let median = |app: &mut App, step: &mut dyn FnMut(u32)| {
                let mut samples = Vec::new();
                for i in 0..FRAMES + 20 {
                    step(i);
                    revision.set(revision.get() + 1);
                    spent.set(std::time::Duration::ZERO);
                    app.frame();
                    if i >= 20 {
                        samples.push(spent.get());
                    }
                }
                samples.sort();
                samples[samples.len() / 2]
            };
            let full_cost = median(&mut full, &mut |_| {});
            let scroll_cost = median(&mut scrolling, &mut |i| {
                let line = busy_screen(1, cols, i as usize);
                let mut t = term.borrow_mut();
                t.advance(b"\r\n");
                t.advance(&line);
            });
            println!(
                "SYNC_COST {cols}x{rows} full_screen_us={:.1} scroll_frame_us={:.1}",
                full_cost.as_secs_f64() * 1e6,
                scroll_cost.as_secs_f64() * 1e6,
            );
        }
    }
}
