//! The app's log: the errors, warnings and notices of this run and the
//! ones before it. The Logs page in the settings lists, searches and copies
//! them. Each entry is one line in `scriptschnell-term.log` in the logs
//! directory (`AppDirs::logs`), so a crash or a closed window keeps what
//! happened before it.
//! The file moves to `scriptschnell-term.log.1` once it grows past
//! [`MAX_FILE`], so the two never take more than twice that.
//!
//! Recording takes a lock and writes one line, so it is cold: it runs when
//! something fails or a notice shows, never per frame or per PTY chunk. An
//! entry that repeats the one before it raises that one's count instead of
//! adding another, and the file gets one line for the whole run of them.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Name of the log file in the logs directory.
pub const FILE_NAME: &str = "scriptschnell-term.log";
/// Size past which the log file moves aside and a new one starts.
pub const MAX_FILE: u64 = 1024 * 1024;
/// Most entries kept in memory, the newest ones.
pub const CAPACITY: usize = 2000;

/// How bad an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    /// The word the log file writes for this level.
    pub fn word(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warning => "WARN",
            Level::Error => "ERROR",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "INFO" => Some(Level::Info),
            "WARN" => Some(Level::Warning),
            "ERROR" => Some(Level::Error),
            _ => None,
        }
    }
}

/// One thing that happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Counts up from 1 across the log, so the Logs page can name the entry
    /// it shows in full.
    pub id: u64,
    /// When it last happened.
    pub at: SystemTime,
    pub level: Level,
    /// What reported it, such as `terminal`, `vcs` or `extension npm`.
    pub source: String,
    pub message: String,
    /// How many times it happened in a row, at least 1.
    pub count: u32,
    /// Whether it came from the log file of an earlier run.
    pub earlier: bool,
}

impl Entry {
    /// Whether the entry's source, message or level word holds `needle`,
    /// which is lower case. An empty needle matches every entry.
    pub fn matches(&self, needle: &str) -> bool {
        needle.is_empty()
            || self.message.to_lowercase().contains(needle)
            || self.source.to_lowercase().contains(needle)
            || self.level.word().eq_ignore_ascii_case(needle)
    }

    /// The entry as one line of text, as the Logs page copies it: the time
    /// in UTC, the level, the source, the message and, for a repeated
    /// entry, how many times it happened.
    pub fn text(&self) -> String {
        let mut text = format!(
            "{} {} [{}] {}",
            utc(self.at),
            self.level.word(),
            self.source,
            self.message
        );
        if self.count > 1 {
            text.push_str(&format!(" (x{})", self.count));
        }
        text
    }
}

/// The log: its entries, newest last, and the file it writes.
#[derive(Debug)]
pub struct Log {
    entries: VecDeque<Entry>,
    next_id: u64,
    file: Option<File>,
    path: Option<PathBuf>,
    /// Bytes in the file so far.
    size: u64,
    /// The newest entry is written to the file with this count; a higher
    /// one is written when another entry comes.
    written_count: u32,
}

