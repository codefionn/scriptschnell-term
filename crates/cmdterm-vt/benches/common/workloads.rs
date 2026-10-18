//! Deterministic byte streams shared by the allocation test and the benches.
//!
//! Each generator repeats a small vocabulary, so a terminal that has seen one pass has
//! interned every style, link and cluster the rest of the stream uses.

#![allow(dead_code)]

/// Small xorshift generator, so streams are the same on every run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: &[&str] = &[
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "terminal",
    "emulator",
    "scrollback",
    "cursor",
    "render",
    "buffer",
    "stream",
    "parse",
    "state",
    "machine",
    "0123456789",
    "/usr/local/bin",
    "src/main.rs",
    "fn",
    "let",
    "match",
    "self",
];

fn words(rng: &mut Rng, out: &mut Vec<u8>, count: usize) {
    for i in 0..count {
        if i > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
    }
}

/// Plain ASCII lines of 10 to 160 columns, so some wrap on an 80 column screen.
pub fn ascii(size: usize) -> Vec<u8> {
    let mut rng = Rng::new(1);
    let mut out = Vec::with_capacity(size + 256);
    while out.len() < size {
        let n = 2 + rng.below(24) as usize;
        words(&mut rng, &mut out, n);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// `ls --color` listings and compiler logs: many short SGR runs.
pub fn sgr(size: usize) -> Vec<u8> {
    let mut rng = Rng::new(2);
    let mut out = Vec::with_capacity(size + 256);
    while out.len() < size {
        if rng.below(2) == 0 {
            for _ in 0..6 {
                let w = WORDS[rng.below(WORDS.len() as u64) as usize];
                match rng.below(4) {
                    0 => out.extend_from_slice(b"\x1b[01;34m"),
                    1 => out.extend_from_slice(b"\x1b[01;32m"),
                    2 => out.extend_from_slice(b"\x1b[0m"),
                    _ => out.extend_from_slice(b"\x1b[01;31m"),
                }
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b"\x1b[0m  ");
            }
        } else {
            out.extend_from_slice(
                b"\x1b[1m\x1b[38;5;208mwarning\x1b[0m\x1b[1m: unused variable: `",
            );
            out.extend_from_slice(b"\x1b[38;2;80;200;120m");
            out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize].as_bytes());
            out.extend_from_slice(b"\x1b[0m\x1b[1m`\x1b[0m\r\n \x1b[1;34m-->\x1b[0m src/lib.rs:");
            out.extend_from_slice(rng.below(900).to_string().as_bytes());
            out.extend_from_slice(b"\x1b[4mx\x1b[24m");
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Full-screen redraws: home, absolute moves, erase line, erase below, reverse video
/// status line and relative cursor moves, on a 24 row screen.
pub fn tui(size: usize) -> Vec<u8> {
    let mut rng = Rng::new(3);
    let mut out = Vec::with_capacity(size + 4096);
    while out.len() < size {
        out.extend_from_slice(b"\x1b[H");
        for row in 1..=23 {
            out.extend_from_slice(format!("\x1b[{row};1H\x1b[2K").as_bytes());
            let n = 1 + rng.below(10) as usize;
            words(&mut rng, &mut out, n);
            out.extend_from_slice(b"\x1b[K");
            if rng.below(4) == 0 {
                out.extend_from_slice(b"\x1b[40G\x1b[7m sel \x1b[27m\x1b[2D\x1b[C");
            }
        }
        out.extend_from_slice(
            b"\x1b[24;1H\x1b[7m status: ok \x1b[K\x1b[0m\x1b[J\x1b[?25l\x1b[?25h",
        );
    }
    out
}

/// CJK, combining sequences and emoji clusters mixed with ASCII.
pub fn wide(size: usize) -> Vec<u8> {
    let mut rng = Rng::new(4);
    let items = [
        "\u{4e16}\u{754c}",
        "e\u{301}",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
        "\u{1f1e9}\u{1f1ea}",
        "\u{2764}\u{fe0f}",
        "\u{d55c}\u{ae00}",
        "caf\u{e9}",
        "a\u{300}\u{302}",
    ];
    let mut out = Vec::with_capacity(size + 256);
    while out.len() < size {
        for _ in 0..12 {
            out.extend_from_slice(items[rng.below(items.len() as u64) as usize].as_bytes());
            out.push(b' ');
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// OSC 8 links drawn from a small set, closed after each use.
pub fn links(size: usize) -> Vec<u8> {
    let mut rng = Rng::new(5);
    let mut out = Vec::with_capacity(size + 256);
    while out.len() < size {
        let n = rng.below(8);
        if n.is_multiple_of(2) {
            out.extend_from_slice(
                format!("\x1b]8;id=l{n};https://example.com/page/{n}\x1b\\").as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("\x1b]8;;https://example.com/doc/{n}\x07").as_bytes());
        }
        words(&mut rng, &mut out, 2);
        out.extend_from_slice(b"\x1b]8;;\x1b\\ ");
        if rng.below(5) == 0 {
            out.extend_from_slice(b"\r\n");
        }
    }
    out
}

/// All named workloads.
pub fn all(size: usize) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("ascii", ascii(size)),
        ("sgr", sgr(size)),
        ("tui", tui(size)),
        ("wide", wide(size)),
        ("links", links(size)),
    ]
}
