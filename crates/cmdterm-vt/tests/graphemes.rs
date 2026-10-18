//! Grapheme clusters in cells: one cell per cluster, the right width, and the
//! interaction with edge, erase, insert and delete.

mod common;

use cmdterm_vt::{Cell, Config, Size, Terminal, Width};
use common::{lines, pos, term};

const E_ACUTE: &str = "e\u{301}";
const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
const DE: &str = "\u{1F1E9}\u{1F1EA}";
const THUMB: &str = "\u{1F44D}\u{1F3FD}";
const HEART16: &str = "\u{2764}\u{FE0F}";
const HEART15: &str = "\u{2764}\u{FE0E}";
const HANGUL: &str = "\u{1100}\u{1161}\u{11A8}";
const ENGLAND: &str = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";

/// Text and width of the cell at column `x` of row `y`.
fn cell(t: &Terminal, y: usize, x: usize) -> (String, Width) {
    let c = t.row(y).unwrap().get(x);
    (t.cell_text(&c).to_string(), c.width)
}

#[test]
fn cell_stays_twelve_bytes() {
    assert_eq!(std::mem::size_of::<Cell>(), 12);
}

#[test]
fn combining_mark_joins_its_base() {
    let t = term(1, 6, "e\u{301}x");
    assert_eq!(lines(&t), vec![format!("{E_ACUTE}x")]);
    assert_eq!(pos(&t), (0, 2));
    assert_eq!(cell(&t, 0, 0), (E_ACUTE.to_string(), Width::Single));
    assert_eq!(cell(&t, 0, 1), ("x".to_string(), Width::Single));
}

#[test]
fn stacked_marks_stay_in_one_cell() {
    let t = term(1, 6, "a\u{300}\u{301}\u{323}b");
    assert_eq!(lines(&t), vec!["a\u{300}\u{301}\u{323}b".to_string()]);
    assert_eq!(pos(&t), (0, 2));
}

#[test]
fn a_cluster_arriving_in_pieces_is_the_same_as_in_one_piece() {
    let mut t = Terminal::new(Size { rows: 1, cols: 6 }, Config::default());
    for b in format!("{FAMILY}z").bytes() {
        t.advance(&[b]);
    }
    assert_eq!(lines(&t), vec![format!("{FAMILY}z")]);
    assert_eq!(pos(&t), (0, 3));
}

#[test]
fn zwj_family_is_one_wide_cell() {
    let t = term(1, 8, &format!("{FAMILY}x"));
    assert_eq!(cell(&t, 0, 0), (FAMILY.to_string(), Width::Wide));
    assert_eq!(cell(&t, 0, 1).1, Width::Continuation);
    assert_eq!(cell(&t, 0, 2).0, "x");
    assert_eq!(pos(&t), (0, 3));
}

#[test]
fn flags_pair_regional_indicators() {
    let t = term(1, 10, &format!("{DE}\u{1F1EB}\u{1F1F7}\u{1F1E9}"));
    assert_eq!(cell(&t, 0, 0), (DE.to_string(), Width::Wide));
    assert_eq!(
        cell(&t, 0, 2),
        ("\u{1F1EB}\u{1F1F7}".to_string(), Width::Wide)
    );
    // The odd indicator is a cluster of its own.
    assert_eq!(cell(&t, 0, 4).0, "\u{1F1E9}");
    assert_eq!(lines(&t), vec![format!("{DE}\u{1F1EB}\u{1F1F7}\u{1F1E9}")]);
}

#[test]
fn tag_sequence_flag_is_one_wide_cell() {
    let t = term(1, 6, &format!("{ENGLAND}x"));
    assert_eq!(cell(&t, 0, 0), (ENGLAND.to_string(), Width::Wide));
    assert_eq!(pos(&t), (0, 3));
}

#[test]
fn skin_tone_modifier_joins_the_emoji() {
    let t = term(1, 6, &format!("{THUMB}x"));
    assert_eq!(cell(&t, 0, 0), (THUMB.to_string(), Width::Wide));
    assert_eq!(pos(&t), (0, 3));
    // On a narrow emoji base the modifier makes the cell wide.
    let t = term(1, 6, "\u{261D}\u{1F3FD}x");
    assert_eq!(
        cell(&t, 0, 0),
        ("\u{261D}\u{1F3FD}".to_string(), Width::Wide)
    );
    assert_eq!(cell(&t, 0, 2).0, "x");
}

