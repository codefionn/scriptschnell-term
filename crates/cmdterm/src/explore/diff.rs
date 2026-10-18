//! A diff, or a commit shown with its diff, prepared for drawing. Parsing
//! happens once on the loader's thread: every line gets its kind, its old
//! and new line numbers and the lexer state it starts in, so drawing a row
//! only slices.
//!
//! The input is git's extended diff format, which `git diff`, `git show`,
//! `hg diff --git` and `hg log -p --git` all print. Hunk line counts decide
//! where a hunk ends, so a removed line that starts with `--` never reads as
//! a file header.

use std::path::Path;

use crate::files::document::{MAX_HIGHLIGHT, push_line};
use crate::files::highlight::{self, Language, State};

/// Patches are read up to this many bytes; the rest is cut off.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;

/// [`Line::file`] of lines before the first file.
pub const NO_FILE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Commit header, message and file statistics.
    Text,
    /// `diff --git`: the first line of a file.
    File,
    /// Mode, index, rename and `---`/`+++` lines, binary notes.
    Meta,
    Hunk,
    Context,
    Added,
    Removed,
}

impl Kind {
    /// Lines that carry code, with line numbers and a `+`/`-` marker.
    pub fn is_code(self) -> bool {
        matches!(self, Kind::Context | Kind::Added | Kind::Removed)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Line {
    pub kind: Kind,
    /// Index into [`Diff::files`], or [`NO_FILE`].
    pub file: u32,
    /// Line numbers in the old and the new file, from 1; 0 where the line
    /// has none.
    pub old: u32,
    pub new: u32,
    /// The lexer state the line's code starts in.
    pub state: State,
}

pub struct DiffFile {
    /// The new path; the old one for a deleted file.
    pub path: String,
    /// `A`dded, `D`eleted, `R`enamed, `C`opied or `M`odified.
    pub status: char,
    pub added: u32,
    pub removed: u32,
    /// Index of the file's `diff --git` line.
    pub line: usize,
    pub language: Option<&'static Language>,
}

pub struct Diff {
    text: String,
    /// Byte offset where each line starts, then one past the end.
    starts: Vec<u32>,
    lines: Vec<Line>,
    pub files: Vec<DiffFile>,
    /// Cells of the widest line, without the `+`/`-` marker.
    pub width: usize,
    /// The patch was longer than [`MAX_BYTES`].
    pub truncated: bool,
    /// Digits of the largest line number.
    pub number_width: usize,
    /// Cells of the longest file name, without its folder: where the
    /// Review list starts its folder column.
    pub name_width: usize,
    pub added: u32,
    pub removed: u32,
}

/// The rest of the open hunk.
#[derive(Default)]
struct Hunk {
    old: u32,
    new: u32,
    old_left: u32,
    new_left: u32,
    /// Lexer states of the old and the new side.
    old_state: Option<State>,
    new_state: Option<State>,
}

impl Hunk {
    /// `@@ -a[,b] +c[,d] @@`.
    fn parse(line: &str) -> Option<Self> {
        let rest = line.strip_prefix("@@ -")?;
        let (old, rest) = rest.split_once(" +")?;
        let (new, _) = rest.split_once(" @@")?;
        let range = |text: &str| -> Option<(u32, u32)> {
            match text.split_once(',') {
                Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
                None => Some((text.parse().ok()?, 1)),
            }
        };
        let (old, old_left) = range(old)?;
        let (new, new_left) = range(new)?;
        Some(Self {
            old,
            new,
            old_left,
            new_left,
            old_state: Some(State::Normal),
            new_state: Some(State::Normal),
        })
    }

    /// The kind of `line` if it belongs to this hunk.
    fn kind(&self, line: &str) -> Option<Kind> {
        match line.as_bytes().first() {
            Some(b'+') if self.new_left > 0 => Some(Kind::Added),
            Some(b'-') if self.old_left > 0 => Some(Kind::Removed),
            Some(b' ') | None if self.old_left > 0 && self.new_left > 0 => Some(Kind::Context),
            Some(b'\\') => Some(Kind::Meta),
            _ => None,
        }
    }

    fn open(&self) -> bool {
        self.old_left > 0 || self.new_left > 0
    }
}

/// Lines between `diff --git` and the first hunk.
const META: [&str; 13] = [
    "index ",
    "old mode ",
    "new mode ",
    "new file mode ",
    "deleted file mode ",
    "similarity index ",
    "dissimilarity index ",
    "rename from ",
    "rename to ",
    "copy from ",
    "copy to ",
    "Binary files ",
    "\\",
];

impl Diff {
    pub fn new(mut bytes: Vec<u8>) -> Self {
        let truncated = bytes.len() > MAX_BYTES;
        bytes.truncate(MAX_BYTES);
        let source = String::from_utf8_lossy(&bytes);
        let source = source.strip_suffix('\n').unwrap_or(&source);
        let mut diff = Diff {
            text: String::with_capacity(source.len()),
            starts: Vec::new(),
            lines: Vec::new(),
            files: Vec::new(),
            width: 0,
            truncated,
            number_width: 1,
            name_width: 0,
            added: 0,
            removed: 0,
        };
        let mut hunk = Hunk::default();
        let mut binary = false;
        let mut old_path: Option<String> = None;
        let mut largest = 0u32;
        for raw in source.split('\n') {
            let raw = raw.strip_suffix('\r').unwrap_or(raw);
            let mut line = Line {
                kind: Kind::Text,
                file: NO_FILE,
                old: 0,
                new: 0,
                state: State::Normal,
            };
            let mut body = raw;
            match hunk
                .kind(raw)
                .filter(|_| hunk.open() || raw.starts_with('\\'))
            {
                Some(kind) => {
                    line.kind = kind;
                    if kind != Kind::Meta {
                        body = &raw[1..];
                    }
                    let file = diff.files.last_mut();
                    match kind {
                        Kind::Added => {
                            line.new = hunk.new;
                            hunk.new += 1;
                            hunk.new_left -= 1;
                            line.state = hunk.new_state.unwrap_or(State::Normal);
                            diff.added += 1;
                            if let Some(file) = file {
                                file.added += 1;
                            }
                        }
                        Kind::Removed => {
                            line.old = hunk.old;
                            hunk.old += 1;
                            hunk.old_left -= 1;
                            line.state = hunk.old_state.unwrap_or(State::Normal);
                            diff.removed += 1;
                            if let Some(file) = file {
                                file.removed += 1;
                            }
                        }
                        Kind::Context => {
                            line.old = hunk.old;
                            line.new = hunk.new;
                            hunk.old += 1;
                            hunk.new += 1;
                            hunk.old_left -= 1;
                            hunk.new_left -= 1;
                            line.state = hunk.new_state.unwrap_or(State::Normal);
                        }
                        _ => {}
                    }
                    largest = largest.max(line.old).max(line.new);
                }
                None => {
                    hunk = Hunk::default();
                    line.kind = diff.outside(raw, &mut hunk, &mut binary, &mut old_path);
                    // The file line above names the path and counts the
                    // changes, so git's index and ---/+++ lines repeat it.
                    if line.kind == Kind::Meta
                        && ["index ", "--- ", "+++ "]
                            .iter()
                            .any(|prefix| raw.starts_with(prefix))
                    {
                        continue;
                    }
                }
            }
            line.file = diff
                .files
                .len()
                .checked_sub(1)
                .map_or(NO_FILE, |i| i as u32);
            diff.starts.push(diff.text.len() as u32);
            let from = diff.text.len();
            diff.width = diff.width.max(push_line(&mut diff.text, body));
            diff.text.push('\n');
            // Advance the lexer of the side, or both sides, the line is on.
            if line.kind.is_code()
                && let Some(language) = diff.files.last().and_then(|file| file.language)
            {
                let code = &diff.text[from..diff.text.len() - 1];
                if code.len() <= MAX_HIGHLIGHT {
                    if line.kind != Kind::Added {
                        hunk.old_state = hunk
                            .old_state
                            .map(|state| highlight::advance(language, state, code));
                    }
                    if line.kind != Kind::Removed {
                        hunk.new_state = hunk
                            .new_state
                            .map(|state| highlight::advance(language, state, code));
                    }
                }
            }
            diff.lines.push(line);
        }
        diff.starts.push(diff.text.len() as u32);
        diff.number_width = crate::files::digits(largest as usize);
        diff.name_width = diff
            .files
            .iter()
            .map(|file| {
                let name = file.path.rsplit('/').next().unwrap_or(&file.path);
                unicode_width::UnicodeWidthStr::width(name)
            })
            .max()
            .unwrap_or(0);
        diff
    }

    /// Classifies a line outside any hunk and records what it says about
    /// the current file.
    fn outside(
        &mut self,
        raw: &str,
        hunk: &mut Hunk,
        binary: &mut bool,
        old_path: &mut Option<String>,
    ) -> Kind {
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            *binary = false;
            *old_path = None;
            self.files.push(DiffFile {
                path: header_path(rest).to_owned(),
                status: 'M',
                added: 0,
                removed: 0,
                line: self.lines.len(),
                language: None,
            });
            return Kind::File;
        }
        let Some(file) = self.files.last_mut() else {
            return Kind::Text;
        };
        if *binary {
            return Kind::Meta;
        }
        if raw.starts_with("@@ ")
            && let Some(parsed) = Hunk::parse(raw)
        {
            *hunk = parsed;
            file.language = highlight::detect(Path::new(&file.path), b"");
            if file.language.is_none() {
                hunk.old_state = None;
                hunk.new_state = None;
            }
            return Kind::Hunk;
        }
        if let Some(path) = raw.strip_prefix("--- ") {
            *old_path = path.strip_prefix("a/").map(str::to_owned);
            return Kind::Meta;
        }
        if let Some(path) = raw.strip_prefix("+++ ") {
            match path.strip_prefix("b/") {
                Some(path) => file.path = path.to_owned(),
                None => {
                    if let Some(old) = old_path.take() {
                        file.path = old;
                    }
                }
            }
            return Kind::Meta;
        }
        if raw.starts_with("GIT binary patch") {
            *binary = true;
            return Kind::Meta;
        }
        if !META.iter().any(|prefix| raw.starts_with(prefix)) {
            return Kind::Text;
        }
        if raw.starts_with("new file mode ") {
            file.status = 'A';
        } else if raw.starts_with("deleted file mode ") {
            file.status = 'D';
        } else if let Some(path) = raw.strip_prefix("rename to ") {
            file.status = 'R';
            file.path = path.to_owned();
        } else if let Some(path) = raw.strip_prefix("copy to ") {
            file.status = 'C';
            file.path = path.to_owned();
        }
        Kind::Meta
    }

    pub fn lines(&self) -> usize {
        self.lines.len()
    }

    /// Line `line` without its newline, and for code without its marker.
    pub fn line(&self, line: usize) -> &str {
        let from = self.starts[line] as usize;
        let to = self.starts[line + 1] as usize;
        self.text[from..to].trim_end_matches('\n')
    }

    pub fn info(&self, line: usize) -> Line {
        self.lines[line]
    }

    /// The file `line` belongs to.
    pub fn file(&self, line: usize) -> Option<&DiffFile> {
        self.files.get(self.lines.get(line)?.file as usize)
    }
}

/// The path of `a/X b/X`, the rest of a `diff --git` line. For a rename
/// the halves differ and the `+++` or `rename to` line fixes the path up.
fn header_path(rest: &str) -> &str {
    let half = rest.len().saturating_sub(5) / 2;
    if rest.len() >= 5
        && rest.len() % 2 == 1
        && rest.is_char_boundary(2 + half)
        && rest.starts_with("a/")
        && rest[2 + half..].starts_with(" b/")
        && rest[2..2 + half] == rest[5 + half..]
    {
        return &rest[2..2 + half];
    }
    match rest.rfind(" b/") {
        Some(at) => &rest[at + 3..],
        None => rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHOW: &str = "commit 1234
Author: test

    Rename and edit

diff --git a/src/old.rs b/src/new.rs
similarity index 90%
rename from src/old.rs
rename to src/new.rs
index 1..2 100644
--- a/src/old.rs
+++ b/src/new.rs
@@ -1,4 +1,4 @@ fn top()
 fn a() {
--- removed line that looks like a header
+++ added line that looks like a header
     /* open
@@ -10,2 +10,3 @@
 x
+y
 z
\\ No newline at end of file
diff --git a/with space.txt b/with space.txt
new file mode 100644
--- /dev/null
+++ b/with space.txt
@@ -0,0 +1 @@
+\tfresh
diff --git a/gone b/gone
deleted file mode 100644
--- a/gone
+++ /dev/null
@@ -1 +0,0 @@
-bye
";

    #[test]
    fn kinds_numbers_and_files() {
        let diff = Diff::new(SHOW.as_bytes().to_vec());
        let kinds: Vec<Kind> = (0..diff.lines()).map(|i| diff.info(i).kind).collect();
        use Kind::*;
        assert_eq!(
            kinds,
            [
                Text, Text, Text, Text, Text, File, Meta, Meta, Meta, Hunk, Context, Removed,
                Added, Context, Hunk, Context, Added, Context, Meta, File, Meta, Hunk, Added, File,
                Meta, Hunk, Removed,
            ]
        );
        assert_eq!(diff.line(11), "-- removed line that looks like a header");
        assert_eq!(diff.line(22), "    fresh");
        let names: Vec<(&str, char, u32, u32)> = diff
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
            .collect();
        assert_eq!(
            names,
            [
                ("src/new.rs", 'R', 2, 1),
                ("with space.txt", 'A', 1, 0),
                ("gone", 'D', 0, 1)
            ]
        );
        assert_eq!(diff.files[1].line, 19);
        assert_eq!((diff.added, diff.removed), (3, 2));
        let line = diff.info(16);
        assert_eq!((line.old, line.new), (0, 11));
        let line = diff.info(17);
        assert_eq!((line.old, line.new), (11, 12));
        assert_eq!(diff.number_width, 2);
        assert_eq!(diff.file(11).map(|f| f.path.as_str()), Some("src/new.rs"));
        assert!(diff.file(2).is_none());
        // The block comment opened on the new side carries into the next line.
        assert_eq!(diff.info(13).state, State::Normal);
        assert_eq!(diff.files[0].language.map(|l| l.name), Some("Rust"));
    }

    #[test]
    fn header_paths() {
        assert_eq!(header_path("a/x b/x"), "x");
        assert_eq!(header_path("a/a b/c b/a b/c"), "a b/c");
        assert_eq!(header_path("a/old b/new"), "new");
        assert_eq!(Diff::new(Vec::new()).lines(), 1);
    }
}
