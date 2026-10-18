//! Reflow: rewraps the primary screen and its scrollback when the column count changes.
//!
//! # Model
//!
//! A logical line is a run of rows joined by the wrapped flag: every row but the last has
//! it set. Reflow cuts each logical line into rows of the new width. A wide character or
//! cluster never splits: when it does not fit at the end of a row it moves whole to the
//! next row, and the row records how many columns hold text (`Row::wrap_cols`), so a later
//! reflow does not read the gap as a space. Trailing default blanks of a logical line go
//! away; blanks with a style stay.
//!
//! Points the caller names (the cursor, the saved cursor, semantic marks) are tracked by
//! their column offset in the logical line and land on the same character afterwards.
//! A point past the end of the text keeps its distance from the end, clamped to the row.
//!
//! The viewport after reflow keeps the cursor at the same distance from the bottom as
//! before, so lines that become longer push the top into the scrollback and lines that
//! become shorter pull it back. Blank rows below the last content and the cursor do not
//! count as content; they are added again at the bottom. Every row gets a new sequence
//! number, so a renderer repaints all of them. Rows beyond the limit leave the scrollback
//! as when scrolling, and stable indexes of the kept rows are `old base + position`, which
//! is monotonic.
//!
//! The alternate screen is not reflowed.
//!
//! # Allocation
//!
//! Reflow is cold and may allocate. A row that is alone in its logical line and still fits
//! moves as it is, so a narrow-to-wide reflow of mostly short lines copies little.

use crate::cell::{Cell, Width};
use crate::screen::{Row, Screen};
use std::collections::VecDeque;
use std::mem;

/// A place on the primary screen that reflow keeps on the same character.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Point {
    /// Line position counted from the oldest scrollback row. After reflow, the new
    /// position in the same numbering.
    pub row: usize,
    pub col: usize,
    /// The cursor waits at the right edge to wrap (it sits on the last printed cell).
    /// After reflow, whether it waits at the new right edge.
    pub pending: bool,
}

