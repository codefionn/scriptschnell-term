//! The scrollback limit, ED 3, eviction, and the collection of the style and link tables.

mod common;

use cmdterm_vt::{Color, Config, SemanticKind, Size, Terminal};
use common::{lines, term};

fn with_limit(rows: usize, cols: usize, scrollback: usize) -> Terminal {
    let config = Config {
        scrollback,
        ..Config::default()
    };
    Terminal::new(Size { rows, cols }, config)
}

/// Stable indexes, the scrollback length and the rows agree with each other.
fn check_stable(t: &Terminal, printed: u64, rows: usize) {
    let s = t.screen();
    assert_eq!(
        s.stable_index(0),
        s.first_stable() + s.scrollback_len() as u64
    );
    assert_eq!(
        s.stable_index(rows - 1) + 1,
        s.first_stable() + s.scrollback_len() as u64 + rows as u64
    );
    for stable in s.first_stable()..s.first_stable() + (s.scrollback_len() + rows) as u64 {
        let want = format!("line{stable}");
        let row = t
            .row_by_stable(stable)
            .unwrap_or_else(|| panic!("no row {stable}"));
        // The cursor's row is empty before its text is written.
        let text = t.row_text(row);
        assert!(
            text == want || (stable >= printed && text.is_empty()),
            "{stable}: {text}"
        );
    }
    assert!(t.row_by_stable(s.first_stable().wrapping_sub(1)).is_none() || s.first_stable() == 0);
    assert!(t.row_by_stable(s.stable_index(rows - 1) + 1).is_none());
}

#[test]
fn the_limit_holds_and_stable_indexes_stay_monotonic() {
    let mut t = with_limit(3, 12, 5);
    let mut last_first = 0;
    for i in 0..40u64 {
        t.advance(format!("line{i}\r\n").as_bytes());
        let first = t.screen().first_stable();
        assert!(first >= last_first);
        last_first = first;
        assert!(t.scrollback_len() <= 5);
        check_stable(&t, i + 1, 3);
    }
    assert_eq!(t.scrollback_len(), 5);
    // 40 lines and the cursor's row: 41 in all, 3 in the viewport, 5 kept.
    assert_eq!(t.screen().first_stable(), 41 - 3 - 5);
}

#[test]
fn a_limit_of_zero_keeps_no_scrollback() {
    let mut t = with_limit(3, 12, 0);
    for i in 0..10u64 {
        t.advance(format!("line{i}\r\n").as_bytes());
        assert_eq!(t.scrollback_len(), 0);
        let s = t.screen();
        assert_eq!(s.stable_index(0), s.first_stable());
        check_stable(&t, i + 1, 3);
    }
    assert!(t.row_by_stable(0).is_none());
    assert_eq!(t.screen().first_stable(), 11 - 3);
    // Scroll regions and wide rows do not keep anything either.
    t.advance(b"\x1b[2J\x1b[H\x1bM\x1b[3S\x1b[1T");
    assert_eq!(t.scrollback_len(), 0);
}

#[test]
fn ed3_clears_the_scrollback_and_keeps_indexes_moving_forward() {
    let mut t = with_limit(3, 12, 100);
    for i in 0..30u64 {
        t.advance(format!("line{i}\r\n").as_bytes());
    }
    let sb = t.scrollback_len() as u64;
    let (first, stable0) = (t.screen().first_stable(), t.stable_index(0));
    let view = lines(&t);
    t.advance(b"\x1b[3J");
    assert_eq!(t.scrollback_len(), 0);
    assert_eq!(t.screen().first_stable(), first + sb);
    assert_eq!(t.stable_index(0), stable0);
    assert_eq!(lines(&t), view);
    assert!(t.row_by_stable(first).is_none());
    t.advance(b"line30\r\nline31\r\n");
    assert_eq!(t.scrollback_len(), 2);
    check_stable(&t, 32, 3);
    // And again on a terminal with nothing to clear.
    let mut t = with_limit(3, 12, 0);
    t.advance(b"a\r\nb\r\nc\r\nd\x1b[3J");
    assert_eq!(t.scrollback_len(), 0);
}