#[test]
fn vs16_makes_wide_and_vs15_keeps_narrow() {
    let t = term(1, 6, &format!("{HEART16}x"));
    assert_eq!(cell(&t, 0, 0), (HEART16.to_string(), Width::Wide));
    assert_eq!(cell(&t, 0, 2).0, "x");
    assert_eq!(pos(&t), (0, 3));

    let t = term(1, 6, &format!("{HEART15}x"));
    assert_eq!(cell(&t, 0, 0), (HEART15.to_string(), Width::Single));
    assert_eq!(cell(&t, 0, 1).0, "x");
    assert_eq!(pos(&t), (0, 2));

    // VS15 after a wide emoji makes it narrow and pulls the cursor back.
    let t = term(1, 6, "\u{1F600}\u{FE0E}x");
    assert_eq!(
        cell(&t, 0, 0),
        ("\u{1F600}\u{FE0E}".to_string(), Width::Single)
    );
    assert_eq!(cell(&t, 0, 1).0, "x");
    assert_eq!(pos(&t), (0, 2));
}

#[test]
fn vs16_overwrites_the_next_cell_when_the_cluster_widens() {
    let t = term(1, 6, "\u{2764}xyz\x1b[2G\u{FE0F}");
    assert_eq!(lines(&t), vec![format!("{HEART16}yz")]);
    assert_eq!(pos(&t), (0, 2));
    assert_eq!(cell(&t, 0, 1).1, Width::Continuation);
}

#[test]
fn vs16_in_insert_mode_pushes_the_rest_right() {
    let t = term(1, 6, "\u{2764}xyz\x1b[2G\x1b[4h\u{FE0F}");
    assert_eq!(lines(&t), vec![format!("{HEART16}xyz")]);
    assert_eq!(pos(&t), (0, 2));
}

#[test]
fn vs16_at_the_right_edge_joins_but_stays_narrow() {
    // There is no column to grow into, and the pending wrap stays pending.
    let t = term(2, 3, "ab\u{2764}\u{FE0F}");
    assert_eq!(lines(&t), vec![format!("ab{HEART16}"), String::new()]);
    assert_eq!(cell(&t, 0, 2).1, Width::Single);
    assert_eq!(pos(&t), (0, 2));
    let t = term(2, 3, "ab\u{2764}\u{FE0F}c");
    assert_eq!(lines(&t), vec![format!("ab{HEART16}"), "c".to_string()]);
}

#[test]
fn hangul_jamo_form_one_cell() {
    let t = term(1, 6, &format!("{HANGUL}x"));
    assert_eq!(cell(&t, 0, 0), (HANGUL.to_string(), Width::Wide));
    assert_eq!(cell(&t, 0, 2).0, "x");
    // A precomposed syllable takes a final consonant.
    let t = term(1, 6, "\u{AC00}\u{11A8}x");
    assert_eq!(
        cell(&t, 0, 0),
        ("\u{AC00}\u{11A8}".to_string(), Width::Wide)
    );
    assert_eq!(cell(&t, 0, 2).0, "x");
    // Two syllables are two cells.
    let t = term(1, 6, "\u{AC00}\u{AC01}");
    assert_eq!(pos(&t), (0, 4));
}

#[test]
fn ascii_never_joins_but_spacing_marks_do() {
    let t = term(1, 6, "ab");
    assert_eq!(pos(&t), (0, 2));
    // Devanagari ka + aa: a spacing mark joins its consonant.
    let t = term(1, 6, "\u{915}\u{93E}x");
    assert_eq!(cell(&t, 0, 0).0, "\u{915}\u{93E}");
    assert_eq!(cell(&t, 0, 0).1, Width::Single);
    assert_eq!(cell(&t, 0, 1).0, "x");
}

