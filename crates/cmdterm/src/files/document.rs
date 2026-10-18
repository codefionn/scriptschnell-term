//! A loaded file, prepared for drawing: tabs expanded, control characters
//! replaced, line starts and the lexer state of every line recorded. All of
//! it happens once on the loader's thread, so drawing a row only slices.

use std::path::Path;

use unicode_width::UnicodeWidthChar;

use super::highlight::{self, Language, State};

/// Files are read up to this many bytes; the rest is cut off.
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Columns a tab advances to the next multiple of.
const TAB: usize = 4;
/// A NUL byte this early marks a binary file.
const SNIFF: usize = 8 * 1024;

pub struct Document {
    text: String,
    /// Byte offset where each line starts, then one past the end.
    starts: Vec<u32>,
    states: Vec<State>,
    pub language: Option<&'static Language>,
    /// The file is longer than [`MAX_BYTES`].
    pub truncated: bool,
    /// Cells of the widest line.
    pub width: usize,
}

/// What reading a file produced.
pub enum Loaded {
    Text(Document),
    Binary,
}

impl Loaded {
    /// Prepares `bytes`, read from `path` with a limit of one byte more than
    /// [`MAX_BYTES`].
    pub fn new(path: &Path, mut bytes: Vec<u8>) -> Self {
        if bytes[..bytes.len().min(SNIFF)].contains(&0) {
            return Self::Binary;
        }
        let truncated = bytes.len() as u64 > MAX_BYTES;
        bytes.truncate(MAX_BYTES as usize);
        let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
        let language = highlight::detect(path, first);
        let source = String::from_utf8_lossy(&bytes);

        let mut text = String::with_capacity(source.len());
        let mut starts = vec![0u32];
        let mut width = 0;
        for (index, line) in source.split('\n').enumerate() {
            if index > 0 {
                text.push('\n');
                starts.push(text.len() as u32);
            }
            width = width.max(push_line(&mut text, line));
        }
        // A final newline ends the last line rather than starting another.
        if text.ends_with('\n') {
            starts.pop();
        }
        starts.push(text.len() as u32);

        let lines = starts.len() - 1;
        let mut states = Vec::with_capacity(lines);
        let mut state = State::Normal;
        for line in 0..lines {
            states.push(state);
            if let Some(language) = language {
                let line = line_of(&text, &starts, line);
                if line.len() <= MAX_HIGHLIGHT {
                    state = highlight::advance(language, state, line);
                }
            }
        }
        Self::Text(Document {
            text,
            starts,
            states,
            language,
            truncated,
            width,
        })
    }
}

/// Appends `line`, which holds no newline, to `text` with tabs expanded and
/// control characters replaced. Returns the cells it covers.
pub fn push_line(text: &mut String, line: &str) -> usize {
    let mut column = 0;
    for ch in line.chars() {
        match ch {
            '\r' => {}
            '\t' => {
                let next = (column / TAB + 1) * TAB;
                text.extend(std::iter::repeat_n(' ', next - column));
                column = next;
            }
            ch if ch.is_control() => {
                text.push('\u{fffd}');
                column += 1;
            }
            ch => {
                text.push(ch);
                column += ch.width().unwrap_or(0);
            }
        }
    }
    column
}

/// Longer lines are drawn without highlighting, so a minified file costs no
/// more per row than its visible part.
pub const MAX_HIGHLIGHT: usize = 8 * 1024;

fn line_of<'a>(text: &'a str, starts: &[u32], line: usize) -> &'a str {
    let from = starts[line] as usize;
    let to = starts[line + 1] as usize;
    text[from..to].trim_end_matches('\n')
}

impl Document {
    pub fn lines(&self) -> usize {
        self.starts.len() - 1
    }

    /// Line `line` without its newline.
    pub fn line(&self, line: usize) -> &str {
        line_of(&self.text, &self.starts, line)
    }

    /// The lexer state line `line` starts in.
    pub fn state(&self, line: usize) -> State {
        self.states[line]
    }
}

/// The part of `line` that covers display columns `from..to`. A wide char
/// that straddles either edge is left out.
pub fn columns(line: &str, from: usize, to: usize) -> &str {
    let mut column = 0;
    let mut start = line.len();
    let mut end = line.len();
    for (index, ch) in line.char_indices() {
        if column >= to {
            end = index;
            break;
        }
        if column >= from && start == line.len() {
            start = index;
        }
        column += ch.width().unwrap_or(0);
    }
    if start > end {
        return "";
    }
    &line[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8]) -> Document {
        match Loaded::new(Path::new("a.rs"), bytes.to_vec()) {
            Loaded::Text(document) => document,
            Loaded::Binary => panic!("binary"),
        }
    }

    #[test]
    fn lines_tabs_and_control_characters() {
        let doc = text(b"fn a() {\r\n\tx\x07\n}\n");
        assert_eq!(doc.lines(), 3);
        assert_eq!(doc.line(0), "fn a() {");
        assert_eq!(doc.line(1), "    x\u{fffd}");
        assert_eq!(doc.line(2), "}");
        assert_eq!(doc.width, 8);
        assert_eq!(doc.language.map(|l| l.name), Some("Rust"));
        assert_eq!(text(b"").lines(), 1);
        assert_eq!(text(b"no newline").lines(), 1);
    }

    #[test]
    fn states_record_where_each_line_starts() {
        let doc = text(b"a /* one\ntwo */ b\nc");
        assert_eq!(doc.state(0), State::Normal);
        assert_eq!(doc.state(1), State::Comment(1));
        assert_eq!(doc.state(2), State::Normal);
    }

    #[test]
    fn nul_bytes_mean_binary() {
        let loaded = Loaded::new(Path::new("x.bin"), b"PK\x03\x04\0\0".to_vec());
        assert!(matches!(loaded, Loaded::Binary));
    }

    #[test]
    fn column_slices_respect_wide_chars() {
        assert_eq!(columns("abcdef", 2, 4), "cd");
        assert_eq!(columns("abc", 1, 99), "bc");
        assert_eq!(columns("abc", 5, 9), "");
        assert_eq!(columns("日本語", 2, 4), "本");
    }
}
