//! Types and helpers for the OSCs the terminal understands: color specs, base64, file
//! URLs, the hyperlink table and the semantic prompt marks.
//!
//! Everything here is pure data handling. The terminal state that uses it lives in
//! `terminal/osc_handlers.rs`.

use crate::color::Rgb;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::mem;

/// The directory a program reported with OSC 7 (or iTerm's `CurrentDir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentDir {
    /// The host part of the `file://` URL; empty for `file:///path`.
    pub host: String,
    /// The path, percent-decoded.
    pub path: String,
}

/// The state ConEmu's OSC 9;4 and Windows Terminal's progress reports describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// No progress; the indicator goes away.
    None,
    /// A percentage from 0 to 100.
    Percentage(u8),
    /// An error, with the percentage reached.
    Error(u8),
    /// Work of unknown length.
    Indeterminate,
    /// Paused, with the percentage reached.
    Paused(u8),
}

/// What a stretch of the screen holds, from OSC 133.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticKind {
    /// The prompt text (mark `A`).
    Prompt,
    /// What the user types (mark `B`).
    Input,
    /// What the command prints (mark `C`). Text before the first mark counts as output.
    Output,
}

/// A run of cells of one [`SemanticKind`]. Rows are stable indexes; both ends are
/// inclusive, like wezterm's `SemanticZone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticZone {
    pub kind: SemanticKind,
    pub start_row: u64,
    pub start_col: usize,
    pub end_row: u64,
    pub end_col: usize,
    /// The `aid=` parameter of the mark that opened the zone.
    pub aid: Option<String>,
    /// The exit status from the `D` mark that ended an output zone.
    pub exit_status: Option<i32>,
}

#[derive(Debug, Clone)]
pub(crate) struct Mark {
    pub row: u64,
    pub col: usize,
    pub kind: SemanticKind,
    pub aid: Option<Box<str>>,
    pub exit: Option<i32>,
}

/// Most marks kept; the oldest go first.
const MAX_MARKS: usize = 4096;

/// Prompt marks in ascending position order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Marks {
    marks: VecDeque<Mark>,
}

impl Marks {
    /// Adds a mark. Marks after the new position are stale (the screen was cleared or the
    /// shell redrew its prompt), so they go.
    pub fn push(&mut self, mark: Mark) {
        while self
            .marks
            .back()
            .is_some_and(|m| (m.row, m.col) > (mark.row, mark.col))
        {
            self.marks.pop_back();
        }
        if self.marks.len() >= MAX_MARKS {
            self.marks.pop_front();
        }
        self.marks.push_back(mark);
    }

