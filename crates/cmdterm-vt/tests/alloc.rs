//! Allocation gates: once a terminal is warm (full scrollback, every style, link and
//! cluster of a stream seen once), advancing it allocates nothing.
//!
//! The counting allocator comes from the `allocation-counter` dev-dependency, so this
//! file needs no unsafe code of its own.

#[path = "../benches/common/workloads.rs"]
mod workloads;

use cmdterm_vt::{Config, Size, Terminal};

/// Input per phase. Debug builds use less so the suite stays quick.
const SIZE: usize = if cfg!(debug_assertions) {
    256 << 10
} else {
    4 << 20
};

/// Scrollback lines. Release builds use the default; debug builds a smaller one.
const SCROLLBACK: usize = if cfg!(debug_assertions) { 200 } else { 10_000 };

fn warm_terminal() -> Terminal {
    let config = Config {
        scrollback: SCROLLBACK,
        ..Config::default()
    };
    Terminal::new(Size { rows: 24, cols: 80 }, config)
}

/// Feeds the stream in 4 KiB chunks, as a PTY reader would.
fn feed(t: &mut Terminal, data: &[u8]) {
    for chunk in data.chunks(4096) {
        t.advance(chunk);
    }
}

fn gate(name: &str, data: &[u8], scrolls: bool) {
    let mut t = warm_terminal();
    // Fill the scrollback and take every style, link and cluster through once.
    for _ in 0..3 {
        feed(&mut t, data);
        let mut replies = Vec::new();
        t.take_replies(&mut replies);
        t.drain_events().for_each(drop);
    }
    if scrolls {
        assert_eq!(
            t.scrollback_len(),
            SCROLLBACK,
            "{name}: scrollback not full"
        );
    }
    let info = allocation_counter::measure(|| feed(&mut t, data));
    assert_eq!(info.count_total, 0, "{name}: {info:?}");
}

#[test]
fn plain_ascii_lines_do_not_allocate() {
    gate("ascii", &workloads::ascii(SIZE), true);
}

#[test]
fn sgr_colored_output_does_not_allocate() {
    gate("sgr", &workloads::sgr(SIZE), true);
}

#[test]
fn scrolling_after_a_reflow_does_not_allocate() {
    let data = workloads::ascii(SIZE);
    let mut t = warm_terminal();
    for _ in 0..3 {
        feed(&mut t, &data);
    }
    // Narrow and widen again: every row is rebuilt by the reflow.
    t.resize(Size { rows: 24, cols: 60 });
    t.resize(Size { rows: 24, cols: 80 });
    // One full scrollback cycle brings every row buffer back through the scroll path.
    // The pass after it is the one that must stay quiet.
    let cycle = SCROLLBACK * 90;
    let mut line = Vec::new();
    for i in 0..cycle / 90 + 1 {
        line.extend_from_slice(format!("{i:0>78}\r\n").as_bytes());
    }
    feed(&mut t, &line);
    let info = allocation_counter::measure(|| feed(&mut t, &line));
    assert_eq!(info.count_total, 0, "{info:?}");
}

#[test]
fn tui_redraws_do_not_allocate() {
    // A TUI redraws in place and never fills the scrollback.
    gate("tui", &workloads::tui(SIZE), false);
}

#[test]
fn wide_characters_and_clusters_do_not_allocate() {
    gate("wide", &workloads::wide(SIZE), true);
}

#[test]
fn repeated_osc_8_links_do_not_allocate() {
    gate("links", &workloads::links(SIZE), true);
}

#[test]
fn replies_and_events_do_not_allocate_in_steady_state() {
    let mut t = warm_terminal();
    let mut out = Vec::with_capacity(1024);
    let round = |t: &mut Terminal, out: &mut Vec<u8>| {
        t.advance(b"\x1b[5n\x1b[6n\x1b[c\x1b[>c\x1b[?6n\x07");
        out.clear();
        t.take_replies(out);
        t.drain_events().for_each(drop);
    };
    for _ in 0..3 {
        round(&mut t, &mut out);
    }
    let info = allocation_counter::measure(|| {
        for _ in 0..100 {
            round(&mut t, &mut out);
        }
    });
    assert_eq!(info.count_total, 0, "{info:?}");
}

#[test]
fn input_encoders_do_not_allocate_with_a_warm_buffer() {
    use cmdterm_vt::{
        KeyCode, KeyEventKind, Mods, MouseButton, MouseEvent,
        MouseEventKind::{Move, Press, Release},
    };
    // Plain modes, then application keys with the kitty protocol and SGR mouse reporting.
    for setup in ["", "\x1b[?1h\x1b=\x1b[>4;2m\x1b[?1000h\x1b[?1006h\x1b[>1u"] {
        let mut t = warm_terminal();
        t.advance(setup.as_bytes());
        let mut out = Vec::with_capacity(1024);
        let run = |t: &mut Terminal, out: &mut Vec<u8>| {
            let keys = [
                (KeyCode::Char('a'), Mods::NONE),
                (KeyCode::Char('c'), Mods::CTRL),
                (KeyCode::Enter, Mods::NONE),
                (KeyCode::F(5), Mods::SHIFT),
            ];
            for (key, mods) in keys {
                for kind in [
                    KeyEventKind::Press,
                    KeyEventKind::Repeat,
                    KeyEventKind::Release,
                ] {
                    out.clear();
                    t.encode_key(key, mods, kind, Some("a"), out);
                }
            }
            for (i, kind) in [Press, Move, Release].into_iter().enumerate() {
                out.clear();
                t.encode_mouse(&MouseEvent::new(kind, MouseButton::Left, 3 + i, 4), out);
            }
            out.clear();
            t.encode_paste("hello world\nsecond line", out);
            out.clear();
            t.encode_focus(true, out);
        };
        run(&mut t, &mut out);
        let info = allocation_counter::measure(|| run(&mut t, &mut out));
        assert_eq!(info.count_total, 0, "{setup:?}: {info:?}");
    }
}
