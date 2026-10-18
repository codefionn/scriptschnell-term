//! Rows and the screen: a viewport of rows plus a scrollback ring.
//!
//! # Model
//!
//! A [`Row`] stores cells only up to its last non-blank cell. A blank is a space in the
//! default style; a space with a background color is kept. Reading a cell past the end
//! gives [`Cell::BLANK`]. Every mutation that changes a row gives it a fresh sequence
//! number, so a renderer repaints exactly the rows whose number moved. A write that
//! leaves the row as it was changes nothing.
//!
//! The stable index of a line is its absolute line number: lines that were scrolled out
//! of the ring come first, then the scrollback, then the viewport. Scrolling a line into
//! the scrollback moves the row and keeps its stable index.
//!
//! A cell holds one grapheme cluster. A cluster of several characters is a cell with a
//! cluster id; its text lives in the terminal's cluster table, so reading a row's text
//! goes through `Terminal::row_text`. A wide cluster is a `Wide` cell plus a
//! `Continuation` cell, like a wide character.
//!
//! # Allocation
//!
//! Printing into a row whose vector has room allocates nothing. Scrolling moves `Row`
//! values; it does not copy cells. Margin-limited scrolling (DECLRMM), resizing and
//! reflow copy cells and may allocate.

use crate::cell::{Cell, Width};
use crate::grapheme::ClusterTable;
use crate::style::StyleId;
use std::collections::VecDeque;
use std::mem;

/// One line of cells.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub(crate) cells: Vec<Cell>,
    seqno: u64,
    pub(crate) wrapped: bool,
    /// For a wrapped row: how many columns hold text when that is fewer than the row is
    /// wide (a wide character did not fit and moved to the next row, or the right margin
    /// is left of the edge). 0 means the whole row. Reflow joins rows by this.
    pub(crate) wrap_cols: u32,
}

impl Row {
    /// The stored cells, without trailing blanks. No decoding needed.
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    /// Number of stored cells.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The cell at column `x`; blank past the stored cells.
    pub fn get(&self, x: usize) -> Cell {
        self.cells.get(x).copied().unwrap_or(Cell::BLANK)
    }

    /// Changes whenever the row changes.
    pub fn seqno(&self) -> u64 {
        self.seqno
    }

    /// True when the text continues on the next row because it hit the right edge.
    pub fn wrapped(&self) -> bool {
        self.wrapped
    }

    /// For a wrapped row, how many of the `cols` columns hold text. Fewer than `cols`
    /// when a wide character did not fit and moved to the next row. The text of a
    /// logical line is its wrapped rows padded to this width, then the last row.
    pub fn text_cols(&self, cols: usize) -> usize {
        match self.wrap_cols {
            0 => cols.max(self.cells.len()),
            n => (n as usize).max(self.cells.len()),
        }
    }

    /// The row's text with cluster cells read through `clusters`, without the
    /// continuation halves of wide characters and without trailing blanks.
    /// `Terminal::row_text` calls this with the terminal's table.
    pub fn text(&self, clusters: &ClusterTable) -> String {
        let mut s = String::new();
        self.push_text(clusters, &mut s);
        s
    }

    pub fn push_text(&self, clusters: &ClusterTable, out: &mut String) {
        for c in &self.cells {
            if c.width != Width::Continuation {
                out.push_str(if c.cluster() == 0 {
                    c.first()
                } else {
                    clusters.get(c.cluster())
                });
            }
        }
        let trimmed = out.trim_end_matches(' ').len();
        out.truncate(trimmed);
    }

    pub(crate) fn touch(&mut self, seq: &mut u64) {
        *seq += 1;
        self.seqno = *seq;
    }

    pub(crate) fn trim(&mut self) {
        while self.cells.last() == Some(&Cell::BLANK) {
            self.cells.pop();
        }
    }

    fn blank_half(&mut self, i: usize) {
        let style = self.cells[i].style;
        self.cells[i] = Cell::blank(style);
    }

    /// Blanks the partner of a wide character that the cell at `i` belongs to.
    fn split(&mut self, i: usize) {
        if i >= self.cells.len() {
            return;
        }
        match self.cells[i].width {
            Width::Continuation if i > 0 => self.blank_half(i - 1),
            Width::Wide if i + 1 < self.cells.len() => self.blank_half(i + 1),
            _ => {}
        }
    }