fn cycle(i: usize) -> String {
    format!(
        "\x1b]133;A;aid={i}\x07$ cmd{i}\x1b]133;B\x07\r\n\x1b]133;C\x07out{i}\r\n\x1b]133;D;0\x07"
    )
}

#[test]
fn marks_of_evicted_rows_are_dropped() {
    let mut t = with_limit(4, 20, 3);
    for i in 0..300 {
        t.advance(cycle(i).as_bytes());
        // Seven lines are kept, two marks per command line pair at most.
        assert!(t.mark_count() <= 14, "{} marks after {i}", t.mark_count());
    }
    let first = t.screen().first_stable();
    let zones = t.semantic_zones();
    assert!(zones.iter().all(|z| z.start_row >= first));
    // The oldest kept lines are still told apart: the newest evicted mark decides.
    assert!(matches!(
        zones[0].kind,
        SemanticKind::Prompt | SemanticKind::Output | SemanticKind::Input
    ));
    // ED 3 drops the marks of the rows it removes.
    let mut t = with_limit(4, 20, 100);
    for i in 0..50 {
        t.advance(cycle(i).as_bytes());
    }
    let before = t.mark_count();
    t.advance(b"\x1b[3J");
    let first = t.screen().first_stable();
    assert!(t.mark_count() < before);
    assert!(t.mark_count() <= 5, "{}", t.mark_count());
    assert!(t.semantic_zones().iter().all(|z| z.start_row >= first));
}

/// A line with a new truecolor and a new link of its own.
fn distinct(n: u32) -> String {
    format!(
        "\x1b[38;2;{};{};{}m\x1b]8;;http://h.test/{n}\x07x\x1b]8;;\x07\x1b[0m\r\n",
        n & 255,
        (n >> 8) & 255,
        (n >> 16) & 255
    )
}

#[test]
fn tables_stay_bounded_over_a_full_scrollback() {
    let mut t = with_limit(10, 80, 1000);
    let mut max = (0, 0, 0);
    for batch in 0..40u32 {
        let mut input = String::new();
        for n in batch * 10_000..(batch + 1) * 10_000 {
            input.push_str(&distinct(n));
        }
        t.advance(input.as_bytes());
        let (s, l, b) = t.table_sizes();
        max = (max.0.max(s), max.1.max(l), max.2.max(b));
    }
    // 400000 distinct styles and links went by; a thousand lines are kept.
    assert!(max.0 < 5000 && max.1 < 5000 && max.2 < 200_000, "{max:?}");
    assert!(t.table_sweeps() > 10);
    // The kept lines still resolve to their own link and color.
    let last = 400_000u32 - 1;
    for back in 0..5u32 {
        let n = last - back;
        let y = 8 - back as usize;
        let c = t.row(y).unwrap().get(0);
        assert_eq!(c.ch(), 'x');
        assert_eq!(t.hyperlink(&c), Some(format!("http://h.test/{n}").as_str()));
        assert_eq!(
            t.style(c.style).fg,
            Color::Rgb(
                (n & 255) as u8,
                ((n >> 8) & 255) as u8,
                ((n >> 16) & 255) as u8
            )
        );
    }
    let first = t.screen().scrollback_row(0).unwrap().get(0);
    let n = t.screen().first_stable() as u32;
    assert_eq!(
        t.hyperlink(&first),
        Some(format!("http://h.test/{n}").as_str())
    );
}

