//! Grapheme clusters: the table behind multi-character cells, and the rules that decide
//! whether a character continues the cluster before it and how wide a cluster is.
//!
//! # Model
//!
//! A cell with a single character keeps it inline and has cluster id 0. A cluster of
//! more than one character (a base with combining marks, a ZWJ sequence, a flag, a
//! skin tone, Hangul jamo) is interned here once and cells refer to it by id. Ids are
//! 16 bits, so a cell stays 12 bytes. Id 0 is reserved.
//!
//! The table is bounded by [`MAX_CLUSTERS`]. When it is full the terminal collects the
//! ids no cell refers to any more ([`ClusterTable::sweep`]) and reuses them. If every id
//! is still in use, further extensions are dropped and the cell keeps its text.
//!
//! # Segmentation
//!
//! The terminal does not segment a stream. For each printed character it joins the text
//! of the cell left of the cursor with the character and asks whether the result is one
//! extended grapheme cluster ([`is_one_cluster`]). That is position based, like xterm,
//! and needs no state between characters.
//!
//! # Widths
//!
//! [`cluster_width`] is the widest `unicode-width` character of the cluster (1 or 2), with
//! VS16 (U+FE0F) forcing 2, VS15 (U+FE0E) forcing 1, and a pair of regional indicators
//! counting 2. The width of the whole string is not used: it adds spacing marks and
//! emoji modifiers up, which would make a Devanagari syllable two columns wide.

use crate::cell::ClusterId;
use rustc_hash::FxHashMap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// Most clusters a table holds (ids `1..=MAX_CLUSTERS`).
pub const MAX_CLUSTERS: usize = u16::MAX as usize;

/// Longest cluster text kept, in bytes. A longer sequence (stacked combining marks
/// used as noise) stops growing and drops the rest.
pub const MAX_CLUSTER_BYTES: usize = 96;

/// Interns the text of multi-character clusters.
#[derive(Debug, Clone)]
pub struct ClusterTable {
    /// Slot 0 is unused. A freed slot holds an empty string.
    texts: Vec<Box<str>>,
    ids: FxHashMap<Box<str>, ClusterId>,
    free: Vec<ClusterId>,
}

impl Default for ClusterTable {
    fn default() -> Self {
        ClusterTable {
            texts: vec![Box::from("")],
            ids: FxHashMap::default(),
            free: Vec::new(),
        }
    }
}

impl ClusterTable {
    /// The text of cluster `id`; empty for id 0 or an unused id. Does not allocate.
    pub fn get(&self, id: ClusterId) -> &str {
        self.texts.get(id as usize).map_or("", |s| s)
    }

    /// Number of ids in use.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Number of slots, used ids or not, including slot 0. A mark slice for
    /// [`ClusterTable::sweep`] has this length.
    pub fn slots(&self) -> usize {
        self.texts.len()
    }

    /// The id of `text`, adding it if new. `None` when the table is full or `text` is
    /// longer than [`MAX_CLUSTER_BYTES`]. Looking up a known cluster does not allocate.
    pub fn intern(&mut self, text: &str) -> Option<ClusterId> {
        if text.len() > MAX_CLUSTER_BYTES {
            return None;
        }
        if let Some(&id) = self.ids.get(text) {
            return Some(id);
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None if self.texts.len() <= MAX_CLUSTERS => {
                self.texts.push(Box::from(""));
                (self.texts.len() - 1) as ClusterId
            }
            None => return None,
        };
        let boxed: Box<str> = Box::from(text);
        self.texts[id as usize] = boxed.clone();
        self.ids.insert(boxed, id);
        Some(id)
    }

    /// Frees every id whose `marks` entry is false. `marks` has [`ClusterTable::slots`]
    /// entries; the caller marks the ids that cells still refer to.
    pub fn sweep(&mut self, marks: &[bool]) {
        for id in 1..self.texts.len() {
            if !marks.get(id).copied().unwrap_or(false) && !self.texts[id].is_empty() {
                let text = std::mem::take(&mut self.texts[id]);
                self.ids.remove(&text);
                self.free.push(id as ClusterId);
            }
        }
    }
}

/// True when `text` is exactly one extended grapheme cluster. Does not allocate.
pub fn is_one_cluster(text: &str) -> bool {
    let mut it = text.graphemes(true);
    it.next().is_some() && it.next().is_none()
}

/// Columns a cluster takes: 1 or 2. See the module docs.
pub fn cluster_width(text: &str) -> usize {
    if text.contains('\u{FE0F}') {
        return 2;
    }
    if text.contains('\u{FE0E}') {
        return 1;
    }
    let mut widest = 1;
    let mut indicators = 0;
    for c in text.chars() {
        widest = widest.max(c.width().unwrap_or(0));
        indicators += ('\u{1F1E6}'..='\u{1F1FF}').contains(&c) as usize;
    }
    if indicators >= 2 { 2 } else { widest.min(2) }
}

