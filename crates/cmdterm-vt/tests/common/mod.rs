#![allow(dead_code)]
use cmdterm_vt::{Config, Size, Terminal};

pub fn term(rows: usize, cols: usize, input: &str) -> Terminal {
    let mut t = Terminal::new(Size { rows, cols }, Config::default());
    t.advance(input.as_bytes());
    t
}

pub fn lines(t: &Terminal) -> Vec<String> {
    t.screen_text()
}

pub fn pos(t: &Terminal) -> (usize, usize) {
    let c = t.cursor();
    (c.row, c.col)
}

pub fn seqnos(t: &Terminal) -> Vec<u64> {
    (0..t.size().rows)
        .map(|y| t.row(y).unwrap().seqno())
        .collect()
}