/// xterm attaches a zero-width character to the cell left of the cursor, wherever the
/// cursor came from, and drops it when there is none. So do we.
#[test]
fn zero_width_after_a_cursor_move_or_at_column_zero() {
    let t = term(1, 4, "\u{301}");
    assert_eq!(lines(&t), vec![String::new()]);
    assert_eq!(pos(&t), (0, 0));

    // Column 0 after a wrap: no cell to its left on this row.
    let t = term(2, 3, "abc\u{301}d\r\u{301}");
    assert_eq!(lines(&t), vec!["abc\u{301}".to_string(), "d".to_string()]);

    // After CUF the cell left of the cursor is a blank and takes the mark.
    let t = term(1, 4, "a\x1b[2C\u{301}");
    assert_eq!(lines(&t), vec!["a  \u{301}".to_string()]);
    assert_eq!(cell(&t, 0, 2), (" \u{301}".to_string(), Width::Single));
    assert_eq!(pos(&t), (0, 3));

    // After CUB it joins the cell left of the new position.
    let t = term(1, 4, "ab\x1b[D\u{301}");
    assert_eq!(cell(&t, 0, 0).0, "a\u{301}");
    assert_eq!(cell(&t, 0, 1).0, "b");

    // A zero-width character that never joins is dropped.
    let t = term(1, 4, "a\u{200B}b");
    assert_eq!(lines(&t), vec!["ab".to_string()]);
}

#[test]
fn rep_repeats_the_last_base_character() {
    let t = term(1, 8, "e\u{301}\x1b[2b");
    assert_eq!(lines(&t), vec![format!("{E_ACUTE}ee")]);
}

#[test]
fn rep_never_merges_with_itself() {
    // Hangul jamo and regional indicators would join if REP printed like normal text.
    let t = term(1, 8, "\u{1100}\x1b[2b");
    assert_eq!(cell(&t, 0, 0).0, "\u{1100}");
    // Jamo are wide: three cells of two columns.
    assert_eq!(cell(&t, 0, 2).0, "\u{1100}");
    assert_eq!(cell(&t, 0, 4).0, "\u{1100}");
    assert_eq!(t.cursor().col, 6);

    let t = term(1, 8, "\u{1F1E9}\x1b[b");
    assert_eq!(cell(&t, 0, 0).0, "\u{1F1E9}");
    assert_eq!(cell(&t, 0, 1).0, "\u{1F1E9}");
    assert_eq!(t.cursor().col, 2);
}

#[test]
fn prepend_run_followed_by_ascii_is_one_cluster() {
    let t = term(1, 8, "\u{600}\u{600}1");
    assert_eq!(cell(&t, 0, 0).0, "\u{600}\u{600}1");
    assert_eq!(t.cursor().col, 1);
}

#[test]
fn cluster_wraps_whole_at_the_right_edge() {
    let t = term(2, 3, &format!("ab{FAMILY}"));
    assert_eq!(lines(&t), vec!["ab".to_string(), FAMILY.to_string()]);
    assert!(t.row(0).unwrap().wrapped());
    assert_eq!(pos(&t), (1, 2));
}

#[test]
fn erasing_half_a_wide_cluster_blanks_both_halves() {
    // EL from the continuation half.
    let t = term(1, 8, &format!("a{FAMILY}b\x1b[3G\x1b[K"));
    assert_eq!(lines(&t), vec!["a".to_string()]);
    // ECH over the left half only.
    let t = term(1, 8, &format!("a{FAMILY}b\x1b[2G\x1b[X"));
    assert_eq!(lines(&t), vec!["a  b".to_string()]);
    // ECH over a narrow cluster leaves the neighbors.
    let t = term(1, 8, &format!("a{E_ACUTE}b\x1b[2G\x1b[X"));
    assert_eq!(lines(&t), vec!["a b".to_string()]);
}

