//! Feeds bytes to a terminal and prints the screen.
//!
//! Usage: `dump [--rows N] [--cols N] [FILE]`. Reads stdin when no file is given.

use cmdterm_vt::{Config, Size, Terminal};
use std::io::Read;

fn main() {
    let (mut rows, mut cols) = (24usize, 80usize);
    let mut file = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--rows" => rows = args.next().and_then(|v| v.parse().ok()).unwrap_or(rows),
            "--cols" => cols = args.next().and_then(|v| v.parse().ok()).unwrap_or(cols),
            _ => file = Some(a),
        }
    }
    let mut input = Vec::new();
    let read = match &file {
        Some(path) => std::fs::read(path).map(|b| input = b),
        None => std::io::stdin().read_to_end(&mut input).map(|_| ()),
    };
    if let Err(e) = read {
        eprintln!("dump: {e}");
        std::process::exit(1);
    }
    let mut term = Terminal::new(Size { rows, cols }, Config::default());
    term.advance(&input);
    for line in term.screen_text() {
        println!("{line}");
    }
    let c = term.cursor();
    println!("cursor: row={} col={}", c.row, c.col);
    let mut replies = Vec::new();
    term.take_replies(&mut replies);
    if !replies.is_empty() {
        println!("replies: {:?}", String::from_utf8_lossy(&replies));
    }
    for e in term.drain_events() {
        println!("event: {e:?}");
    }
}
