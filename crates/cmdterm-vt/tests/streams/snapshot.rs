//! A comparable view of a screen.

use cmdterm_vt::{Rgb, Terminal, Width, flags};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strike: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellSnap {
    pub text: String,
    /// 1 or 2. A continuation cell repeats the width of its wide cell.
    pub width: u8,
    pub continuation: bool,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: Attrs,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub rows: Vec<Vec<CellSnap>>,
    /// Row and column.
    pub cursor: (usize, usize),
}

pub fn of_cmdterm(t: &Terminal) -> Snapshot {
    let size = t.size();
    let pal = t.palette();
    let mut rows = Vec::with_capacity(size.rows);
    for y in 0..size.rows {
        let row = t.row(y).unwrap();
        let mut out = Vec::with_capacity(size.cols);
        for x in 0..size.cols {
            let c = row.get(x);
            let s = t.style(c.style);
            out.push(CellSnap {
                text: t.cell_text(&c).to_string(),
                width: if c.width == Width::Single { 1 } else { 2 },
                continuation: c.width == Width::Continuation,
                fg: pal.resolve(s.fg, true),
                bg: pal.resolve(s.bg, false),
                attrs: Attrs {
                    bold: s.has(flags::BOLD),
                    faint: s.has(flags::FAINT),
                    italic: s.has(flags::ITALIC),
                    underline: s.underline != cmdterm_vt::Underline::None,
                    inverse: s.has(flags::INVERSE),
                    invisible: s.has(flags::INVISIBLE),
                    strike: s.has(flags::STRIKE),
                },
            });
        }
        rows.push(out);
    }
    let c = t.cursor();
    Snapshot {
        rows,
        cursor: (c.row, c.col),
    }
}