#[test]
fn insert_and_delete_move_wide_clusters_whole() {
    let t = term(1, 8, &format!("a{FAMILY}b\r\x1b[2@"));
    assert_eq!(lines(&t), vec![format!("  a{FAMILY}b")]);
    let t = term(1, 8, &format!("a{FAMILY}b\r\x1b[P"));
    assert_eq!(lines(&t), vec![format!("{FAMILY}b")]);
    // Deleting into the middle cuts a cluster that starts left of the span.
    let t = term(1, 8, &format!("a{FAMILY}b\x1b[3G\x1b[P"));
    assert_eq!(lines(&t), vec!["a b".to_string()]);
    // Inserting inside it cuts it too.
    let t = term(1, 8, &format!("a{FAMILY}b\x1b[3G\x1b[@"));
    assert_eq!(lines(&t), vec!["a   b".to_string()]);
    // IRM pushes a cluster right.
    let t = term(1, 8, &format!("{E_ACUTE}{FAMILY}\r\x1b[4hx"));
    assert_eq!(lines(&t), vec![format!("x{E_ACUTE}{FAMILY}")]);
}

#[test]
fn overwriting_a_cluster_replaces_all_of_it() {
    let t = term(1, 8, &format!("{FAMILY}\rxy"));
    assert_eq!(lines(&t), vec!["xy".to_string()]);
    let t = term(1, 8, &format!("{E_ACUTE}\rx"));
    assert_eq!(lines(&t), vec!["x".to_string()]);
}

#[test]
fn resize_moves_a_cluster_whole_to_the_next_row() {
    let mut t = term(2, 6, &format!("ab{FAMILY}"));
    t.resize(Size { rows: 2, cols: 3 });
    assert_eq!(lines(&t), vec!["ab".to_string(), FAMILY.to_string()]);
    assert!(t.row(0).unwrap().wrapped());
    assert_eq!(cell(&t, 1, 0), (FAMILY.to_string(), Width::Wide));
    assert_eq!(cell(&t, 1, 1).1, Width::Continuation);
    // And back: the gap left at the end of the first row is not text.
    t.resize(Size { rows: 2, cols: 6 });
    assert_eq!(lines(&t)[0], format!("ab{FAMILY}"));
}

#[test]
fn a_changed_cluster_gets_a_new_seqno() {
    let mut t = term(1, 6, "e");
    let s = t.row(0).unwrap().seqno();
    t.advance("\u{301}".as_bytes());
    assert_ne!(t.row(0).unwrap().seqno(), s);
}

#[test]
fn cluster_ids_are_reused_after_the_cells_are_gone() {
    let mut t = Terminal::new(Size { rows: 2, cols: 200 }, Config::default());
    let marks = ['\u{300}', '\u{301}', '\u{302}', '\u{303}', '\u{304}'];
    let mut buf = String::new();
    let mut n = 0u32;
    let mut cp = 0x4E00u32;
    while n < 70_000 {
        buf.clear();
        buf.push_str("\x1b[2J\x1b[H");
        for _ in 0..100 {
            let base = char::from_u32(cp).unwrap();
            buf.push(base);
            buf.push(marks[(n % 5) as usize]);
            buf.push(marks[((n / 5) % 5) as usize]);
            cp += 1;
            if cp > 0x9FFF {
                cp = 0x4E00;
            }
            n += 1;
        }
        t.advance(buf.as_bytes());
    }
    // The table filled and was swept; the newest line is intact.
    let line = lines(&t)[0].clone();
    assert_eq!(line.chars().filter(|c| *c >= '\u{4E00}').count(), 100);
    assert!(line.contains('\u{300}') || line.contains('\u{301}'));
}

#[test]
fn printing_clusters_into_a_warm_screen_does_not_allocate() {
    let mut t = Terminal::new(Size { rows: 3, cols: 40 }, Config::default());
    let text = format!(
        "plain {E_ACUTE} {FAMILY} {DE} {THUMB} {HEART16} {HANGUL} x\u{301}\u{302} \u{2764}\u{FE0F}"
    );
    let bytes = text.as_bytes();
    t.advance(bytes);
    t.advance(b"\r");
    let info = allocation_counter::measure(|| {
        t.advance(bytes);
        t.advance(b"\r");
    });
    assert_eq!(info.count_total, 0, "{info:?}");
}

#[test]
fn cell_text_does_not_allocate() {
    let t = term(1, 8, &format!("a{FAMILY}\u{4E16}"));
    let row = t.row(0).unwrap();
    let info = allocation_counter::measure(|| {
        for x in 0..6 {
            let c = row.get(x);
            std::hint::black_box(t.cell_text(&c));
        }
    });
    assert_eq!(info.count_total, 0);
}