#[test]
fn many_links_in_one_line_do_not_reach_the_cap() {
    // 200 links per line fill the 65536 slots in 330 lines unless they are collected.
    let mut t = with_limit(3, 400, 20);
    for line in 0..2000u32 {
        let mut s = String::new();
        for k in 0..200u32 {
            s.push_str(&format!("\x1b]8;;http://h.test/{line}/{k}\x07x"));
        }
        s.push_str("\x1b]8;;\x07\r\n");
        t.advance(s.as_bytes());
    }
    let (_, links, _) = t.table_sizes();
    assert!(links < 65536 / 2, "{links} links");
    let c = t.row(1).unwrap().get(199);
    assert_eq!(t.hyperlink(&c), Some("http://h.test/1999/199"));
}

#[test]
fn the_pen_keeps_its_link_and_style_through_a_collection() {
    let mut t = with_limit(4, 40, 200);
    t.advance(b"\x1b]8;id=pen;http://pen.test/\x07\x1b[1;38;2;1;2;3m\x1b7");
    // Line feeds only: the pen's style is not in any cell, but a lot of others are.
    let before = t.table_sweeps();
    for n in 0..30_000u32 {
        t.advance(format!("\x1b[48;2;{};{};{}m\n", n & 255, (n >> 8) & 255, n >> 16).as_bytes());
    }
    t.advance(b"\x1b[1;38;2;1;2;3m\x1b[49m\x1b[H");
    assert!(t.table_sweeps() > before);
    t.advance(b"p");
    let c = t.row(0).unwrap().get(0);
    assert_eq!(t.hyperlink(&c), Some("http://pen.test/"));
    assert_eq!(t.hyperlink_id(&c), Some("pen"));
    assert_eq!(t.style(c.style).fg, Color::Rgb(1, 2, 3));
    // The saved cursor still restores a working pen.
    t.advance(b"\x1b8q");
    let c = t.row(0).unwrap().get(0);
    assert_eq!(c.ch(), 'q');
    assert_eq!(t.style(c.style).fg, Color::Rgb(1, 2, 3));
    // Same link and id intern to one style after collection.
    t.advance(b"\x1b]8;id=pen;http://pen.test/\x07r");
    let row = t.row(0).unwrap();
    assert_eq!(row.get(0).style, row.get(1).style);
}

#[test]
fn overwriting_the_screen_with_new_colors_collects_without_scrolling() {
    let mut t = with_limit(3, 20, 1000);
    for n in 0..200_000u32 {
        t.advance(format!("\r\x1b[38;2;{};{};{}mx", n & 255, (n >> 8) & 255, n >> 16).as_bytes());
    }
    let (styles, _, _) = t.table_sizes();
    assert!(styles < 100_000, "{styles}");
    assert!(t.table_sweeps() > 0);
    assert_eq!(lines(&t)[0], "x");
}

#[test]
fn an_idle_screen_with_few_styles_never_sweeps() {
    let mut t = term(3, 20, "");
    for _ in 0..2000 {
        t.advance(b"\x1b[31mred\x1b[0m plain\r\n");
    }
    assert_eq!(t.table_sweeps(), 0);
}

#[test]
fn printing_into_a_full_scrollback_does_not_allocate() {
    let mut t = with_limit(5, 40, 50);
    let chunk = b"plain text \x1b[1;31mred bold\x1b[0m \x1b[38;5;200mpalette\x1b[m \x1b[2K\x1b[3C\x1b[2Dx\r\n";
    // Fill and warm: the scrollback is full, styles are interned, and every row buffer
    // has been through the ring once.
    for _ in 0..300 {
        t.advance(chunk);
        t.advance(b"0123456789 abcdefghijklmnopqrstuvwxyz 0123456789 0123456789 0123\r\n");
    }
    assert_eq!(t.scrollback_len(), 50);
    let info = allocation_counter::measure(|| {
        for _ in 0..100 {
            t.advance(chunk);
            t.advance(b"0123456789 abcdefghijklmnopqrstuvwxyz 0123456789 0123456789 0123\r\n");
        }
    });
    assert_eq!(info.count_total, 0, "{info:?}");
    assert_eq!(t.scrollback_len(), 50);
}