    /// Records the exit status of the command whose output came last.
    pub fn set_exit(&mut self, status: i32) {
        // Only an output mark after the latest prompt belongs to this command. A command
        // that never started (empty line: A, B, D) has none, and the status is dropped.
        for m in self.marks.iter_mut().rev() {
            match m.kind {
                SemanticKind::Prompt => return,
                SemanticKind::Output => {
                    m.exit = Some(status);
                    return;
                }
                SemanticKind::Input => {}
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Mark> {
        self.marks.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Mark> {
        self.marks.iter_mut()
    }

    /// Drops the marks of lines older than `first`, the oldest line kept. The newest of
    /// them stays, moved to the start of `first`: it still says what kind of text the
    /// oldest kept lines are (a command's output whose prompt scrolled away). Does not
    /// allocate.
    pub fn prune(&mut self, first: u64) {
        while self.marks.get(1).is_some_and(|m| m.row < first) {
            self.marks.pop_front();
        }
        if let Some(m) = self.marks.front_mut()
            && m.row < first
        {
            m.row = first;
            m.col = 0;
        }
    }

    pub fn len(&self) -> usize {
        self.marks.len()
    }
}

/// Longest URI kept.
pub(crate) const MAX_URI: usize = 8192;
/// Most distinct hyperlinks kept, and most bytes their text may take.
const MAX_LINKS: usize = 1 << 16;
const MAX_LINK_BYTES: usize = 16 << 20;

#[derive(Debug, Clone)]
struct Link {
    uri: Box<str>,
    id: Option<Box<str>>,
}

/// Interns hyperlinks. Id 0 means no link; a cell reaches its link through the style's
/// `hyperlink` field, so cells stay small.
#[derive(Debug, Clone, Default)]
pub(crate) struct LinkTable {
    links: Vec<Link>,
    /// Link id by hash of (id, uri). A collision with a different link probes the next
    /// hash value, so a lookup needs no owned key and never allocates.
    index: FxHashMap<u64, u32>,
    bytes: usize,
}

fn link_hash(id: Option<&str>, uri: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    id.hash(&mut h);
    uri.hash(&mut h);
    h.finish()
}

impl LinkTable {
    /// Returns the id of the link, adding it when new. `None` when the table is full or
    /// the URI is too long.
    pub fn intern(&mut self, id: Option<&str>, uri: &str) -> Option<u32> {
        if uri.len() > MAX_URI {
            return None;
        }
        let mut h = link_hash(id, uri);
        while let Some(&n) = self.index.get(&h) {
            let l = &self.links[n as usize - 1];
            if &*l.uri == uri && l.id.as_deref() == id {
                return Some(n);
            }
            h = h.wrapping_add(1);
        }
        let size = uri.len() + id.map_or(0, str::len);
        if self.links.len() >= MAX_LINKS || self.bytes + size > MAX_LINK_BYTES {
            return None;
        }
        self.bytes += size;
        self.links.push(Link {
            uri: uri.into(),
            id: id.map(Into::into),
        });
        let n = self.links.len() as u32;
        self.index.insert(h, n);
        Some(n)
    }

    /// Number of links kept.
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Bytes of link text kept.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Keeps the links whose `live` entry is true (`live[n]` for link id `n`, entry 0
    /// unused), renumbers them in their old order, and writes the old-to-new id map to
    /// `remap` (0 for a dropped link). Frees their slots and bytes. Cold.
    pub fn compact(&mut self, live: &[bool], remap: &mut Vec<u32>) {
        remap.clear();
        remap.resize(self.links.len() + 1, 0);
        let old = mem::take(&mut self.links);
        self.index.clear();
        self.bytes = 0;
        for (i, link) in old.into_iter().enumerate() {
            if !live.get(i + 1).copied().unwrap_or(false) {
                continue;
            }
            self.bytes += link.uri.len() + link.id.as_deref().map_or(0, str::len);
            let n = self.links.len() as u32 + 1;
            remap[i + 1] = n;
            let mut h = link_hash(link.id.as_deref(), &link.uri);
            while self.index.contains_key(&h) {
                h = h.wrapping_add(1);
            }
            self.index.insert(h, n);
            self.links.push(link);
        }
    }

    pub fn uri(&self, n: u32) -> Option<&str> {
        let i = (n as usize).checked_sub(1)?;
        self.links.get(i).map(|l| &*l.uri)
    }

    pub fn id(&self, n: u32) -> Option<&str> {
        let i = (n as usize).checked_sub(1)?;
        self.links.get(i)?.id.as_deref()
    }
}

/// Parses an X11 color: `rgb:R/G/B` with one to four hex digits per part, or `#RGB`,
/// `#RRGGBB`, `#RRRGGGBBB`, `#RRRRGGGGBBBB`.
pub fn parse_color(spec: &[u8]) -> Option<Rgb> {
    let s = std::str::from_utf8(spec).ok()?;
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut it = rest.split('/');
        let (r, g, b) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        return Some(Rgb::new(scale(r)?, scale(g)?, scale(b)?));
    }
    let hex = s.strip_prefix('#')?;
    if hex.is_empty() || hex.len() % 3 != 0 || hex.len() > 12 || !hex.is_ascii() {
        return None;
    }
    let n = hex.len() / 3;
    Some(Rgb::new(
        scale(&hex[..n])?,
        scale(&hex[n..2 * n])?,
        scale(&hex[2 * n..])?,
    ))
}

/// Scales one to four hex digits to 8 bits, keeping the high bits as xterm does for
/// longer values and repeating the digit for a single one.
fn scale(part: &str) -> Option<u8> {
    if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(part, 16).ok()?;
    Some(match part.len() {
        1 => (v * 17) as u8,
        2 => v as u8,
        3 => (v >> 4) as u8,
        _ => (v >> 8) as u8,
    })
}

/// Formats a color the way xterm answers queries: `rgb:RRRR/GGGG/BBBB`.
pub fn format_color(c: Rgb) -> String {
    let w = |v: u8| u16::from(v) * 0x101;
    format!("rgb:{:04x}/{:04x}/{:04x}", w(c.r), w(c.g), w(c.b))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decodes standard base64. Padding is optional; ASCII whitespace is skipped. Anything
/// else outside the alphabet makes the whole input invalid.
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut pad = false;
    for &b in input {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        if pad {
            return None;
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // A single leftover sextet cannot hold a byte.
    if bits >= 6 { None } else { Some(out) }
}

/// Encodes standard base64 with padding.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Replaces `%XX` escapes. Invalid escapes stay as they are; invalid UTF-8 becomes U+FFFD.
pub fn percent_decode(s: &[u8]) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && i + 2 < s.len()
            && let (Some(h), Some(l)) = (hex(s[i + 1]), hex(s[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses the `file://host/path` URL of OSC 7.
pub fn parse_file_url(url: &[u8]) -> Option<CurrentDir> {
    let rest = url.strip_prefix(b"file://")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let host = String::from_utf8_lossy(&rest[..slash]).into_owned();
    let path = percent_decode(&rest[slash..]);
    Some(CurrentDir { host, path })
}