/// Distinct two-mark clusters over CJK bases, as many as asked for.
fn unique_clusters(n: usize) -> impl Iterator<Item = String> {
    let marks = ['\u{301}', '\u{300}', '\u{302}'];
    (0..n).map(move |i| {
        let base = char::from_u32(0x3400 + (i / 3) as u32).unwrap();
        format!("{base}{}", marks[i % 3])
    })
}

#[test]
fn a_full_table_of_live_clusters_does_not_sweep_per_character() {
    use cmdterm_vt::{Config, Size, Terminal};
    let mut t = Terminal::new(
        Size {
            rows: 700,
            cols: 200,
        },
        Config::default(),
    );
    let mut input = String::new();
    for c in unique_clusters(65535) {
        input.push_str(&c);
    }
    t.advance(input.as_bytes());
    let before = t.cluster_sweeps();
    let mut more = String::from("\x1b[H");
    for c in unique_clusters(65535 + 2000).skip(65535) {
        more.push_str(&c);
    }
    t.advance(more.as_bytes());
    // Few cells were written, so a full table is rescanned at most once.
    assert!(t.cluster_sweeps() - before <= 1);
}

/// A sweep must not free the id handed out for the cluster being written.
#[test]
fn a_sweep_during_growth_keeps_the_new_cluster() {
    let config = Config {
        scrollback: 4,
        ..Config::default()
    };
    let mut t = Terminal::new(Size { rows: 2, cols: 10 }, config);
    for i in 0..600u32 {
        let base = char::from_u32(0x4E00 + i).unwrap();
        t.advance(format!("\r\n{base}\u{301}").as_bytes());
        let c = t.row(1).unwrap().get(0);
        assert_eq!(t.cell_text(&c), format!("{base}\u{301}"), "line {i}");
    }
    assert!(t.cluster_sweeps() > 0);
}

/// Overwriting never drops rows, yet a full table must still be collected.
#[test]
fn redrawing_on_the_alternate_screen_collects_clusters() {
    let mut t = Terminal::new(Size { rows: 2, cols: 100 }, Config::default());
    t.advance(b"\x1b[?1049h");
    for c in unique_clusters(70_000) {
        t.advance(b"\x1b[H");
        t.advance(c.as_bytes());
    }
    let last = unique_clusters(70_000).last().unwrap();
    let cell = t.row(0).unwrap().get(0);
    assert_eq!(t.cell_text(&cell), last);
    assert!(t.cluster_sweeps() > 0);
}

#[test]
fn a_spacing_mark_before_a_joiner_blocks_the_emoji_sequence() {
    let text = "\u{1F600}\u{0903}\u{200D}\u{1F600}";
    let mut t = Terminal::new(Size { rows: 2, cols: 10 }, Config::default());
    t.advance(text.as_bytes());
    let mut got = Vec::new();
    for x in 0..10 {
        let c = t.row(0).unwrap().get(x);
        let s = t.cell_text(&c);
        if !s.is_empty() && s != " " {
            got.push(s.to_string());
        }
    }
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got.concat(), text);
}

#[test]
fn zwj_fast_path_matches_segmentation() {
    use unicode_segmentation::UnicodeSegmentation;
    let cases = [
        "\u{1F600}\u{200D}\u{200D}\u{1F600}",
        "\u{1F600}\u{200D}\u{300}\u{200D}\u{1F600}",
        "\u{1F3FB}\u{200D}\u{1F600}",
        "\u{1F600}\u{200D}\u{1F600}",
        "\u{1F600}\u{300}\u{200D}\u{1F600}",
    ];
    for s in cases {
        let t = term(1, 20, s);
        let mut x = 0;
        for g in s.graphemes(true) {
            let (text, w) = cell(&t, 0, x);
            assert_eq!(text, g, "{s:?} at column {x}");
            x += if w == Width::Wide { 2 } else { 1 };
        }
        assert_eq!(pos(&t).1, x, "{s:?} cursor");
    }
}
