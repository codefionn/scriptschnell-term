//! Reflow on resize: logical lines, wide characters, the cursor, the saved cursor,
//! marks, styles and links through a change of the column count.

mod common;

use cmdterm_vt::{Config, SemanticKind, Size, Terminal, Width};
use common::{lines, pos, term};
use proptest::prelude::*;

fn size(rows: usize, cols: usize) -> Size {
    Size { rows, cols }
}

/// Every row of the primary screen, scrollback first.
fn all_rows(t: &Terminal) -> Vec<&cmdterm_vt::Row> {
    let s = t.screen();
    (0..s.scrollback_len())
        .map(|i| s.scrollback_row(i).unwrap())
        .chain((0..s.rows()).map(|y| s.row(y).unwrap()))
        .collect()
}

/// The text of each row, scrollback first.
fn row_texts(t: &Terminal) -> Vec<String> {
    all_rows(t).into_iter().map(|r| t.row_text(r)).collect()
}

/// Logical lines: rows joined by the wrapped flag, with the gap left by a wide character
/// that moved to the next row not counted as text. Trailing empty lines are dropped.
fn logical(t: &Terminal) -> Vec<String> {
    let cols = t.size().cols;
    let mut out = Vec::new();
    let mut cur = String::new();
    for r in all_rows(t) {
        for c in r.cells().iter().filter(|c| c.width != Width::Continuation) {
            cur.push_str(t.cell_text(c));
        }
        if r.wrapped() {
            cur.extend(std::iter::repeat_n(' ', r.text_cols(cols) - r.len()));
        } else {
            out.push(std::mem::take(&mut cur).trim_end().to_string());
        }
    }
    if !cur.is_empty() {
        out.push(cur.trim_end().to_string());
    }
    while out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

fn seqs(t: &Terminal) -> Vec<u64> {
    all_rows(t).into_iter().map(|r| r.seqno()).collect()
}

#[test]
fn narrow_then_widen_restores_the_text() {
    let mut t = term(5, 12, "hello world!\r\nfoo bar baz qux\r\nx");
    let before = logical(&t);
    assert_eq!(before, ["hello world!", "foo bar baz qux", "x"]);
    t.resize(size(5, 5));
    assert_eq!(logical(&t), before);
    assert_eq!(
        row_texts(&t).into_iter().take(3).collect::<Vec<_>>(),
        ["hello", " worl", "d!"]
    );
    t.resize(size(5, 12));
    assert_eq!(logical(&t), before);
    assert_eq!(lines(&t)[..3], ["hello world!", "foo bar baz", "qux"]);
}

#[test]
fn widening_joins_wrapped_rows_and_clears_the_flag() {
    let mut t = term(4, 4, "abcdefgh\r\nij");
    assert!(t.row(0).unwrap().wrapped());
    t.resize(size(4, 10));
    assert_eq!(lines(&t), ["abcdefgh", "ij", "", ""]);
    assert!(!t.row(0).unwrap().wrapped());
}

#[test]
fn wide_characters_wrap_whole() {
    // "ab" then two wide characters: width 7.
    let mut t = term(10, 8, "ab\u{65e5}\u{672c}c");
    t.resize(size(10, 3));
    assert_eq!(row_texts(&t)[..3], ["ab", "\u{65e5}", "\u{672c}c"]);
    let r = all_rows(&t)[0];
    assert!(r.wrapped());
    assert_eq!(r.text_cols(3), 2);
    // Wide halves stay paired.
    let wide = all_rows(&t)[1];
    assert_eq!(wide.get(0).width, Width::Wide);
    assert_eq!(wide.get(1).width, Width::Continuation);
    t.resize(size(10, 8));
    assert_eq!(logical(&t), ["ab\u{65e5}\u{672c}c"]);
    t.resize(size(10, 2));
    assert_eq!(
        row_texts(&t)[..4],
        ["ab", "\u{65e5}", "\u{672c}", "c"].map(String::from)
    );
}

#[test]
fn a_wide_character_in_one_column_becomes_a_blank() {
    let mut t = term(10, 4, "a\u{65e5}b");
    t.resize(size(10, 1));
    assert_eq!(row_texts(&t)[..3], ["a", "", "b"].map(String::from));
}

#[test]
fn the_cursor_stays_on_its_character_mid_line() {
    // Cursor on "d".
    let mut t = term(4, 10, "abcdefghij\x1b[1;4H");
    assert_eq!(pos(&t), (0, 3));
    t.resize(size(4, 4));
    assert_eq!(pos(&t), (0, 3));
    t.resize(size(4, 3));
    // "abc" "def" "ghi" "j": the cursor row moved down with its line. The viewport keeps
    // the cursor's distance from the bottom.
    let (y, x) = pos(&t);
    assert_eq!(x, 0);
    assert_eq!(t.row_text(t.row(y).unwrap()), "def");
    t.advance(b"X");
    assert_eq!(logical(&t), ["abcXefghij"]);
}

#[test]
fn the_cursor_at_the_end_of_the_text_waits_to_wrap() {
    let mut t = term(4, 10, "abcdefgh");
    assert_eq!(pos(&t), (0, 8));
    t.resize(size(4, 4));
    let c = t.cursor();
    assert_eq!((c.col, c.pending_wrap), (3, true));
    assert_eq!(t.row_text(t.row(c.row).unwrap()), "efgh");
    t.advance(b"i");
    assert_eq!(logical(&t), ["abcdefghi"]);
    t.resize(size(4, 20));
    assert_eq!(logical(&t), ["abcdefghi"]);
    assert_eq!(pos(&t), (0, 9));
    assert!(!t.cursor().pending_wrap);
}

#[test]
fn a_pending_wrap_stays_pending_when_it_still_sits_at_the_edge() {
    let mut t = term(3, 5, "abcde");
    assert!(t.cursor().pending_wrap);
    t.resize(size(3, 8));
    // The cursor now sits after "abcde" with room to spare.
    assert_eq!(pos(&t), (0, 5));
    assert!(!t.cursor().pending_wrap);
    t.advance(b"f");
    assert_eq!(lines(&t)[0], "abcdef");
}

#[test]
fn the_cursor_in_blank_space_keeps_its_distance_from_the_text() {
    let mut t = term(3, 20, "ab\x1b[1;10H");
    t.resize(size(3, 6));
    // Column 9 does not fit: it clamps to the row's edge.
    assert_eq!(t.cursor().row, 0);
    assert!(t.cursor().col < 6);
}

#[test]
fn the_saved_cursor_follows_reflow() {
    let mut t = term(5, 10, "abcdefghij\x1b[1;7H\x1b7\x1b[2;1H");
    t.resize(size(5, 4));
    t.advance(b"\x1b8X");
    assert_eq!(logical(&t), ["abcdefXhij"]);
    // Back on the wide terminal the saved spot still names "X".
    let mut t = term(5, 10, "abcdefghij\x1b[1;7H\x1b7\x1b[2;1H");
    t.resize(size(5, 4));
    t.resize(size(5, 10));
    t.advance(b"\x1b8X");
    assert_eq!(logical(&t), ["abcdefXhij"]);
}

#[test]
fn the_cursor_row_keeps_its_place_relative_to_the_bottom() {
    // Content that fits the viewport stays at the top.
    let mut t = term(6, 10, "0123456789\r\nabcdefghij\x1b[2;3H");
    t.resize(size(6, 5));
    assert_eq!(pos(&t), (2, 2));
    assert_eq!(t.scrollback_len(), 0);
    // A scrolled screen.
    let mut t = term(4, 10, "");
    for i in 0..10 {
        t.advance(format!("{i}23456789\r\n").as_bytes());
    }
    // Cursor on the last line, one blank row below it.
    t.advance(b"\x1b[3;3H");
    let line = t.row_text(t.row(2).unwrap());
    t.resize(size(4, 5));
    assert_eq!(pos(&t), (2, 2));
    assert_eq!(t.row_text(t.row(2).unwrap()), line[..5]);
    t.resize(size(4, 10));
    assert_eq!(pos(&t), (2, 2));
    assert_eq!(t.row_text(t.row(2).unwrap()), line);
}

#[test]
fn lines_move_into_and_out_of_the_scrollback() {
    let mut t = term(3, 10, "");
    for i in 0..5 {
        t.advance(format!("{i}23456789\r\n").as_bytes());
    }
    t.advance(b"end");
    assert_eq!(t.scrollback_len(), 3);
    let base = t.screen().first_stable();
    t.resize(size(3, 5));
    // 5 lines of two rows, "end", and the cursor on the last row.
    assert_eq!(t.scrollback_len(), 10 + 1 - 3);
    assert_eq!(t.screen().first_stable(), base);
    assert_eq!(logical(&t).last().unwrap(), "end");
    let cursor = t.cursor();
    assert_eq!(cursor.row, 2);
    t.resize(size(3, 10));
    assert_eq!(t.scrollback_len(), 3);
    assert_eq!(lines(&t)[2], "end");
    assert_eq!(logical(&t).len(), 6);
    // Stable indexes agree with positions.
    let s = t.screen();
    for y in 0..3 {
        assert_eq!(
            s.row_by_stable(s.stable_index(y)).unwrap().seqno(),
            s.row(y).unwrap().seqno()
        );
    }
    assert_eq!(
        s.stable_index(0),
        s.first_stable() + s.scrollback_len() as u64
    );
}

#[test]
fn rows_past_the_limit_leave_the_scrollback() {
    let config = Config {
        scrollback: 4,
        ..Config::default()
    };
    let mut t = Terminal::new(size(2, 10), config);
    for i in 0..6 {
        t.advance(format!("{i}23456789\r\n").as_bytes());
    }
    let before = t.screen().first_stable();
    t.resize(size(2, 5));
    assert_eq!(t.scrollback_len(), 4);
    assert!(t.screen().first_stable() > before);
    let s = t.screen();
    assert_eq!(s.stable_index(0), s.first_stable() + 4);
    assert_eq!(logical(&t).last().map(String::as_str), Some("523456789"));
}

#[test]
fn a_scrollback_limit_of_zero_keeps_nothing() {
    let config = Config {
        scrollback: 0,
        ..Config::default()
    };
    let mut t = Terminal::new(size(2, 10), config);
    t.advance(b"0123456789\r\nabcdefghij\r\nlast");
    t.resize(size(2, 4));
    assert_eq!(t.scrollback_len(), 0);
    assert_eq!(lines(&t), ["ij", "last"]);
}

#[test]
fn every_row_gets_a_new_seqno() {
    let mut t = term(4, 10, "");
    for i in 0..6 {
        t.advance(format!("{i}23456789\r\n").as_bytes());
    }
    let before = *seqs(&t).iter().max().unwrap();
    t.resize(size(4, 6));
    assert!(seqs(&t).iter().all(|&s| s > before), "{:?}", seqs(&t));
    let before = *seqs(&t).iter().max().unwrap();
    t.resize(size(4, 14));
    assert!(seqs(&t).iter().all(|&s| s > before));
    // Short lines that move as they are get a new number too.
    let before = *seqs(&t).iter().max().unwrap();
    t.resize(size(4, 15));
    assert!(seqs(&t).iter().all(|&s| s > before));
}

#[test]
fn the_alternate_screen_is_not_reflowed() {
    let mut t = term(4, 10, "abcdefghij\x1b[?1049h\x1b[HZZZZZZZZZZ\r\nyy");
    t.resize(size(4, 5));
    assert_eq!(lines(&t), ["ZZZZZ", "yy", "", ""]);
    assert!(!t.row(0).unwrap().wrapped());
    assert_eq!(t.scrollback_len(), 0);
    t.advance(b"\x1b[?1049l");
    assert_eq!(logical(&t), ["abcdefghij"]);
    assert_eq!(row_texts(&t)[..2], ["abcde", "fghij"]);
    // The cursor came back to where the primary screen left it: on "j" with the wrap
    // waiting.
    let c = t.cursor();
    assert_eq!((c.col, c.pending_wrap), (4, true));
}

#[test]
fn the_primary_cursor_in_the_saved_slot_follows_reflow_on_the_alternate_screen() {
    let mut t = term(4, 10, "abcdefghij\x1b[1;7H\x1b[?1049h");
    t.resize(size(4, 4));
    t.advance(b"\x1b[?1049lX");
    assert_eq!(logical(&t), ["abcdefXhij"]);
}

#[test]
fn marks_follow_their_logical_line() {
    let mut t = term(4, 20, "");
    t.advance(
        b"\x1b]133;A\x07$ a long command line here\r\n\x1b]133;C\x07output text that wraps\r\n",
    );
    t.advance(b"\x1b]133;A\x07$ ");
    let zones = t.semantic_zones();
    let text = |t: &Terminal, z: &cmdterm_vt::SemanticZone| {
        t.row_text(t.row_by_stable(z.start_row).unwrap())
    };
    assert_eq!(zones[0].kind, SemanticKind::Prompt);
    t.resize(size(4, 8));
    let zones = t.semantic_zones();
    assert_eq!(zones[0].kind, SemanticKind::Prompt);
    assert_eq!(text(&t, &zones[0]), "$ a long");
    let out = zones
        .iter()
        .find(|z| z.kind == SemanticKind::Output)
        .unwrap();
    assert_eq!(text(&t, out), "output t");
    let last = zones.last().unwrap();
    assert_eq!(last.kind, SemanticKind::Prompt);
    assert_eq!(text(&t, last), "$");
    t.resize(size(4, 40));
    let zones = t.semantic_zones();
    let out = zones
        .iter()
        .find(|z| z.kind == SemanticKind::Output)
        .unwrap();
    assert_eq!(text(&t, out), "output text that wraps");
    assert_eq!((out.start_row, out.start_col), (out.start_row, 0));
}

#[test]
fn styles_and_links_survive_reflow() {
    let mut t = term(
        4,
        12,
        "\x1b[31;1mred\x1b[0m \x1b]8;id=a;http://example.com/x\x07link text\x1b]8;;\x07 \x1b[44mblue",
    );
    t.resize(size(4, 5));
    t.resize(size(4, 9));
    let rows = all_rows(&t);
    let find = |ch: char| {
        rows.iter()
            .flat_map(|r| r.cells().iter())
            .find(|c| c.ch() == ch)
            .copied()
            .unwrap()
    };
    let r = find('r');
    assert_eq!(
        t.style(r.style).flags & cmdterm_vt::flags::BOLD,
        cmdterm_vt::flags::BOLD
    );
    let l = find('k');
    assert_eq!(t.hyperlink(&l), Some("http://example.com/x"));
    assert_eq!(t.hyperlink_id(&l), Some("a"));
    let b = find('b');
    assert_ne!(t.style(b.style).bg, cmdterm_vt::Color::Default);
    assert_eq!(logical(&t), ["red link text blue"]);
}

#[test]
fn styled_trailing_blanks_are_kept() {
    let mut t = term(3, 8, "ab\x1b[44m   \x1b[0m");
    t.resize(size(3, 6));
    let r = t.row(0).unwrap();
    assert_eq!(r.len(), 5);
    assert_ne!(r.get(4).style, cmdterm_vt::StyleId::DEFAULT);
}

#[test]
fn trailing_default_blanks_go() {
    let mut t = term(3, 8, "ab   \r\ncd");
    t.resize(size(3, 4));
    assert_eq!(lines(&t), ["ab", "cd", ""]);
    assert_eq!(t.row(0).unwrap().len(), 2);
}

#[test]
fn clusters_stay_whole() {
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    let mut t = term(5, 9, &format!("ab\u{e9}\u{301}{family}z"));
    t.resize(size(5, 3));
    let texts = row_texts(&t);
    assert_eq!(texts[0], "ab\u{e9}\u{301}");
    assert_eq!(texts[1], format!("{family}z"));
    t.resize(size(5, 9));
    assert_eq!(logical(&t), [format!("ab\u{e9}\u{301}{family}z")]);
}

#[test]
fn narrowing_cuts_content_below_a_raised_cursor() {
    let mut t = Terminal::new(size(4, 10), Config::default());
    t.advance(b"aaaaaaaaaa\r\nbbbbbbbbbb\r\ncccccccccc\r\ndddddddddd\x1b[H");
    t.resize(size(4, 5));
    assert_eq!(logical(&t), ["aaaaaaaaaa", "bbbbbbbbbb"]);
    assert_eq!(pos(&t), (0, 0));
}

#[test]
fn a_progress_cursor_stays_on_its_line_through_narrow_and_widen() {
    let mut t = term(4, 10, "progress1\r\nprogress2\r\nprogress3\r\n\x1b[3A");
    assert_eq!(pos(&t), (0, 0));
    t.resize(size(4, 5));
    let r = t.cursor().row;
    assert!(lines(&t)[r].starts_with("progr"));
    assert_eq!(r, 0);
    t.resize(size(4, 10));
    assert_eq!(pos(&t), (0, 0));
    assert_eq!(lines(&t)[0], "progress1");
}

#[test]
fn cursor_in_blank_space_is_clamped_not_pending() {
    let mut t = term(3, 20, "ab\r\ncd\x1b[1;10H");
    t.resize(size(3, 6));
    assert_eq!(pos(&t), (0, 5));
    assert!(!t.cursor().pending_wrap);
    t.advance(b"X");
    assert_eq!(lines(&t)[..2], ["ab   X", "cd"]);
}

#[test]
fn cursor_in_blank_space_keeps_the_rows_below() {
    let mut t = term(4, 20, "ab\r\ncd\r\nef\x1b[2;15H");
    t.resize(size(4, 8));
    t.advance(b"Y");
    let l = lines(&t);
    assert_eq!(l[0], "ab");
    assert_eq!(l[2], "ef");
    assert!(l[1].starts_with("cd") && l[1].ends_with('Y'), "{l:?}");
}

fn cases() -> u32 {
    std::env::var("CMDTERM_VT_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

/// Lines of text: ASCII with spaces, wide characters and clusters.
fn text_lines() -> impl Strategy<Value = Vec<String>> {
    let piece = prop_oneof![
        6 => "[a-z0-9 ]{1,12}",
        2 => Just("\u{65e5}\u{672c}".to_string()),
        1 => Just("e\u{301}".to_string()),
        1 => Just("\u{1F468}\u{200D}\u{1F469}".to_string()),
    ];
    let line = prop::collection::vec(piece, 0..8).prop_map(|v| v.concat());
    prop::collection::vec(line, 0..12)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha, rng_seed: proptest::test_runner::RngSeed::Fixed(0x5eed), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn resizes_keep_the_logical_lines(
        text in text_lines(),
        rows in 1usize..8,
        cols in 2usize..24,
        sizes in prop::collection::vec((1usize..8, 2usize..24), 1..6),
        tail in "[a-z]{0,6}",
        jump in prop::option::of((1usize..10, 1usize..30)),
    ) {
        let mut t = Terminal::new(size(rows, cols), Config::default());
        // The last line stays open, so the cursor sits at its end.
        let mut expected: Vec<String> = text.iter().map(|l| l.trim_end().to_string()).collect();
        let open = expected.pop().unwrap_or_default();
        for l in &expected {
            t.advance(l.as_bytes());
            t.advance(b"\r\n");
        }
        t.advance(open.as_bytes());
        expected.push(open.clone());
        if let Some((r, c)) = jump {
            t.advance(format!("\x1b[{r};{c}H").as_bytes());
        }
        let trimmed = |v: &[String]| {
            let mut v = v.to_vec();
            while v.last().is_some_and(String::is_empty) {
                v.pop();
            }
            v
        };
        for (r, c) in sizes {
            // A change of rows alone cuts rows below a raised cursor, as xterm does.
            if jump.is_some() && c == t.size().cols {
                continue;
            }
            t.resize(size(r, c));
            if jump.is_some() {
                // Narrowing may cut lines below the cursor. What stays is a prefix.
                let got = logical(&t);
                let want = trimmed(&expected);
                prop_assert!(got.len() <= want.len());
                for (i, g) in got.iter().enumerate() {
                    if i + 1 < got.len() {
                        prop_assert_eq!(g, &want[i]);
                    } else {
                        prop_assert!(want[i].starts_with(g.as_str()), "{:?} vs {:?}", got, want);
                    }
                }
                expected = got;
                if expected.is_empty() {
                    expected.push(String::new());
                }
            } else {
                prop_assert_eq!(logical(&t), trimmed(&expected));
            }
            let cur = t.cursor();
            prop_assert!(cur.row < r && cur.col < c);
            for (i, row) in all_rows(&t).into_iter().enumerate() {
                prop_assert!(row.len() <= c, "row {} has {} cells", i, row.len());
            }
        }
        if jump.is_some() {
            // Printing after a raised cursor changes at most the cursor's line.
            let before = logical(&t);
            // One character, so a wrap at the right edge cannot legitimately
            // reach the next line.
            t.advance(&tail.as_bytes()[..tail.len().min(1)]);
            let after = logical(&t);
            prop_assert!(after.len() >= before.len());
            let changed = before.iter().zip(&after).filter(|(a, b)| a != b).count();
            prop_assert!(changed <= 1, "{:?} became {:?}", before, after);
            return Ok(());
        }
        // Printing goes on where the open line ended.
        t.advance(tail.as_bytes());
        expected.last_mut().unwrap().push_str(&tail);
        prop_assert_eq!(logical(&t), trimmed(&expected));
    }
}

/// Timing of a reflow of a full scrollback. Run with
/// `cargo test -p cmdterm-vt --release --test reflow -- --ignored --nocapture`.
#[test]
#[ignore = "timing: needs a release build"]
fn reflow_of_ten_thousand_lines_is_fast() {
    use std::time::Instant;
    let config = Config {
        scrollback: 30_000,
        ..Config::default()
    };
    let mut t = Terminal::new(size(24, 120), config);
    let mut input = String::new();
    for i in 0..10_000 {
        let line: String = (0..120)
            .map(|j| char::from(b'a' + ((i + j) % 26) as u8))
            .collect();
        input.push_str(&line);
        input.push_str("\r\n");
    }
    t.advance(input.as_bytes());
    // 10000 lines and the cursor's blank row, minus the 24 of the viewport.
    assert_eq!(t.scrollback_len(), 10_001 - 24);
    let mut best = [u128::MAX; 2];
    for _ in 0..5 {
        let start = Instant::now();
        t.resize(size(24, 80));
        best[0] = best[0].min(start.elapsed().as_micros());
        let start = Instant::now();
        t.resize(size(24, 120));
        best[1] = best[1].min(start.elapsed().as_micros());
    }
    eprintln!(
        "reflow 10000 lines: 120 -> 80 columns {} us, 80 -> 120 columns {} us (best of 5)",
        best[0], best[1]
    );
    if !cfg!(debug_assertions) {
        assert!(best[0] < 5000 && best[1] < 5000, "{best:?}");
    }
}
