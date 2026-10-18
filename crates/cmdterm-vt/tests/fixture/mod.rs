//! Used by `tests/fixtures.rs`: the fixture layout, replay and the snapshot text.
//!
//! A fixture is a directory `tests/fixtures/<name>/` with:
//!
//! - `meta.toml`: program, args, size, environment summary, date, tool version.
//! - `output.bin`: the bytes the program wrote to the terminal.
//! - `chunks.txt`: `chunk <n>` takes the next n bytes of `output.bin` (one PTY read);
//!   `resize <rows> <cols>` resizes the terminal. Events replay in file order.
//! - `snapshot.txt`: the golden final state of cmdterm-vt (see [`render`]).
#![allow(dead_code)]

use cmdterm_vt::{Color, Config, Size, Style, Terminal, Underline, Width, flags};
use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Lines of scrollback every replay keeps, so snapshots of long outputs stay readable.
pub const SCROLLBACK: usize = 200;

/// Seed of the random chunk splits.
pub const SPLIT_SEED: u64 = 0x5eed_f1e7_u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    Chunk(usize),
    Resize(usize, usize),
}

pub struct Fixture {
    pub dir: PathBuf,
    pub name: String,
    pub rows: usize,
    pub cols: usize,
    pub output: Vec<u8>,
    pub events: Vec<Ev>,
}

pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn format_chunks(events: &[Ev]) -> String {
    let mut s = String::new();
    for e in events {
        match e {
            Ev::Chunk(n) => writeln!(s, "chunk {n}").unwrap(),
            Ev::Resize(r, c) => writeln!(s, "resize {r} {c}").unwrap(),
        }
    }
    s
}

pub fn parse_chunks(text: &str) -> Result<Vec<Ev>, String> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let num = |s: &str| {
            s.parse::<usize>()
                .map_err(|_| format!("chunks.txt line {}: bad number {s:?}", i + 1))
        };
        match parts.as_slice() {
            ["chunk", n] => out.push(Ev::Chunk(num(n)?)),
            ["resize", r, c] => out.push(Ev::Resize(num(r)?, num(c)?)),
            _ => return Err(format!("chunks.txt line {}: cannot parse {line:?}", i + 1)),
        }
    }
    Ok(out)
}

/// Reads `rows` and `cols` from the `[size]` table of a `meta.toml`.
pub fn parse_size(meta: &str) -> Result<(usize, usize), String> {
    let t: toml::Table = meta.parse().map_err(|e| format!("meta.toml: {e}"))?;
    let size = t
        .get("size")
        .and_then(|v| v.as_table())
        .ok_or("meta.toml: no [size] table")?;
    let get = |k: &str| {
        size.get(k)
            .and_then(|v| v.as_integer())
            .map(|v| v as usize)
            .ok_or(format!("meta.toml: size.{k} missing"))
    };
    Ok((get("rows")?, get("cols")?))
}

pub fn load(dir: &Path) -> Result<Fixture, String> {
    let read =
        |f: &str| std::fs::read(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()));
    let meta = String::from_utf8(read("meta.toml")?).map_err(|e| e.to_string())?;
    let (rows, cols) = parse_size(&meta)?;
    let output = read("output.bin")?;
    let events = parse_chunks(&String::from_utf8_lossy(&read("chunks.txt")?))?;
    let total: usize = events
        .iter()
        .map(|e| if let Ev::Chunk(n) = e { *n } else { 0 })
        .sum();
    if total != output.len() {
        return Err(format!(
            "chunks.txt covers {total} bytes, output.bin has {}",
            output.len()
        ));
    }
    Ok(Fixture {
        dir: dir.to_path_buf(),
        name: dir.file_name().unwrap().to_string_lossy().into_owned(),
        rows,
        cols,
        output,
        events,
    })
}

/// A xorshift64 generator for the random splits.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    pub fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

#[derive(Clone, Copy)]
pub enum Split {
    /// The chunks as the program's reads delivered them.
    Recorded,
    /// Each recorded chunk cut at random places, from this seed.
    Random(u64),
}

/// The event list a replay feeds: chunk lengths (cut for `Random`) and resizes in order.
pub fn plan(events: &[Ev], split: Split) -> Vec<Ev> {
    let Split::Random(seed) = split else {
        return events.to_vec();
    };
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    for e in events {
        match e {
            Ev::Chunk(n) => {
                let mut left = *n;
                while left > 0 {
                    // Mostly short pieces, sometimes a single byte, sometimes the rest.
                    let take = match rng.below(4) {
                        0 => 1,
                        1 => left,
                        _ => 1 + rng.below(left.min(64)),
                    };
                    out.push(Ev::Chunk(take));
                    left -= take;
                }
            }
            r => out.push(r.clone()),
        }
    }
    out
}

pub fn config() -> Config {
    Config {
        scrollback: SCROLLBACK,
        ..Config::default()
    }
}