impl Screen {
    /// Rewraps the lines of this screen to `cols` columns and fits the viewport to
    /// `rows`. `points` are tracked through the change; `anchor` indexes the point (the
    /// cursor) whose distance from the bottom of the viewport stays as it was, and whose
    /// row stays visible. Returns the position of the new first viewport row.
    pub(crate) fn reflow(
        &mut self,
        rows: usize,
        cols: usize,
        points: &mut [Point],
        anchor: Option<usize>,
    ) -> usize {
        let old_cols = self.cols;
        let old_rows = self.rows.len();
        let sb = self.scrollback.len();
        let mut old: Vec<Row> = Vec::with_capacity(sb + old_rows);
        old.extend(self.scrollback.drain(..));
        old.append(&mut self.rows);

        // Rows below the last content and every point are blank padding, not lines.
        let mut used = sb;
        for (i, r) in old.iter().enumerate().skip(sb) {
            if !r.cells.is_empty() || r.wrapped {
                used = i + 1;
            }
        }
        for p in points.iter() {
            used = used.max(p.row + 1);
        }
        used = used.min(old.len());
        // Rows between the cursor and the bottom of the old viewport.
        let below = anchor.map_or(0, |a| {
            old_rows.saturating_sub(1 + points[a].row.saturating_sub(sb))
        });
        old.truncate(used);

        // Indexes of the points in the order of their rows.
        let mut order: Vec<usize> = (0..points.len()).collect();
        order.sort_by_key(|&i| (points[i].row, points[i].col));
        let mut next_point = 0;

        let mut out: Vec<Row> = Vec::with_capacity(used + used / 4);
        let mut flat: Vec<Cell> = Vec::new();
        let mut starts: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < used {
            let mut j = i;
            while old[j].wrapped && j + 1 < used {
                j += 1;
            }
            let has_points = next_point < order.len() && points[order[next_point]].row <= j;
            if i == j && !has_points && old[i].cells.len() <= cols {
                let mut r = mem::take(&mut old[i]);
                // Room for a full line, so printing into it never grows it.
                r.cells.reserve_exact(cols.saturating_sub(r.cells.len()));
                r.wrapped = false;
                r.wrap_cols = 0;
                out.push(r);
                i += 1;
                continue;
            }
            // The text of the logical line.
            let text_cols = |r: &Row| {
                if r.wrap_cols != 0 {
                    (r.wrap_cols as usize).max(r.cells.len())
                } else {
                    old_cols.max(r.cells.len())
                }
            };
            let single = i == j;
            if !single {
                flat.clear();
                for (g, r) in old[i..=j].iter().enumerate() {
                    flat.extend_from_slice(&r.cells);
                    if i + g < j {
                        let want = flat.len() - r.cells.len() + text_cols(r);
                        flat.resize(want, Cell::BLANK);
                    }
                }
            }
            let src_all: &[Cell] = if single { &old[i].cells } else { &flat };
            let mut n = src_all.len();
            while n > 0 && src_all[n - 1] == Cell::BLANK {
                n -= 1;
            }
            let src = &src_all[..n];

            // Cut into rows.
            let first_out = out.len();
            starts.clear();
            let mut s = 0;
            loop {
                let mut e = (s + cols).min(n);
                if e < n && src[e - 1].width == Width::Wide {
                    e -= 1;
                }
                starts.push(s);
                if e == s && s < n {
                    // A wide character in a one column terminal cannot be shown. Keep
                    // its style as a blank.
                    let mut row = Row::default();
                    row.cells.reserve_exact(cols);
                    row.cells.push(Cell::blank(src[s].style));
                    s += 2;
                    row.wrapped = s < n;
                    out.push(row);
                } else {
                    let mut row = Row::default();
                    row.cells.reserve_exact(cols);
                    row.cells.extend_from_slice(&src[s..e]);
                    row.trim();
                    if e < n {
                        row.wrapped = true;
                        row.wrap_cols = if e - s < cols { (e - s) as u32 } else { 0 };
                    }
                    s = e;
                    out.push(row);
                }
                if s >= n {
                    break;
                }
            }
            let count = out.len() - first_out;

            // Track the points that sat in this logical line.
            let prefix =
                |g: usize| -> usize { old[i..i + g].iter().map(&text_cols).sum::<usize>() };
            while next_point < order.len() && points[order[next_point]].row <= j {
                let p = &mut points[order[next_point]];
                next_point += 1;
                let g = p.row - i;
                let mut col = p.col;
                if i + g < j {
                    col = col.min(text_cols(&old[i + g]).saturating_sub(1));
                }
                let o = prefix(g) + col;
                // Chunk holding offset `o`.
                let k = match starts.partition_point(|&st| st <= o) {
                    0 => 0,
                    k => k - 1,
                };
                let raw = if o >= n {
                    // Past the text: keep the distance from its end.
                    let last = count - 1;
                    (last, (n - starts[last]) + (o - n))
                } else {
                    (k, o - starts[k])
                };
                let (row_k, mut c) = raw;
                let was_pending = p.pending;
                if was_pending {
                    c += 1;
                }
                // A cursor that was waiting to wrap stays pending. One right at
                // the end of the last line's text may become pending, so printing
                // continues the line. One in blank space, or before rows with
                // text, is clamped to the last column: wrapping would overwrite
                // the next line.
                let at_end = o == n && j + 1 >= used;
                p.pending = (was_pending || at_end) && c >= cols;
                p.col = c.min(cols - 1);
                p.row = first_out + row_k;
            }
            i = j + 1;
        }

        // Which part of the lines is the viewport.
        let n = out.len();
        let top = match anchor {
            _ if n <= rows => 0,
            Some(a) => {
                let c = points[a].row.min(n.saturating_sub(1));
                let below = below.min(rows - 1);
                let want = c.saturating_sub(rows - 1 - below);
                // The cursor's line always stays visible. Lines that no longer fit
                // below the viewport are cut, as a row-only resize does.
                want.max(n.saturating_sub(rows)).min(c)
            }
            None => n.saturating_sub(rows),
        };
        let mut viewport = out.split_off(top);
        viewport.truncate(rows);
        while viewport.len() < rows {
            viewport.push(Row::default());
        }
        let popped = top.saturating_sub(self.limit);
        if popped > 0 {
            out.drain(..popped);
            self.base += popped as u64;
            self.dropped += popped as u64;
        }
        for r in out.iter_mut().chain(viewport.iter_mut()) {
            r.touch(&mut self.seq);
        }
        self.scrollback = VecDeque::from(out);
        self.rows = viewport;
        self.cols = cols;
        top
    }
}