impl Log {
    pub const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 1,
            file: None,
            path: None,
            size: 0,
            written_count: 0,
        }
    }

    /// Starts writing to `FILE_NAME` in `dir`, after reading the entries
    /// already in it. Returns the file's path.
    pub fn open(&mut self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(FILE_NAME);
        let earlier = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let mut loaded: VecDeque<Entry> = earlier
            .lines()
            .filter_map(parse_line)
            .map(|mut entry| {
                entry.earlier = true;
                entry
            })
            .collect();
        while loaded.len() + self.entries.len() > CAPACITY && !loaded.is_empty() {
            loaded.pop_front();
        }
        for entry in loaded.iter_mut() {
            entry.id = self.next_id;
            self.next_id += 1;
        }
        // Entries recorded before the file opened are this run's, so they
        // come after the earlier runs' and go into the file now.
        let current: Vec<Entry> = self.entries.drain(..).collect();
        self.entries = loaded;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        self.file = Some(file);
        self.path = Some(path.clone());
        self.written_count = 0;
        for mut entry in current {
            entry.id = self.next_id;
            self.next_id += 1;
            let count = entry.count;
            self.entries.push_back(entry);
            self.written_count = 0;
            self.write_newest(count);
        }
        Ok(path)
    }

    /// Adds an entry, or counts it again when it repeats the newest one.
    pub fn record(&mut self, level: Level, source: &str, message: &str, at: SystemTime) {
        if let Some(last) = self.entries.back_mut()
            && !last.earlier
            && last.level == level
            && last.source == source
            && last.message == message
        {
            last.count = last.count.saturating_add(1);
            last.at = at;
            return;
        }
        // The run of repeats before this entry goes into the file first.
        if let Some(last) = self.entries.back()
            && !last.earlier
            && last.count > self.written_count
        {
            let count = last.count;
            self.write_newest(count);
        }
        if self.entries.len() == CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            id: self.next_id,
            at,
            level,
            source: source.to_owned(),
            message: message.to_owned(),
            count: 1,
            earlier: false,
        });
        self.next_id += 1;
        self.written_count = 0;
        self.write_newest(1);
    }

    /// Writes the newest entry to the file with `count`, moving the file
    /// aside first when it is full. A file that cannot be written is
    /// dropped: the entries stay in memory.
    fn write_newest(&mut self, count: u32) {
        let Some(entry) = self.entries.back() else {
            return;
        };
        let mut line = file_line(entry);
        if count > 1 {
            line.push_str(&format!("\t{count}"));
        }
        line.push('\n');
        self.written_count = count;
        if self.size + line.len() as u64 > MAX_FILE
            && let Some(path) = &self.path
        {
            self.file = None;
            let old = path.with_extension("log.1");
            let reopened = std::fs::rename(path, &old).and_then(|()| {
                OpenOptions::new().create(true).append(true).open(path)
            });
            self.size = 0;
            self.file = reopened.ok();
        }
        if let Some(file) = &mut self.file {
            if file.write_all(line.as_bytes()).is_ok() {
                self.size += line.len() as u64;
            } else {
                self.file = None;
            }
        }
    }

    /// Writes a pending count of repeats, as the app ends.
    pub fn flush(&mut self) {
        if let Some(last) = self.entries.back()
            && !last.earlier
            && last.count > self.written_count
        {
            let count = last.count;
            self.write_newest(count);
        }
    }

    /// Drops every entry and empties the file.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.written_count = 0;
        if let Some(file) = &self.file
            && file.set_len(0).is_ok()
        {
            self.size = 0;
        }
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path.with_extension("log.1"));
        }
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &Entry> + ExactSizeIterator {
        self.entries.iter()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

static LOG: Mutex<Log> = Mutex::new(Log::new());
/// Bumped by every change to [`LOG`], so the Logs page redraws only when
/// there is something new. Starts at 1, so a page that saw 0 has not drawn.
static REVISION: AtomicU64 = AtomicU64::new(1);

fn with_log<T>(body: impl FnOnce(&mut Log) -> T) -> T {
    body(&mut LOG.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Starts writing the app's log to `dir`, the logs directory, and notes
/// that this run started. Entries recorded before go into the file too.
pub fn init(dir: &Path) -> io::Result<PathBuf> {
    let path = with_log(|log| log.open(dir))?;
    info(
        "app",
        format!("scriptschnell-term {} started", env!("CARGO_PKG_VERSION")),
    );
    REVISION.fetch_add(1, Ordering::Relaxed);
    Ok(path)
}

/// Adds an entry to the app's log.
pub fn record(level: Level, source: &str, message: impl AsRef<str>) {
    with_log(|log| log.record(level, source, message.as_ref(), SystemTime::now()));
    REVISION.fetch_add(1, Ordering::Relaxed);
}

pub fn error(source: &str, message: impl AsRef<str>) {
    record(Level::Error, source, message);
}

pub fn warn(source: &str, message: impl AsRef<str>) {
    record(Level::Warning, source, message);
}

pub fn info(source: &str, message: impl AsRef<str>) {
    record(Level::Info, source, message);
}

/// Logs an error and prints it to stderr, for the places that printed it
/// before the log existed: the terminal that started the app still sees it.
pub fn error_and_print(source: &str, message: impl AsRef<str>) {
    let message = message.as_ref();
    eprintln!("scriptschnell-term: {message}");
    error(source, message);
}

/// The entries, oldest first.
pub fn entries() -> Vec<Entry> {
    with_log(|log| log.entries().cloned().collect())
}

/// Changes whenever the log does. An atomic load, for per-frame polls.
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

/// Where the log file is, once [`init`] opened it.
pub fn path() -> Option<PathBuf> {
    with_log(|log| log.path().map(Path::to_path_buf))
}

/// Drops every entry, in memory and in the file.
pub fn clear() {
    with_log(Log::clear);
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Writes a pending count of repeats to the file.
pub fn flush() {
    with_log(Log::flush);
}

/// Logs every panic with where it happened, then runs the hook that was
/// in place, so the message still reaches stderr.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let thread = std::thread::current();
        let mut text = format!("thread '{}' panicked", thread.name().unwrap_or("unnamed"));
        if let Some(place) = info.location() {
            text.push_str(&format!(" at {}:{}", place.file(), place.line()));
        }
        if !message.is_empty() {
            text.push_str(": ");
            text.push_str(&message);
        }
        // A panic while the log is locked must not deadlock: try only.
        if let Ok(mut log) = LOG.try_lock() {
            log.record(Level::Error, "panic", &text, SystemTime::now());
            log.flush();
            REVISION.fetch_add(1, Ordering::Relaxed);
        }
        previous(info);
    }));
}

