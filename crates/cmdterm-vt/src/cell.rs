//! The cell: one grapheme cluster, a style id and a width flag.
//!
//! A cell is 12 bytes. It holds the UTF-8 bytes of its first character inline, so a
//! cell with one character reads as text without any table. A cell whose cluster has
//! more than one character also carries a nonzero [`ClusterId`] into the terminal's
//! cluster table (`grapheme.rs`), which holds the full text.

use crate::style::StyleId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Width {
    Single,
    /// The left half of a double-width character.
    Wide,
    /// The right half of a double-width character. Its text is a space.
    Continuation,
}

/// Index into the cluster table. 0 means the cell holds one character.
pub type ClusterId = u16;

/// One screen cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// UTF-8 of the first character, zero padded.
    text: [u8; 4],
    pub style: StyleId,
    cluster: ClusterId,
    len: u8,
    pub width: Width,
}

impl Cell {
    /// A space in the default style. Rows do not store trailing cells equal to this.
    pub const BLANK: Cell = Cell::blank(StyleId::DEFAULT);

    /// A space in `style` (erased cells keep the pen's colors, not its attributes).
    pub const fn blank(style: StyleId) -> Cell {
        Cell {
            text: [b' ', 0, 0, 0],
            style,
            cluster: 0,
            len: 1,
            width: Width::Single,
        }
    }

    /// A single-width cell with the ASCII character `b`.
    pub(crate) const fn ascii(b: u8, style: StyleId) -> Cell {
        debug_assert!(b.is_ascii());
        Cell {
            text: [b, 0, 0, 0],
            style,
            cluster: 0,
            len: 1,
            width: Width::Single,
        }
    }

    /// A cell with one character.
    pub fn new(ch: char, style: StyleId, width: Width) -> Cell {
        let mut text = [0; 4];
        let len = ch.encode_utf8(&mut text).len() as u8;
        Cell {
            text,
            style,
            cluster: 0,
            len,
            width,
        }
    }

    /// The same cell, now standing for the cluster `id` of the cluster table. The
    /// cell keeps its first character.
    pub(crate) fn with_cluster(mut self, id: ClusterId) -> Cell {
        self.cluster = id;
        self
    }

    /// The first character of the cell's text.
    pub fn ch(&self) -> char {
        self.first().chars().next().unwrap_or(' ')
    }

    /// The cluster table id, or 0 when the cell holds one character.
    pub fn cluster(&self) -> ClusterId {
        self.cluster
    }

    /// The first character as text. Does not allocate. For a cluster cell use
    /// `Terminal::cell_text`, which returns the whole cluster.
    pub fn first(&self) -> &str {
        std::str::from_utf8(&self.text[..self.len as usize]).unwrap_or(" ")
    }
}
