//! Cell styles and the per-terminal table that interns them.

use crate::color::Color;
use rustc_hash::FxHashMap;

/// Bits of [`Style::flags`].
pub mod flags {
    pub const BOLD: u8 = 1;
    pub const FAINT: u8 = 2;
    pub const ITALIC: u8 = 4;
    pub const BLINK: u8 = 8;
    pub const INVERSE: u8 = 16;
    pub const INVISIBLE: u8 = 32;
    pub const STRIKE: u8 = 64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Everything that decides how a cell looks besides its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Color,
    pub flags: u8,
    pub underline: Underline,
    /// Hyperlink id, 0 for none. Set by OSC 8; the terminal maps it to the URI.
    pub hyperlink: u32,
}

impl Style {
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    pub fn set(&mut self, flag: u8, on: bool) {
        if on {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
}

/// Index into a [`StyleTable`]. Id 0 is always the default style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StyleId(pub u32);

impl StyleId {
    pub const DEFAULT: StyleId = StyleId(0);
}

/// Most distinct styles a table keeps. Past this, new styles fall back to the default
/// style instead of growing without bound. The terminal collects the styles no cell uses
/// long before that (see [`StyleTable::compact`]).
pub const MAX_STYLES: usize = 1 << 20;

/// Interns styles so a cell carries a small id.
#[derive(Debug, Clone)]
pub struct StyleTable {
    styles: Vec<Style>,
    ids: FxHashMap<Style, StyleId>,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTable {
    pub fn new() -> Self {
        let mut ids = FxHashMap::default();
        ids.insert(Style::default(), StyleId::DEFAULT);
        Self {
            styles: vec![Style::default()],
            ids,
        }
    }

    /// Returns the id of `style`, adding it when new. Allocates only for a style never
    /// seen before.
    pub fn intern(&mut self, style: &Style) -> StyleId {
        if let Some(&id) = self.ids.get(style) {
            return id;
        }
        if self.styles.len() >= MAX_STYLES {
            return StyleId::DEFAULT;
        }
        let id = StyleId(self.styles.len() as u32);
        self.styles.push(*style);
        self.ids.insert(*style, id);
        id
    }

    pub fn get(&self, id: StyleId) -> &Style {
        self.styles.get(id.0 as usize).unwrap_or(&self.styles[0])
    }

    pub fn len(&self) -> usize {
        self.styles.len()
    }

    /// Keeps the styles whose `live` entry is true (and the default style), renumbers
    /// them in their old order and writes the old-to-new id map to `remap`. A style's
    /// hyperlink id goes through `link_map` (old link id to new, index 0 unused).
    /// `live` and `remap` have [`StyleTable::len`] entries. Cold: rebuilds the index.
    pub(crate) fn compact(&mut self, live: &[bool], link_map: &[u32], remap: &mut Vec<u32>) {
        remap.clear();
        remap.resize(self.styles.len(), 0);
        self.ids.clear();
        let mut kept = 1;
        self.ids.insert(self.styles[0], StyleId::DEFAULT);
        for (i, slot) in remap.iter_mut().enumerate().skip(1) {
            if !live.get(i).copied().unwrap_or(false) {
                continue;
            }
            let mut st = self.styles[i];
            st.hyperlink = link_map.get(st.hyperlink as usize).copied().unwrap_or(0);
            self.styles[kept] = st;
            self.ids.insert(st, StyleId(kept as u32));
            *slot = kept as u32;
            kept += 1;
        }
        self.styles.truncate(kept);
    }

    pub fn is_empty(&self) -> bool {
        false
    }
}