/// `at` as `2026-11-25 18:10:38 UTC`.
pub fn utc(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The entry as the file keeps it: tab-separated time (seconds and
/// milliseconds since the epoch, then readable UTC), level, source and
/// message, with tabs, newlines and backslashes in them escaped.
fn file_line(entry: &Entry) -> String {
    let since = entry.at.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    format!(
        "{}.{:03}\t{}\t{}\t{}\t{}",
        since.as_secs(),
        since.subsec_millis(),
        utc(entry.at),
        entry.level.word(),
        escape(&entry.source),
        escape(&entry.message)
    )
}

/// An entry from a line [`file_line`] wrote, with an optional count after
/// the message. `None` for a line it did not write.
fn parse_line(line: &str) -> Option<Entry> {
    let mut fields = line.split('\t');
    let stamp = fields.next()?;
    let _readable = fields.next()?;
    let level = Level::parse(fields.next()?)?;
    let source = unescape(fields.next()?);
    let message = unescape(fields.next()?);
    let count = match fields.next() {
        Some(count) => count.parse().ok()?,
        None => 1,
    };
    let (secs, millis) = stamp.split_once('.').unwrap_or((stamp, "0"));
    let at = UNIX_EPOCH
        + Duration::from_secs(secs.parse().ok()?)
        + Duration::from_millis(millis.parse().ok()?);
    Some(Entry {
        id: 0,
        at,
        level,
        source,
        message,
        count,
        earlier: false,
    })
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The date of day `days` after 1970-01-01, after Howard Hinnant's
/// `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn dates_are_written_in_utc() {
        assert_eq!(utc(at(0)), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(at(951_782_400)), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(at(1_795_630_238)), "2026-11-25 18:10:38 UTC");
    }

    #[test]
    fn lines_keep_tabs_newlines_and_backslashes() {
        let entry = Entry {
            id: 0,
            at: at(1_795_630_238) + Duration::from_millis(42),
            level: Level::Warning,
            source: "extension a\tb".into(),
            message: "first\nsecond \\n\tthird\\".into(),
            count: 1,
            earlier: false,
        };
        let line = file_line(&entry);
        assert!(!line.contains('\n'));
        assert_eq!(parse_line(&line), Some(entry.clone()));
        assert_eq!(
            parse_line(&format!("{line}\t3")).map(|entry| entry.count),
            Some(3)
        );
        assert_eq!(parse_line("not a log line"), None);
    }

    #[test]
    fn repeats_count_up_and_reach_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::new();
        log.record(Level::Error, "early", "before the file", at(1));
        let path = log.open(dir.path()).unwrap();
        for _ in 0..3 {
            log.record(Level::Error, "vcs", "git failed", at(2));
        }
        log.record(Level::Info, "notice", "done", at(3));
        let shown: Vec<_> = log
            .entries()
            .map(|e| (e.source.as_str(), e.count))
            .collect();
        assert_eq!(shown, [("early", 1), ("vcs", 1 + 2), ("notice", 1)]);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].ends_with("before the file"));
        assert!(lines[1].ends_with("git failed"));
        assert!(lines[2].ends_with("git failed\t3"));

        // The next run reads them back as earlier entries, the repeated
        // one with its count.
        let mut next = Log::new();
        next.open(dir.path()).unwrap();
        let earlier: Vec<_> = next
            .entries()
            .map(|e| (e.source.as_str(), e.count, e.earlier))
            .collect();
        assert_eq!(
            earlier,
            [
                ("early", 1, true),
                ("vcs", 1, true),
                ("vcs", 3, true),
                ("notice", 1, true)
            ]
        );

        next.clear();
        assert_eq!(next.entries().len(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn a_full_file_moves_aside() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Log::new();
        let path = log.open(dir.path()).unwrap();
        let long = "x".repeat(1000);
        for index in 0..(MAX_FILE / 1000 + 10) {
            log.record(Level::Info, "test", &format!("{index} {long}"), at(index));
        }
        let old = path.with_extension("log.1");
        assert!(old.exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= MAX_FILE);
        assert!(std::fs::metadata(&old).unwrap().len() <= MAX_FILE);
        assert!(log.entries().len() <= CAPACITY);
    }

    #[test]
    fn search_finds_source_message_and_level() {
        let entry = Entry {
            id: 1,
            at: at(0),
            level: Level::Error,
            source: "Forge".into(),
            message: "Token Expired".into(),
            count: 2,
            earlier: false,
        };
        assert!(entry.matches(""));
        assert!(entry.matches("forge"));
        assert!(entry.matches("expired"));
        assert!(entry.matches("error"));
        assert!(!entry.matches("vcs"));
        assert_eq!(
            entry.text(),
            "1970-01-01 00:00:00 UTC ERROR [Forge] Token Expired (x2)"
        );
    }
}