pub fn replay(fx: &Fixture, split: Split) -> Terminal {
    let mut t = Terminal::new(
        Size {
            rows: fx.rows,
            cols: fx.cols,
        },
        config(),
    );
    let mut at = 0;
    let mut replies = Vec::new();
    for e in plan(&fx.events, split) {
        match e {
            Ev::Chunk(n) => {
                t.advance(&fx.output[at..at + n]);
                at += n;
                replies.clear();
                t.take_replies(&mut replies);
                t.drain_events().for_each(drop);
            }
            Ev::Resize(rows, cols) => t.resize(Size { rows, cols }),
        }
    }
    t
}

fn color(out: &mut String, key: &str, c: Color) {
    match c {
        Color::Default => {}
        Color::Palette(n) => write!(out, " {key}={n}").unwrap(),
        Color::Rgb(r, g, b) => write!(out, " {key}=#{r:02x}{g:02x}{b:02x}").unwrap(),
    }
}

fn style_text(s: &Style) -> String {
    let mut out = String::new();
    for (bit, name) in [
        (flags::BOLD, "bold"),
        (flags::FAINT, "faint"),
        (flags::ITALIC, "italic"),
        (flags::BLINK, "blink"),
        (flags::INVERSE, "inverse"),
        (flags::INVISIBLE, "invisible"),
        (flags::STRIKE, "strike"),
    ] {
        if s.has(bit) {
            write!(out, " {name}").unwrap();
        }
    }
    if s.underline != Underline::None {
        write!(out, " underline={:?}", s.underline).unwrap();
    }
    color(&mut out, "fg", s.fg);
    color(&mut out, "bg", s.bg);
    color(&mut out, "ul", s.underline_color);
    if s.hyperlink != 0 {
        out.push_str(" link");
    }
    out.trim_start().to_string()
}

/// The golden text of a terminal state: size, title, cursor, modes, the scrollback text
/// and, for each screen row, its text (clusters whole) and the runs of non-default style.
pub fn render(t: &Terminal) -> String {
    let mut s = String::new();
    let size = t.size();
    writeln!(s, "size {}x{}", size.rows, size.cols).unwrap();
    writeln!(s, "title {:?}", t.title()).unwrap();
    writeln!(s, "icon {:?}", t.icon_name()).unwrap();
    let c = t.cursor();
    writeln!(
        s,
        "cursor row={} col={} visible={} pending_wrap={} shape={:?} blink={}",
        c.row, c.col, c.visible, c.pending_wrap, c.shape, c.blink
    )
    .unwrap();
    let m = t.modes();
    writeln!(s, "alt_screen {}", m.alt_screen).unwrap();
    let mut on = Vec::new();
    for (flag, name) in [
        (m.autowrap, "autowrap"),
        (m.origin, "origin"),
        (m.insert, "insert"),
        (m.left_right_margins, "left_right_margins"),
        (m.app_cursor, "app_cursor"),
        (m.app_keypad, "app_keypad"),
        (m.reverse_video, "reverse_video"),
        (m.focus_events, "focus_events"),
        (m.alternate_scroll, "alternate_scroll"),
        (m.bracketed_paste, "bracketed_paste"),
        (m.synchronized_output, "synchronized_output"),
        (m.newline, "newline"),
    ] {
        if flag {
            on.push(name.to_string());
        }
    }
    writeln!(s, "modes {}", on.join(" ")).unwrap();
    writeln!(
        s,
        "mouse {:?} encoding {:?} kitty_flags {}",
        m.mouse, m.mouse_encoding, m.kitty_flags
    )
    .unwrap();
    let screen = t.screen();
    writeln!(s, "scrollback {}", screen.scrollback_len()).unwrap();
    for i in 0..screen.scrollback_len() {
        if let Some(row) = screen.scrollback_row(i) {
            let wrap = if row.wrapped() { '~' } else { ' ' };
            writeln!(s, "sb {i:04}{wrap}|{}|", t.row_text(row)).unwrap();
        }
    }
    writeln!(s, "screen").unwrap();
    for y in 0..size.rows {
        let Some(row) = t.row(y) else { continue };
        let wrap = if row.wrapped() { '~' } else { ' ' };
        writeln!(s, "{y:02}{wrap}|{}|", t.row_text(row)).unwrap();
        // Runs of one style over the columns. A wide cell and its continuation share one.
        let mut runs = Vec::new();
        let mut x = 0;
        while x < size.cols {
            let st = *t.style(row.get(x).style);
            let mut end = x;
            while end + 1 < size.cols {
                let n = row.get(end + 1);
                if n.width != Width::Continuation && *t.style(n.style) != st {
                    break;
                }
                end += 1;
            }
            if st != Style::default() {
                runs.push(format!("{x}-{end} {}", style_text(&st)));
            }
            x = end + 1;
        }
        if !runs.is_empty() {
            writeln!(s, "   runs {}", runs.join("; ")).unwrap();
        }
    }
    s
}

/// Paths that must not appear in a fixture.
pub fn path_leaks(bytes: &[u8]) -> Vec<&'static str> {
    let mut v = Vec::new();
    for pat in ["/home/", "/Users/"] {
        if bytes.windows(pat.len()).any(|w| w == pat.as_bytes()) {
            v.push(pat);
        }
    }
    v
}