    /// Blanks wide halves that lost their partner.
    fn fix_wide(&mut self) {
        for i in 0..self.cells.len() {
            match self.cells[i].width {
                Width::Wide => {
                    if self.cells.get(i + 1).map(|c| c.width) != Some(Width::Continuation) {
                        self.blank_half(i);
                    }
                }
                Width::Continuation => {
                    if i == 0 || self.cells[i - 1].width != Width::Wide {
                        self.blank_half(i);
                    }
                }
                Width::Single => {}
            }
        }
    }

    fn pad(&mut self, n: usize) {
        if self.cells.len() < n {
            self.cells.resize(n, Cell::BLANK);
        }
    }
}

/// The viewport and the scrollback of one screen (primary or alternate).
#[derive(Debug, Clone)]
pub struct Screen {
    pub(crate) rows: Vec<Row>,
    pub(crate) scrollback: VecDeque<Row>,
    pub(crate) limit: usize,
    pub(crate) base: u64,
    /// Rows that left the screen for good (scrolled out of the scrollback, or
    /// dropped without one). Table collection counts these.
    pub(crate) dropped: u64,
    pub(crate) seq: u64,
    pub(crate) cols: usize,
    /// A row that left the screen, kept empty with its buffer so the next row that
    /// scrolls in does not allocate.
    spare: Option<Row>,
}