/// True when `c` certainly continues the cluster `text`, decided without segmentation
/// for the common cases: a combining mark, variation selector, zero width joiner or
/// emoji modifier (all extend any cluster, GB9), and an emoji after a joiner that follows
/// an emoji (GB11). `text` is never empty and never starts with a control. A false
/// answer means "ask [`is_one_cluster`]".
pub(crate) fn surely_joins(text: &str, c: char) -> bool {
    match c as u32 {
        0x300..=0x36F | 0x200D | 0xFE00..=0xFE0F | 0x1F3FB..=0x1F3FF => true,
        0x1F300..=0x1FAFF => {
            // GB11: ExtPict Extend* ZWJ x ExtPict. Walk the cluster as that state
            // machine; anything else asks `is_one_cluster`.
            let pict = |ch: char| {
                matches!(ch as u32, 0x1F300..=0x1FAFF) && !matches!(ch as u32, 0x1F3FB..=0x1F3FF)
            };
            // 0 = after ExtPict or Extend, 1 = right after the ZWJ.
            let mut after_zwj = false;
            let mut chars = text.chars();
            if !chars.next().is_some_and(pict) {
                return false;
            }
            for ch in chars {
                match ch as u32 {
                    0x200D if !after_zwj => after_zwj = true,
                    0x300..=0x36F | 0xFE00..=0xFE0F | 0x1F3FB..=0x1F3FF if !after_zwj => {}
                    _ if after_zwj && pict(ch) => after_zwj = false,
                    _ => return false,
                }
            }
            after_zwj
        }
        _ => false,
    }
}

/// True for characters that neither extend a cluster nor are extended by one that
/// starts with a single plain character: Latin below the combining marks, CJK
/// ideographs, Hangul syllables and the emoji blocks without modifiers and regional
/// indicators. Two plain characters in a row are always two clusters, so the terminal
/// skips segmentation for them.
pub(crate) fn is_plain(ch: char) -> bool {
    matches!(ch as u32,
        0..=0x2FF
        | 0x3400..=0x9FFF
        | 0xAC00..=0xD7A3
        | 0x1F300..=0x1F3FA
        | 0x1F400..=0x1FAFF)
}

/// True for the characters that can start a cluster that a following ASCII character
/// would join (the Prepend class starts at U+0600).
pub(crate) fn may_prepend(ch: char) -> bool {
    ch as u32 >= 0x600
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_is_stable_and_sweep_frees_unmarked_ids() {
        let mut t = ClusterTable::default();
        let a = t.intern("e\u{301}").unwrap();
        let b = t.intern("a\u{300}").unwrap();
        assert_ne!(a, b);
        assert_eq!(t.intern("e\u{301}"), Some(a));
        assert_eq!(t.get(a), "e\u{301}");
        let mut marks = vec![false; t.slots()];
        marks[b as usize] = true;
        t.sweep(&marks);
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(a), "");
        assert_eq!(t.get(b), "a\u{300}");
        // The freed id is reused.
        assert_eq!(t.intern("o\u{308}"), Some(a));
    }

    #[test]
    fn long_clusters_are_refused() {
        let mut t = ClusterTable::default();
        let long = format!("a{}", "\u{301}".repeat(MAX_CLUSTER_BYTES));
        assert_eq!(t.intern(&long), None);
    }

    #[test]
    fn full_table_refuses_until_swept() {
        let mut t = ClusterTable::default();
        for i in 0..MAX_CLUSTERS {
            let s = format!("{i}\u{301}");
            assert!(t.intern(&s).is_some());
        }
        assert_eq!(t.intern("x\u{301}"), None);
        t.sweep(&vec![false; t.slots()]);
        assert!(t.intern("x\u{301}").is_some());
    }

    #[test]
    fn widths() {
        assert_eq!(cluster_width("e\u{301}"), 1);
        assert_eq!(cluster_width("\u{1F1E9}\u{1F1EA}"), 2);
        assert_eq!(cluster_width("\u{2764}\u{FE0F}"), 2);
        assert_eq!(cluster_width("\u{1F600}\u{FE0E}"), 1);
        assert_eq!(cluster_width("\u{1100}\u{1161}\u{11A8}"), 2);
        assert_eq!(cluster_width("\u{915}\u{93E}"), 1);
        assert!(is_one_cluster("\u{1F468}\u{200D}\u{1F469}"));
        assert!(!is_one_cluster("ab"));
    }
}