impl Screen {
    pub fn new(rows: usize, cols: usize, scrollback_limit: usize) -> Self {
        let mut s = Screen {
            rows: Vec::with_capacity(rows),
            scrollback: VecDeque::new(),
            limit: scrollback_limit,
            base: 0,
            dropped: 0,
            seq: 0,
            cols,
            spare: None,
        };
        for _ in 0..rows {
            s.rows.push(Row::default());
        }
        s.touch_all();
        s
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The viewport row `y`, 0 at the top.
    pub fn row(&self, y: usize) -> Option<&Row> {
        self.rows.get(y)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Scrollback row `i`, 0 the oldest.
    pub fn scrollback_row(&self, i: usize) -> Option<&Row> {
        self.scrollback.get(i)
    }

    /// Stable index of the oldest line still kept.
    pub fn first_stable(&self) -> u64 {
        self.base
    }

    /// Stable index of viewport row `y`.
    pub fn stable_index(&self, y: usize) -> u64 {
        self.base + self.scrollback.len() as u64 + y as u64
    }

    /// The line with this stable index, in the scrollback or the viewport.
    pub fn row_by_stable(&self, stable: u64) -> Option<&Row> {
        let i = stable.checked_sub(self.base)? as usize;
        if i < self.scrollback.len() {
            self.scrollback.get(i)
        } else {
            self.rows.get(i - self.scrollback.len())
        }
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// Makes this screen's counter at least `seq` and gives every viewport row a new
    /// number, so a renderer that switches to this screen repaints it.
    pub(crate) fn resync(&mut self, seq: u64) {
        self.seq = self.seq.max(seq);
        self.touch_all();
    }

    /// The stable index one past the last viewport row.
    pub(crate) fn next_stable(&self) -> u64 {
        self.base + self.scrollback.len() as u64 + self.rows.len() as u64
    }

    /// Continues the counters of an earlier screen, for a full reset: sequence numbers
    /// and stable indexes never repeat or go back.
    pub(crate) fn continue_from(&mut self, seq: u64, base: u64) {
        self.seq = self.seq.max(seq);
        self.base = self.base.max(base);
        self.touch_all();
    }

    fn touch_all(&mut self) {
        for r in &mut self.rows {
            r.touch(&mut self.seq);
        }
    }

    /// Keeps the buffer of a row that is gone for the next row that scrolls in.
    fn recycle(&mut self, mut row: Row) {
        row.cells.clear();
        row.wrapped = false;
        row.wrap_cols = 0;
        // Room for a full line, so printing into it never grows it.
        row.cells.reserve_exact(self.cols);
        self.spare = Some(row);
    }

    fn push_scrollback(&mut self, row: Row) {
        if self.limit == 0 {
            self.base += 1;
            self.dropped += 1;
            self.recycle(row);
            return;
        }
        self.scrollback.push_back(row);
        while self.scrollback.len() > self.limit {
            if let Some(gone) = self.scrollback.pop_front() {
                self.recycle(gone);
            }
            self.base += 1;
            self.dropped += 1;
        }
    }

    pub(crate) fn clear_scrollback(&mut self) {
        self.base += self.scrollback.len() as u64;
        self.dropped += self.scrollback.len() as u64;
        self.scrollback.clear();
    }

    /// Marks row `y` as continuing on the next row. `text_cols` is how many columns hold
    /// text: fewer than the width when a wide character moved to the next row.
    pub(crate) fn set_wrapped(&mut self, y: usize, text_cols: usize) {
        let cols = self.cols;
        let row = &mut self.rows[y];
        let wrap_cols = if text_cols >= cols {
            0
        } else {
            text_cols as u32
        };
        if !row.wrapped || row.wrap_cols != wrap_cols {
            row.wrapped = true;
            row.wrap_cols = wrap_cols;
            row.touch(&mut self.seq);
        }
    }

    /// Replaces cells `from..to` of row `y` with blanks in `style`.
    pub(crate) fn erase(&mut self, y: usize, from: usize, to: usize, style: StyleId) {
        let to = to.min(self.cols);
        if from >= to {
            return;
        }
        let cols = self.cols;
        let row = &mut self.rows[y];
        let len = row.cells.len();
        let mut changed = false;
        if style == StyleId::DEFAULT && from >= len {
            if to >= cols && row.wrapped {
                row.wrapped = false;
                row.wrap_cols = 0;
                row.touch(&mut self.seq);
            }
            return;
        }
        if from > 0 && from < len && row.cells[from].width == Width::Continuation {
            row.blank_half(from - 1);
            changed = true;
        }
        if to < len && row.cells[to].width == Width::Continuation {
            row.blank_half(to);
            changed = true;
        }
        if style != StyleId::DEFAULT && len < to {
            row.cells.resize(to, Cell::BLANK);
            changed = true;
        }
        let blank = Cell::blank(style);
        let end = to.min(row.cells.len());
        for c in &mut row.cells[from..end] {
            if *c != blank {
                *c = blank;
                changed = true;
            }
        }
        row.trim();
        if to >= cols && row.wrapped {
            row.wrapped = false;
            row.wrap_cols = 0;
            changed = true;
        }
        if changed {
            row.touch(&mut self.seq);
        }
    }

    /// Blanks all of row `y`.
    pub(crate) fn clear_row(&mut self, y: usize, style: StyleId) {
        self.erase(y, 0, self.cols, style);
    }

    /// Writes `cell` (one character or a cluster) at `(y, x)`, `w` columns wide (1 or 2).
    /// The caller made sure it fits. Replaces a cell of the same text in place, which is
    /// how a cluster grows.
    pub(crate) fn put_cell(&mut self, y: usize, x: usize, cell: Cell, w: usize) {
        let row = &mut self.rows[y];
        let mut new = cell;
        new.width = if w == 2 { Width::Wide } else { Width::Single };
        let cont = Cell::new(' ', cell.style, Width::Continuation);
        let len = row.cells.len();
        if w == 1 && new == Cell::BLANK && x >= len {
            // The row already reads blank here.
            return;
        }
        if x + w <= len && row.cells[x] == new && (w == 1 || row.cells[x + 1] == cont) {
            return;
        }
        row.split(x);
        if w == 2 {
            row.split(x + 1);
        }
        row.pad(x + w);
        row.cells[x] = new;
        if w == 2 {
            row.cells[x + 1] = cont;
        }
        row.trim();
        row.touch(&mut self.seq);
    }

    pub(crate) fn scrollback_limit(&self) -> usize {
        self.limit
    }

    /// Rows dropped so far; see the field.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Marks the style ids that cells of this screen refer to.
    pub(crate) fn mark_styles(&self, marks: &mut [bool]) {
        for r in self.rows.iter().chain(self.scrollback.iter()) {
            for c in &r.cells {
                if let Some(m) = marks.get_mut(c.style.0 as usize) {
                    *m = true;
                }
            }
        }
    }

    /// Rewrites every cell's style id through `map` (old id to new id). Rows keep their
    /// sequence numbers: what they show does not change.
    pub(crate) fn remap_styles(&mut self, map: &[u32]) {
        for r in self.rows.iter_mut().chain(self.scrollback.iter_mut()) {
            for c in &mut r.cells {
                c.style = StyleId(map.get(c.style.0 as usize).copied().unwrap_or(0));
            }
        }
    }

    /// Marks the cluster ids that cells of this screen refer to.
    pub(crate) fn mark_clusters(&self, marks: &mut [bool]) {
        for r in self.rows.iter().chain(self.scrollback.iter()) {
            for c in &r.cells {
                if let Some(m) = marks.get_mut(c.cluster() as usize) {
                    *m = true;
                }
            }
        }
    }

    /// ICH: shifts cells `x..=right` right by `n`, filling with blanks.
    pub(crate) fn insert_cells(
        &mut self,
        y: usize,
        x: usize,
        n: usize,
        right: usize,
        style: StyleId,
    ) {
        self.shift_cells(y, x, n, right, style, true);
    }

    /// DCH: shifts cells `x..=right` left by `n`, filling the end with blanks.
    pub(crate) fn delete_cells(
        &mut self,
        y: usize,
        x: usize,
        n: usize,
        right: usize,
        style: StyleId,
    ) {
        self.shift_cells(y, x, n, right, style, false);
    }

    fn shift_cells(
        &mut self,
        y: usize,
        x: usize,
        n: usize,
        right: usize,
        style: StyleId,
        ins: bool,
    ) {
        let right = right.min(self.cols - 1);
        if x > right {
            return;
        }
        let row = &mut self.rows[y];
        if style == StyleId::DEFAULT && x >= row.cells.len() {
            return;
        }
        let n = n.min(right - x + 1);
        let blank = Cell::blank(style);
        let same = if ins {
            (x..=right).all(|i| row.get(i) == if i < x + n { blank } else { row.get(i - n) })
        } else {
            (x..=right).all(|i| {
                row.get(i)
                    == if i + n <= right {
                        row.get(i + n)
                    } else {
                        blank
                    }
            })
        };
        if same {
            return;
        }
        row.pad(right + 1);
        // A wide character that starts at `x` moves whole. Only one that starts left of
        // `x` is cut.
        if x > 0 && row.cells[x].width == Width::Continuation {
            row.blank_half(x - 1);
        }
        let span = &mut row.cells[x..=right];
        if ins {
            span.rotate_right(n);
            span[..n].fill(blank);
        } else {
            span.rotate_left(n);
            let l = span.len();
            span[l - n..].fill(blank);
        }
        row.fix_wide();
        row.trim();
        row.touch(&mut self.seq);
    }

    /// Scrolls rows `top..=bottom`, columns `left..=right`, up by `n`. With
    /// `to_scrollback` the rows that leave the top move into the scrollback.
    pub(crate) fn scroll_up(
        &mut self,
        region: Region,
        n: usize,
        style: StyleId,
        to_scrollback: bool,
    ) {
        let Region {
            top,
            bottom,
            left,
            right,
        } = region;
        let n = n.min(bottom - top + 1);
        if n == 0 {
            return;
        }
        if left == 0 && right + 1 >= self.cols {
            let before = self.base + self.scrollback.len() as u64;
            self.rows[top..=bottom].rotate_left(n);
            for i in (bottom + 1 - n)..=bottom {
                let fresh = self.spare.take().unwrap_or_default();
                let old = mem::replace(&mut self.rows[i], fresh);
                if to_scrollback {
                    self.push_scrollback(old);
                } else if top == 0 && self.scrollback.is_empty() {
                    // The line is dropped. Without scrollback (the alternate
                    // screen, or a limit of 0) the later lines keep their stable
                    // index. With scrollback, bumping `base` would renumber every
                    // kept line, so the rows below renumber instead; they are
                    // touched below, which makes caches repaint them.
                    self.base += 1;
                    self.dropped += 1;
                    self.recycle(old);
                } else {
                    self.recycle(old);
                }
                self.clear_row(i, style);
            }
            // A line that moved up kept its stable index when the lines above it
            // went to the scrollback or were dropped from the top, so it is
            // unchanged and keeps its number: a renderer that remembers the line
            // can copy it instead of converting it again. Only the lines that
            // scrolled in and lines whose index moved need a new number.
            let kept = to_scrollback || (top == 0 && self.scrollback.is_empty());
            let touched = if kept { bottom + 1 - n } else { top };
            for r in &mut self.rows[touched..=bottom] {
                r.touch(&mut self.seq);
            }
            if self.base + self.scrollback.len() as u64 != before {
                // Stable indexes are positional, so rows below a partial region
                // renumber. Give them new sequence numbers so caches repaint.
                for r in &mut self.rows[bottom + 1..] {
                    r.touch(&mut self.seq);
                }
            }
        } else {
            for y in top..=bottom {
                if y + n <= bottom {
                    self.copy_span(y, y + n, left, right);
                } else {
                    self.erase(y, left, right + 1, style);
                }
            }
        }
    }

    /// Scrolls rows `top..=bottom`, columns `left..=right`, down by `n`.
    pub(crate) fn scroll_down(&mut self, region: Region, n: usize, style: StyleId) {
        let Region {
            top,
            bottom,
            left,
            right,
        } = region;
        let n = n.min(bottom - top + 1);
        if n == 0 {
            return;
        }
        if left == 0 && right + 1 >= self.cols {
            self.rows[top..=bottom].rotate_right(n);
            for i in top..top + n {
                let r = &mut self.rows[i];
                r.cells.clear();
                r.wrapped = false;
                r.wrap_cols = 0;
                self.clear_row(i, style);
            }
            for r in &mut self.rows[top..=bottom] {
                r.touch(&mut self.seq);
            }
        } else {
            for y in (top..=bottom).rev() {
                if y >= top + n {
                    self.copy_span(y, y - n, left, right);
                } else {
                    self.erase(y, left, right + 1, style);
                }
            }
        }
    }

    fn copy_span(&mut self, dst: usize, src: usize, left: usize, right: usize) {
        let same = (left..=right).all(|x| self.rows[dst].get(x) == self.rows[src].get(x));
        if same {
            return;
        }
        self.rows[dst].pad(right + 1);
        for x in left..=right {
            let c = self.rows[src].get(x);
            self.rows[dst].cells[x] = c;
        }
        let row = &mut self.rows[dst];
        row.fix_wide();
        row.trim();
        row.touch(&mut self.seq);
    }

    /// Changes the viewport size by cutting and padding, without reflow (the alternate
    /// screen, and the primary screen when only the row count changes; a column change of
    /// the primary screen goes through `reflow`). Rows pushed off the top go to the
    /// scrollback when `to_scrollback` is set. Returns how many rows left the top.
    pub(crate) fn resize(
        &mut self,
        rows: usize,
        cols: usize,
        cursor_row: usize,
        to_scrollback: bool,
    ) -> usize {
        let mut shifted = 0;
        while self.rows.len() > rows {
            if cursor_row > shifted && cursor_row - shifted >= rows {
                let old = self.rows.remove(0);
                if to_scrollback {
                    self.push_scrollback(old);
                } else if self.scrollback.is_empty() {
                    // As in `scroll_up`: never renumber kept scrollback.
                    self.base += 1;
                    self.dropped += 1;
                }
                shifted += 1;
            } else {
                self.rows.pop();
            }
        }
        while self.rows.len() < rows {
            self.rows.push(Row::default());
        }
        if cols < self.cols {
            for r in self.rows.iter_mut().chain(self.scrollback.iter_mut()) {
                if r.cells.len() > cols {
                    r.cells.truncate(cols);
                    r.fix_wide();
                    r.trim();
                }
            }
        }
        self.cols = cols;
        self.touch_all();
        shifted
    }
}

/// A scrolling area: rows `top..=bottom`, columns `left..=right`, all inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub top: usize,
    pub bottom: usize,
    pub left: usize,
    pub right: usize,
}
