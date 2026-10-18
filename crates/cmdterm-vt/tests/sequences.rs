mod common;
use cmdterm_vt::{Color, Event, OscEnd, Underline, Width, flags};
use common::*;

struct Case {
    name: &'static str,
    rows: usize,
    cols: usize,
    input: &'static str,
    text: &'static [&'static str],
    cursor: (usize, usize),
}

fn run(cases: &[Case]) {
    for c in cases {
        let t = term(c.rows, c.cols, c.input);
        assert_eq!(lines(&t), c.text, "text of {}", c.name);
        assert_eq!(pos(&t), c.cursor, "cursor of {}", c.name);
    }
}

#[test]
fn printing_and_wrap() {
    run(&[
        Case {
            name: "plain",
            rows: 2,
            cols: 5,
            input: "abc",
            text: &["abc", ""],
            cursor: (0, 3),
        },
        Case {
            name: "pending wrap stays",
            rows: 2,
            cols: 5,
            input: "abcde",
            text: &["abcde", ""],
            cursor: (0, 4),
        },
        Case {
            name: "wrap",
            rows: 2,
            cols: 5,
            input: "abcdef",
            text: &["abcde", "f"],
            cursor: (1, 1),
        },
        Case {
            name: "cr after pending",
            rows: 2,
            cols: 5,
            input: "abcde\rX",
            text: &["Xbcde", ""],
            cursor: (0, 1),
        },
        Case {
            name: "scroll on wrap",
            rows: 2,
            cols: 3,
            input: "abcdefg",
            text: &["def", "g"],
            cursor: (1, 1),
        },
        Case {
            name: "no autowrap",
            rows: 2,
            cols: 3,
            input: "\x1b[?7labcdef",
            text: &["abf", ""],
            cursor: (0, 2),
        },
        Case {
            name: "wide",
            rows: 1,
            cols: 6,
            input: "a日b",
            text: &["a日b"],
            cursor: (0, 4),
        },
        Case {
            name: "wide wraps at last col",
            rows: 2,
            cols: 3,
            input: "ab日",
            text: &["ab", "日"],
            cursor: (1, 2),
        },
        Case {
            name: "overwrite half of wide",
            rows: 1,
            cols: 4,
            input: "日\rx",
            text: &["x"],
            cursor: (0, 1),
        },
        Case {
            name: "overwrite continuation",
            rows: 1,
            cols: 4,
            input: "日\x1b[2Gx",
            text: &[" x"],
            cursor: (0, 2),
        },
    ]);
}

#[test]
fn wide_cells() {
    let t = term(1, 4, "日");
    let r = t.row(0).unwrap();
    assert_eq!(r.get(0).width, Width::Wide);
    assert_eq!(r.get(1).width, Width::Continuation);
    assert_eq!(r.len(), 2);
}

/// Found by the differential harness: inserting at a wide character used to blank its
/// right half and lose the whole character.
#[test]
fn insert_before_wide_character() {
    run(&[
        Case {
            name: "irm",
            rows: 1,
            cols: 7,
            input: "\x1b[4h日本\r0",
            text: &["0日本"],
            cursor: (0, 1),
        },
        Case {
            name: "ich",
            rows: 1,
            cols: 7,
            input: "日本\r\x1b[2@",
            text: &["  日本"],
            cursor: (0, 0),
        },
        Case {
            name: "ich inside a wide character",
            rows: 1,
            cols: 7,
            input: "日本\x1b[2G\x1b[@",
            text: &["   本"],
            cursor: (0, 1),
        },
    ]);
}

/// Found by the differential harness: filling the last column with wrap-around off left a
/// wrap pending that turning wrap-around on later acted on.
#[test]
fn no_pending_wrap_without_autowrap() {
    run(&[Case {
        name: "wrap turned on after a full line",
        rows: 2,
        cols: 4,
        input: "\x1b[?7laaaa\x1b[?7hA",
        text: &["aaaA", ""],
        cursor: (0, 3),
    }]);
}

#[test]
fn wrapped_flag() {
    let t = term(3, 3, "abcdef\r\nx");
    assert!(t.row(0).unwrap().wrapped());
    assert!(!t.row(1).unwrap().wrapped());
}

#[test]
fn c0_controls() {
    run(&[
        Case {
            name: "bs",
            rows: 1,
            cols: 5,
            input: "ab\x08c",
            text: &["ac"],
            cursor: (0, 2),
        },
        Case {
            name: "bs at 0",
            rows: 1,
            cols: 5,
            input: "\x08\x08a",
            text: &["a"],
            cursor: (0, 1),
        },
        Case {
            name: "ht",
            rows: 1,
            cols: 20,
            input: "a\tb",
            text: &["a       b"],
            cursor: (0, 9),
        },
        Case {
            name: "ht at end",
            rows: 1,
            cols: 10,
            input: "\t\t\t\tx",
            text: &["         x"],
            cursor: (0, 9),
        },
        Case {
            name: "lf keeps col",
            rows: 3,
            cols: 5,
            input: "ab\ncd",
            text: &["ab", "  cd", ""],
            cursor: (1, 4),
        },
        Case {
            name: "vt ff",
            rows: 3,
            cols: 5,
            input: "a\x0b\x0cb",
            text: &["a", "", " b"],
            cursor: (2, 2),
        },
        Case {
            name: "crlf",
            rows: 2,
            cols: 5,
            input: "ab\r\ncd",
            text: &["ab", "cd"],
            cursor: (1, 2),
        },
    ]);
}

#[test]
fn bell_event() {
    let mut t = term(1, 5, "a\x07b");
    assert_eq!(t.drain_events().collect::<Vec<_>>(), vec![Event::Bell]);
    assert!(t.drain_events().next().is_none());
}

#[test]
fn esc_sequences() {
    run(&[
        Case {
            name: "ind scrolls",
            rows: 2,
            cols: 3,
            input: "a\nb\x1bD",
            text: &[" b", ""],
            cursor: (1, 2),
        },
        Case {
            name: "nel",
            rows: 3,
            cols: 4,
            input: "ab\x1bEcd",
            text: &["ab", "cd", ""],
            cursor: (1, 2),
        },
        Case {
            name: "ri",
            rows: 3,
            cols: 4,
            input: "a\nb\x1bM",
            text: &["a", " b", ""],
            cursor: (0, 2),
        },
        Case {
            name: "ri scrolls down",
            rows: 2,
            cols: 3,
            input: "ab\x1bM",
            text: &["", "ab"],
            cursor: (0, 2),
        },
        Case {
            name: "decsc decrc",
            rows: 3,
            cols: 5,
            input: "ab\x1b7\r\n\r\ncd\x1b8X",
            text: &["abX", "", "cd"],
            cursor: (0, 3),
        },
        Case {
            name: "decrc w/o save",
            rows: 3,
            cols: 5,
            input: "\n\nab\x1b8X",
            text: &["X", "", "ab"],
            cursor: (0, 1),
        },
        Case {
            name: "hts",
            rows: 1,
            cols: 12,
            input: "\x1b[3g\x1b[4G\x1bH\r\tX",
            text: &["   X"],
            cursor: (0, 4),
        },
        Case {
            name: "ris",
            rows: 2,
            cols: 4,
            input: "ab\x1b[2;2H\x1b[31m\x1bcX",
            text: &["X", ""],
            cursor: (0, 1),
        },
        Case {
            name: "decaln",
            rows: 2,
            cols: 3,
            input: "\x1b#8",
            text: &["EEE", "EEE"],
            cursor: (0, 0),
        },
    ]);
}

#[test]
fn charsets() {
    run(&[
        Case {
            name: "g0 special",
            rows: 1,
            cols: 6,
            input: "\x1b(0lqk\x1b(Blqk",
            text: &["┌─┐lqk"],
            cursor: (0, 5),
        },
        Case {
            name: "so si",
            rows: 1,
            cols: 6,
            input: "\x1b)0x\x0ex\x0fx",
            text: &["x│x"],
            cursor: (0, 3),
        },
        Case {
            name: "uk",
            rows: 1,
            cols: 4,
            input: "\x1b(A#a",
            text: &["£a"],
            cursor: (0, 2),
        },
        Case {
            name: "ss2 as c1",
            rows: 1,
            cols: 4,
            input: "\x1b*0\u{8e}qq",
            text: &["─q"],
            cursor: (0, 2),
        },
        Case {
            name: "ss3 as c1",
            rows: 1,
            cols: 4,
            input: "\x1b+0\u{8f}qq",
            text: &["─q"],
            cursor: (0, 2),
        },
        Case {
            name: "ss2",
            rows: 1,
            cols: 4,
            input: "\x1b*0\x1bNqq",
            text: &["─q"],
            cursor: (0, 2),
        },
        Case {
            name: "ss3",
            rows: 1,
            cols: 4,
            input: "\x1b+0\x1bOqq",
            text: &["─q"],
            cursor: (0, 2),
        },
        Case {
            name: "ls2",
            rows: 1,
            cols: 4,
            input: "\x1b*0\x1bnq\x0fq",
            text: &["─q"],
            cursor: (0, 2),
        },
        Case {
            name: "unicode untouched",
            rows: 1,
            cols: 4,
            input: "\x1b(0é",
            text: &["é"],
            cursor: (0, 1),
        },
    ]);
}

#[test]
fn cursor_movement() {
    run(&[
        Case {
            name: "cuu",
            rows: 5,
            cols: 5,
            input: "\n\n\n\x1b[2A",
            text: &["", "", "", "", ""],
            cursor: (1, 0),
        },
        Case {
            name: "cuu clamps",
            rows: 5,
            cols: 5,
            input: "\n\x1b[9A",
            text: &["", "", "", "", ""],
            cursor: (0, 0),
        },
        Case {
            name: "cud",
            rows: 5,
            cols: 5,
            input: "\x1b[2B",
            text: &["", "", "", "", ""],
            cursor: (2, 0),
        },
        Case {
            name: "cud clamps",
            rows: 5,
            cols: 5,
            input: "\x1b[99B",
            text: &["", "", "", "", ""],
            cursor: (4, 0),
        },
        Case {
            name: "cuf",
            rows: 1,
            cols: 10,
            input: "\x1b[3Cx",
            text: &["   x"],
            cursor: (0, 4),
        },
        Case {
            name: "cuf clamps",
            rows: 1,
            cols: 5,
            input: "\x1b[99C",
            text: &[""],
            cursor: (0, 4),
        },
        Case {
            name: "cub",
            rows: 1,
            cols: 10,
            input: "abc\x1b[2Dx",
            text: &["axc"],
            cursor: (0, 2),
        },
        Case {
            name: "cnl",
            rows: 3,
            cols: 5,
            input: "ab\x1b[2Ex",
            text: &["ab", "", "x"],
            cursor: (2, 1),
        },
        Case {
            name: "cpl",
            rows: 3,
            cols: 5,
            input: "\n\nab\x1b[2Fx",
            text: &["x", "", "ab"],
            cursor: (0, 1),
        },
        Case {
            name: "cha",
            rows: 1,
            cols: 10,
            input: "abc\x1b[6Gx",
            text: &["abc  x"],
            cursor: (0, 6),
        },
        Case {
            name: "hpa",
            rows: 1,
            cols: 10,
            input: "\x1b[4`x",
            text: &["   x"],
            cursor: (0, 4),
        },
        Case {
            name: "vpa",
            rows: 4,
            cols: 5,
            input: "a\x1b[3dx",
            text: &["a", "", " x", ""],
            cursor: (2, 2),
        },
        Case {
            name: "cup",
            rows: 4,
            cols: 5,
            input: "\x1b[3;4Hx",
            text: &["", "", "   x", ""],
            cursor: (2, 4),
        },
        Case {
            name: "cup default",
            rows: 4,
            cols: 5,
            input: "\n\n\x1b[Hx",
            text: &["x", "", "", ""],
            cursor: (0, 1),
        },
        Case {
            name: "hvp",
            rows: 4,
            cols: 5,
            input: "\x1b[2;2fx",
            text: &["", " x", "", ""],
            cursor: (1, 2),
        },
        Case {
            name: "cup clamps",
            rows: 2,
            cols: 3,
            input: "\x1b[9;9Hx",
            text: &["", "  x"],
            cursor: (1, 2),
        },
        Case {
            name: "cht",
            rows: 1,
            cols: 30,
            input: "\x1b[2Ix",
            text: &["                x"],
            cursor: (0, 17),
        },
        Case {
            name: "cbt",
            rows: 1,
            cols: 30,
            input: "\x1b[20G\x1b[Zx",
            text: &["                x"],
            cursor: (0, 17),
        },
        Case {
            name: "cbt clamps",
            rows: 1,
            cols: 30,
            input: "\x1b[5G\x1b[9Zx",
            text: &["x"],
            cursor: (0, 1),
        },
    ]);
}

#[test]
fn erase() {
    run(&[
        Case {
            name: "ed0",
            rows: 3,
            cols: 4,
            input: "aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[J",
            text: &["aaaa", "bb", ""],
            cursor: (1, 2),
        },
        Case {
            name: "ed1",
            rows: 3,
            cols: 4,
            input: "aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[1J",
            text: &["", "   b", "cccc"],
            cursor: (1, 2),
        },
        Case {
            name: "ed2",
            rows: 2,
            cols: 4,
            input: "aaaa\r\nbbbb\x1b[2J",
            text: &["", ""],
            cursor: (1, 3),
        },
        Case {
            name: "el0",
            rows: 1,
            cols: 6,
            input: "abcdef\x1b[3G\x1b[K",
            text: &["ab"],
            cursor: (0, 2),
        },
        Case {
            name: "el1",
            rows: 1,
            cols: 6,
            input: "abcdef\x1b[3G\x1b[1K",
            text: &["   def"],
            cursor: (0, 2),
        },
        Case {
            name: "el2",
            rows: 1,
            cols: 6,
            input: "abcdef\x1b[2K",
            text: &[""],
            cursor: (0, 5),
        },
        Case {
            name: "ech",
            rows: 1,
            cols: 6,
            input: "abcdef\x1b[2G\x1b[3X",
            text: &["a   ef"],
            cursor: (0, 1),
        },
        Case {
            name: "decsed",
            rows: 2,
            cols: 4,
            input: "aaaa\r\nbbbb\x1b[1;2H\x1b[?J",
            text: &["a", ""],
            cursor: (0, 1),
        },
        Case {
            name: "decsel",
            rows: 1,
            cols: 4,
            input: "abcd\x1b[2G\x1b[?2K",
            text: &[""],
            cursor: (0, 1),
        },
        Case {
            name: "ech wide cut",
            rows: 1,
            cols: 6,
            input: "a日b\x1b[3G\x1b[X",
            text: &["a  b"],
            cursor: (0, 2),
        },
    ]);
}

#[test]
fn ed3_clears_scrollback() {
    let mut t = term(2, 3, "a\r\nb\r\nc\r\nd");
    assert_eq!(t.scrollback_len(), 2);
    let stable = t.stable_index(0);
    t.advance(b"\x1b[3J");
    assert_eq!(t.scrollback_len(), 0);
    assert_eq!(t.stable_index(0), stable);
    assert_eq!(lines(&t), ["c", "d"]);
}

#[test]
fn insert_delete() {
    run(&[
        Case {
            name: "ich",
            rows: 1,
            cols: 6,
            input: "abcd\x1b[2G\x1b[2@",
            text: &["a  bcd"],
            cursor: (0, 1),
        },
        Case {
            name: "ich cut",
            rows: 1,
            cols: 4,
            input: "abcd\x1b[2G\x1b[9@",
            text: &["a"],
            cursor: (0, 1),
        },
        Case {
            name: "dch",
            rows: 1,
            cols: 6,
            input: "abcdef\x1b[2G\x1b[2P",
            text: &["adef"],
            cursor: (0, 1),
        },
        Case {
            name: "il",
            rows: 4,
            cols: 3,
            input: "a\r\nb\r\nc\x1b[2;1H\x1b[L",
            text: &["a", "", "b", "c"],
            cursor: (1, 0),
        },
        Case {
            name: "dl",
            rows: 4,
            cols: 3,
            input: "a\r\nb\r\nc\x1b[2;1H\x1b[M",
            text: &["a", "c", "", ""],
            cursor: (1, 0),
        },
        Case {
            name: "dl big",
            rows: 3,
            cols: 3,
            input: "a\r\nb\r\nc\x1b[2;1H\x1b[9M",
            text: &["a", "", ""],
            cursor: (1, 0),
        },
        Case {
            name: "irm",
            rows: 1,
            cols: 6,
            input: "abc\x1b[4h\x1b[1GX\x1b[4lY",
            text: &["XYbc"],
            cursor: (0, 2),
        },
        Case {
            name: "dch splits wide",
            rows: 1,
            cols: 6,
            input: "日b\x1b[1G\x1b[P",
            text: &[" b"],
            cursor: (0, 0),
        },
    ]);
}

#[test]
fn scrolling() {
    run(&[
        Case {
            name: "su",
            rows: 3,
            cols: 3,
            input: "a\r\nb\r\nc\x1b[2S",
            text: &["c", "", ""],
            cursor: (2, 1),
        },
        Case {
            name: "sd",
            rows: 3,
            cols: 3,
            input: "a\r\nb\r\nc\x1b[S\x1b[T\x1b[T",
            text: &["", "", "b"],
            cursor: (2, 1),
        },
        Case {
            name: "decstbm",
            rows: 5,
            cols: 3,
            input: "a\r\nb\r\nc\r\nd\r\ne\x1b[2;4r\x1b[4;1H\n",
            text: &["a", "c", "d", "", "e"],
            cursor: (3, 0),
        },
        Case {
            name: "decstbm homes",
            rows: 5,
            cols: 3,
            input: "\n\n\x1b[2;4rx",
            text: &["x", "", "", "", ""],
            cursor: (0, 1),
        },
        Case {
            name: "decstbm invalid",
            rows: 5,
            cols: 3,
            input: "\x1b[4;2r\x1b[3;3rx",
            text: &["x", "", "", "", ""],
            cursor: (0, 1),
        },
        Case {
            name: "ri in region",
            rows: 5,
            cols: 3,
            input: "a\r\nb\r\nc\r\nd\x1b[2;4r\x1b[2;1H\x1bM",
            text: &["a", "", "b", "c", ""],
            cursor: (1, 0),
        },
        Case {
            name: "il outside region ignored",
            rows: 5,
            cols: 3,
            input: "a\r\nb\x1b[2;3r\x1b[1;1H\x1b[L",
            text: &["a", "b", "", "", ""],
            cursor: (0, 0),
        },
        Case {
            name: "origin mode",
            rows: 5,
            cols: 5,
            input: "\x1b[2;4r\x1b[?6h\x1b[1;1Hx\x1b[9;9Hy",
            text: &["", "x", "", "    y", ""],
            cursor: (3, 4),
        },
    ]);
}

#[test]
fn left_right_margins() {
    run(&[
        Case {
            name: "declrmm wrap",
            rows: 3,
            cols: 8,
            input: "\x1b[?69h\x1b[3;6s\x1b[1;3Habcdef",
            text: &["  abcd", "  ef", ""],
            cursor: (1, 4),
        },
        Case {
            name: "cr to left margin",
            rows: 2,
            cols: 8,
            input: "\x1b[?69h\x1b[3;6s\x1b[1;5H\rx",
            text: &["  x", ""],
            cursor: (0, 3),
        },
        Case {
            name: "dch within margins",
            rows: 1,
            cols: 8,
            input: "abcdefgh\x1b[?69h\x1b[2;5s\x1b[1;2H\x1b[P",
            text: &["acde fgh"],
            cursor: (0, 1),
        },
        Case {
            name: "su within margins",
            rows: 3,
            cols: 6,
            input: "aaaaaa\r\nbbbbbb\r\ncccccc\x1b[?69h\x1b[2;4s\x1b[S",
            text: &["abbbaa", "bcccbb", "c   cc"],
            cursor: (0, 0),
        },
        Case {
            name: "csi s saves without lrmm",
            rows: 1,
            cols: 5,
            input: "ab\x1b[s\x1b[1Gx\x1b[uy",
            text: &["xby"],
            cursor: (0, 3),
        },
        Case {
            name: "reset mode 69",
            rows: 1,
            cols: 6,
            input: "\x1b[?69h\x1b[2;3s\x1b[?69l\x1b[1;1Habcdef",
            text: &["abcdef"],
            cursor: (0, 5),
        },
    ]);
}

#[test]
fn rep_and_tabs() {
    run(&[
        Case {
            name: "rep",
            rows: 1,
            cols: 10,
            input: "a\x1b[3b",
            text: &["aaaa"],
            cursor: (0, 4),
        },
        Case {
            name: "rep without char",
            rows: 1,
            cols: 10,
            input: "\x1b[3b",
            text: &[""],
            cursor: (0, 0),
        },
        Case {
            name: "tbc 3",
            rows: 1,
            cols: 20,
            input: "\x1b[3g\tx",
            text: &["                   x"],
            cursor: (0, 19),
        },
        Case {
            name: "tbc 0",
            rows: 1,
            cols: 20,
            input: "\x1b[9G\x1b[g\r\tx",
            text: &["                x"],
            cursor: (0, 17),
        },
    ]);
}

#[test]
fn sgr_attributes() {
    let sty = |s: &str| {
        let t = term(1, 4, &format!("\x1b[{s}mx"));
        let id = t.row(0).unwrap().get(0).style;
        *t.style(id)
    };
    assert!(sty("1").has(flags::BOLD));
    assert!(sty("2").has(flags::FAINT));
    assert!(sty("3").has(flags::ITALIC));
    assert!(sty("5").has(flags::BLINK));
    assert!(sty("7").has(flags::INVERSE));
    assert!(sty("8").has(flags::INVISIBLE));
    assert!(sty("9").has(flags::STRIKE));
    assert!(!sty("53").has(flags::BOLD));
    assert_eq!(sty("53"), Default::default());
    assert_eq!(sty("4").underline, Underline::Single);
    assert_eq!(sty("21").underline, Underline::Double);
    for (n, u) in [
        (0, Underline::None),
        (1, Underline::Single),
        (2, Underline::Double),
        (3, Underline::Curly),
        (4, Underline::Dotted),
        (5, Underline::Dashed),
    ] {
        assert_eq!(sty(&format!("4:{n}")).underline, u);
    }
    assert!(!sty("1;22").has(flags::BOLD));
    assert!(!sty("1;2;22").has(flags::FAINT));
    assert!(!sty("3;23").has(flags::ITALIC));
    assert_eq!(sty("4;24").underline, Underline::None);
    assert!(!sty("5;25").has(flags::BLINK));
    assert!(!sty("7;27").has(flags::INVERSE));
    assert!(!sty("8;28").has(flags::INVISIBLE));
    assert!(!sty("9;29").has(flags::STRIKE));
    assert_eq!(sty("1;3;31;42;0"), Default::default());
    assert_eq!(sty(""), Default::default());
    assert_eq!(sty("31").fg, Color::Palette(1));
    assert_eq!(sty("37").fg, Color::Palette(7));
    assert_eq!(sty("41").bg, Color::Palette(1));
    assert_eq!(sty("47").bg, Color::Palette(7));
    assert_eq!(sty("90").fg, Color::Palette(8));
    assert_eq!(sty("97").fg, Color::Palette(15));
    assert_eq!(sty("100").bg, Color::Palette(8));
    assert_eq!(sty("107").bg, Color::Palette(15));
    assert_eq!(sty("31;39").fg, Color::Default);
    assert_eq!(sty("41;49").bg, Color::Default);
    assert_eq!(sty("38;5;200").fg, Color::Palette(200));
    assert_eq!(sty("48;5;17").bg, Color::Palette(17));
    assert_eq!(sty("38;2;1;2;3").fg, Color::Rgb(1, 2, 3));
    assert_eq!(sty("48;2;4;5;6").bg, Color::Rgb(4, 5, 6));
    assert_eq!(sty("38:5:200").fg, Color::Palette(200));
    assert_eq!(sty("38:2:1:2:3").fg, Color::Rgb(1, 2, 3));
    assert_eq!(sty("38:2::1:2:3").fg, Color::Rgb(1, 2, 3));
    assert_eq!(sty("48:2::4:5:6").bg, Color::Rgb(4, 5, 6));
    assert_eq!(sty("58;2;7;8;9").underline_color, Color::Rgb(7, 8, 9));
    assert_eq!(sty("58:5:9").underline_color, Color::Palette(9));
    assert_eq!(sty("58;5;9;59").underline_color, Color::Default);
    assert_eq!(sty("38;5;1;1").flags, flags::BOLD);
    assert_eq!(sty("38;2;1;2;3;4").underline, Underline::Single);
    assert_eq!(sty("38;5;300").fg, Color::Palette(255));
}

#[test]
fn sgr_applies_to_following_cells_only() {
    let t = term(1, 6, "a\x1b[1mb\x1b[0mc");
    let r = t.row(0).unwrap();
    assert!(!t.style(r.get(0).style).has(flags::BOLD));
    assert!(t.style(r.get(1).style).has(flags::BOLD));
    assert!(!t.style(r.get(2).style).has(flags::BOLD));
}

#[test]
fn erase_keeps_background() {
    let t = term(1, 4, "\x1b[44m\x1b[2K");
    let r = t.row(0).unwrap();
    assert_eq!(r.len(), 4);
    assert_eq!(t.style(r.get(3).style).bg, Color::Palette(4));
    let t = term(1, 4, "\x1b[44mab\x1b[0m\x1b[2K");
    assert_eq!(t.row(0).unwrap().len(), 0);
}

#[test]
fn alternate_screen() {
    let mut t = term(2, 4, "main\x1b[2;2H\x1b[?1049hALT");
    assert_eq!(lines(&t), ["", " ALT"]);
    assert!(t.modes().alt_screen);
    t.advance(b"\x1b[?1049l");
    assert_eq!(lines(&t), ["main", ""]);
    assert_eq!(pos(&t), (1, 1));
    t.advance(b"\x1b[?1049h");
    assert_eq!(lines(&t), ["", ""]);
    t.advance(b"\x1b[?1049l\x1b[?47hx\x1b[?47l");
    assert_eq!(lines(&t), ["main", ""]);
    t.advance(b"\x1b[?47h");
    assert_eq!(lines(&t)[1], " x");
    t.advance(b"\x1b[?1047l");
    t.advance(b"\x1b[?1047h");
    assert_eq!(lines(&t), ["", ""]);
    // The alternate screen keeps no scrollback.
    t.advance(b"a\r\nb\r\nc");
    assert_eq!(t.scrollback_len(), 0);
}

#[test]
fn soft_reset() {
    let t = term(
        4,
        5,
        "\x1b[2;3r\x1b[?6h\x1b[4h\x1b[31m\x1b[?25l\x1b(0\x1b[!pab",
    );
    let m = t.modes();
    assert!(!m.origin && !m.insert);
    assert!(t.cursor().visible);
    assert_eq!(t.margins(), (0, 3, 0, 4));
    let r = t.row(1).unwrap();
    assert_eq!(t.row_text(r), "ab");
    assert_eq!(*t.style(r.get(0).style), Default::default());
}

#[test]
fn replies() {
    let mut t = term(5, 10, "\x1b[c\x1b[>c\x1b[5n\x1b[3;4H\x1b[6n");
    let mut out = Vec::new();
    t.take_replies(&mut out);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "\x1b[?62;22c\x1b[>1;10;0c\x1b[0n\x1b[3;4R"
    );
    // Nothing reached the screen or the events.
    assert_eq!(lines(&t), ["", "", "", "", ""]);
    assert!(t.drain_events().next().is_none());
    let mut out = Vec::new();
    t.take_replies(&mut out);
    assert!(out.is_empty());
    t.advance(b"\x1b[2;4r\x1b[?6h\x1b[2;2H\x1b[6n");
    t.take_replies(&mut out);
    assert_eq!(out, b"\x1b[2;2R");
}

#[test]
fn osc_events() {
    let mut t = term(
        1,
        5,
        "\x1b]0;hello;world\x07\x1b]1;icon\x1b\\\x1b]2;t\x07\x1b]99;c;abc\x07\x1b]777;x\x1b\\",
    );
    let ev: Vec<_> = t.drain_events().collect();
    assert_eq!(
        ev,
        vec![
            Event::Title("hello;world".into()),
            Event::IconName("icon".into()),
            Event::Title("t".into()),
            Event::Osc {
                code: "99".into(),
                payload: b"c;abc".to_vec(),
                end: OscEnd {
                    truncated: false,
                    bell: true
                }
            },
            Event::Osc {
                code: "777".into(),
                payload: b"x".to_vec(),
                end: OscEnd {
                    truncated: false,
                    bell: false
                }
            },
        ]
    );
    assert_eq!(lines(&t), [""]);
}

#[test]
fn seqno_changes_only_on_mutation() {
    let mut t = term(3, 5, "ab");
    let s0 = seqnos(&t);
    t.advance(b"\x1b[2;2H\x1b[A\x1b[C");
    assert_eq!(seqnos(&t), s0, "cursor moves");
    t.advance(b"\x1b[2;1H\x1b[2K\x1b[3;1H\x1b[J");
    let s1 = seqnos(&t);
    assert_eq!(s1[0], s0[0], "erasing blank rows");
    assert_eq!(s1[1], s0[1]);
    t.advance(b"\x1b[1;1Hx");
    let s2 = seqnos(&t);
    assert_ne!(s2[0], s1[0]);
    assert_eq!(s2[1..], s1[1..]);
    t.advance(b"\x1b[1;1Hx");
    assert_eq!(seqnos(&t), s2, "rewriting the same cell");
    t.advance(b"\x1b[1;1Hy");
    assert_ne!(seqnos(&t)[0], s2[0]);
    let s3 = seqnos(&t);
    t.advance(b"\x1b[2;1H\x1b[1K");
    assert_eq!(seqnos(&t), s3, "el on a blank row");
    t.advance(b"\x1b[1;1H\x1b[K");
    assert_ne!(seqnos(&t)[0], s3[0], "el on the row with text");
}

/// A line that scrolls up keeps its stable index and its sequence number, so a
/// renderer that shows it can copy it. Only the new bottom row is new.
#[test]
fn scrolling_up_keeps_the_numbers_of_lines_that_stay() {
    for scrollback in [true, false] {
        let mut t = term(4, 5, "");
        if !scrollback {
            // The alternate screen drops the top line instead.
            t.advance(b"\x1b[?1049h");
        }
        t.advance(b"one\r\ntwo\r\nthree\r\nfour");
        let key =
            |t: &cmdterm_vt::Terminal, y: usize| (t.stable_index(y), t.row(y).unwrap().seqno());
        let before: Vec<_> = (0..4).map(|y| key(&t, y)).collect();
        t.advance(b"\r\nfive");
        let after: Vec<_> = (0..4).map(|y| key(&t, y)).collect();
        // Rows 1..4 moved to 0..3 and keep both numbers.
        assert_eq!(after[..3], before[1..], "scrollback {scrollback}");
        assert_ne!(after[3], before[3]);
        assert_eq!(lines(&t), ["two", "three", "four", "five"]);
    }
}

#[test]
fn trailing_blanks_do_not_touch_seqno() {
    let mut t = term(3, 10, "ab");
    let s = seqnos(&t);
    t.advance(b"   ");
    assert_eq!(seqnos(&t), s);
    t.advance(b"\x1b[10G ");
    assert_eq!(seqnos(&t), s);
}

#[test]
fn rows_below_partial_region_renumber_with_new_seqno() {
    let mut t = term(4, 3, "a\r\nb\r\nc\r\nd");
    t.advance(b"\x1b[1;2r\x1b[2;1H");
    let before = seqnos(&t);
    t.advance(b"\n");
    let after = seqnos(&t);
    assert_ne!(after[3], before[3]);
    let scr = t.screen();
    let r = scr.row_by_stable(scr.stable_index(3)).unwrap();
    assert_eq!(r.seqno(), after[3]);
}

#[test]
fn seqno_after_scroll_and_edits() {
    let mut t = term(3, 3, "a\r\nb\r\nc");
    let s = seqnos(&t);
    t.advance(b"\n");
    let s2 = seqnos(&t);
    assert!(s.iter().zip(&s2).all(|(a, b)| a != b));
    let mut t = term(2, 6, "abc");
    let s = seqnos(&t);
    t.advance(b"\x1b[1G\x1b[@");
    assert_ne!(seqnos(&t)[0], s[0]);
    assert_eq!(seqnos(&t)[1], s[1]);
}

#[test]
fn stable_index_survives_scrolling() {
    let mut t = term(2, 3, "a\r\nb");
    assert_eq!(t.stable_index(0), 0);
    assert_eq!(t.stable_index(1), 1);
    t.advance(b"\r\nc\r\nd");
    assert_eq!(t.scrollback_len(), 2);
    assert_eq!(t.row_text(t.row_by_stable(0).unwrap()), "a");
    assert_eq!(t.row_text(t.row_by_stable(1).unwrap()), "b");
    assert_eq!(t.row_text(t.row_by_stable(2).unwrap()), "c");
    assert_eq!(t.row_text(t.row_by_stable(3).unwrap()), "d");
    assert_eq!(t.stable_index(1), 3);
    assert!(t.row_by_stable(4).is_none());
    assert_eq!(t.row_text(t.screen().scrollback_row(0).unwrap()), "a");
}

#[test]
fn scrollback_limit_drops_oldest() {
    let cfg = cmdterm_vt::Config {
        scrollback: 2,
        ..Default::default()
    };
    let mut t = cmdterm_vt::Terminal::new(cmdterm_vt::Size { rows: 2, cols: 3 }, cfg);
    t.advance(b"a\r\nb\r\nc\r\nd\r\ne");
    assert_eq!(t.scrollback_len(), 2);
    assert_eq!(t.screen().first_stable(), 1);
    assert!(t.row_by_stable(0).is_none());
    assert_eq!(t.row_text(t.row_by_stable(1).unwrap()), "b");
    assert_eq!(t.row_text(t.row_by_stable(4).unwrap()), "e");
}

#[test]
fn region_scroll_does_not_feed_scrollback_below_top() {
    let t = term(4, 3, "a\r\nb\r\nc\r\nd\x1b[2;4r\x1b[4;1H\n\n");
    assert_eq!(t.scrollback_len(), 0);
    let t = term(4, 3, "a\r\nb\r\nc\r\nd\x1b[1;3r\x1b[3;1H\n");
    assert_eq!(t.scrollback_len(), 1);
}

#[test]
fn rows_are_trimmed() {
    let t = term(2, 10, "abc   \x1b[1G\x1b[3C\x1b[K");
    assert_eq!(t.row(0).unwrap().len(), 3);
    let t = term(1, 10, "abc\x1b[1;1H\x1b[K");
    assert_eq!(t.row(0).unwrap().len(), 0);
    let t = term(1, 10, "abc\x1b[2G\x1b[8X");
    assert_eq!(t.row(0).unwrap().len(), 1);
    let t = term(1, 10, "   ");
    assert_eq!(t.row(0).unwrap().len(), 0);
    let t = term(1, 10, "ab  ");
    assert_eq!(
        t.row(0).unwrap().len(),
        2,
        "spaces written over blanks are not stored"
    );
}

#[test]
fn palette_resolves() {
    use cmdterm_vt::Rgb;
    let t = term(1, 1, "");
    let p = t.palette();
    assert_eq!(p.get(1), Rgb::new(0xcd, 0, 0));
    assert_eq!(p.get(16), Rgb::new(0, 0, 0));
    assert_eq!(p.get(21), Rgb::new(0, 0, 255));
    assert_eq!(p.get(196), Rgb::new(255, 0, 0));
    assert_eq!(p.get(231), Rgb::new(255, 255, 255));
    assert_eq!(p.get(232), Rgb::new(8, 8, 8));
    assert_eq!(p.get(255), Rgb::new(238, 238, 238));
    assert_eq!(p.resolve(Color::Default, true), p.fg);
    assert_eq!(p.resolve(Color::Default, false), p.bg);
    assert_eq!(p.resolve(Color::Palette(9), true), Rgb::new(255, 0, 0));
    assert_eq!(p.resolve(Color::Rgb(1, 2, 3), false), Rgb::new(1, 2, 3));
}

#[test]
fn split_input_is_the_same() {
    let input = "\x1b[1;31mhe\x1b[0mllo\x1b[2;3H日本\r\n\x1b(0lq\x1b]0;t\x07";
    let whole = term(4, 8, input);
    let mut parts =
        cmdterm_vt::Terminal::new(cmdterm_vt::Size { rows: 4, cols: 8 }, Default::default());
    for b in input.as_bytes() {
        parts.advance(std::slice::from_ref(b));
    }
    assert_eq!(lines(&whole), lines(&parts));
    assert_eq!(pos(&whole), pos(&parts));
}

#[test]
fn resize_clamps() {
    let mut t = term(4, 6, "abcdef\r\nx\r\n\r\ny");
    t.resize(cmdterm_vt::Size { rows: 2, cols: 3 });
    assert_eq!(lines(&t), ["", "y"]);
    assert_eq!(pos(&t), (1, 1));
    // "abcdef" rewrapped into two rows, then "x", all in the scrollback.
    assert_eq!(t.scrollback_len(), 3);
    t.advance(b"zzzzzz");
    assert!(t.cursor().col < 3);
}

#[test]
fn resize_on_alt_screen_keeps_saved_cursor() {
    let mut t = term(5, 10, "$ vim\r\n\x1b[1m\x1b[?1049hVIM");
    t.resize(cmdterm_vt::Size { rows: 6, cols: 12 });
    t.advance(b"\x1b[?1049l");
    assert_eq!(pos(&t), (1, 0));
    t.advance(b"x");
    assert_eq!(lines(&t)[0], "$ vim");
    assert_eq!(lines(&t)[1], "x");
}

#[test]
fn ris_keeps_counters_moving_forward() {
    let mut t = term(3, 10, "A");
    let (seq, stable) = (seqnos(&t)[0], t.stable_index(0));
    t.advance(b"\x1bcB");
    assert_ne!(seqnos(&t)[0], seq, "row 0 seqno after RIS");
    assert!(t.stable_index(0) >= stable);
    assert_eq!(lines(&t)[0], "B");
}

#[test]
fn delete_line_at_the_top_keeps_scrollback_indexes() {
    let mut t = term(3, 10, "a\r\nb\r\nc\r\nd\r\ne");
    assert_eq!(t.scrollback_len(), 2);
    let kept: Vec<String> = (0..2)
        .map(|i| t.row_text(t.row_by_stable(i).unwrap()))
        .collect();
    t.advance(b"\x1b[H\x1b[M");
    for (i, text) in kept.iter().enumerate() {
        assert_eq!(&t.row_text(t.row_by_stable(i as u64).unwrap()), text);
    }
    assert_eq!(lines(&t)[0], "d");
    assert_eq!(t.row_text(t.row_by_stable(t.stable_index(0)).unwrap()), "d");
}

#[test]
fn stable_index_without_scrollback() {
    let cfg = cmdterm_vt::Config {
        scrollback: 0,
        ..Default::default()
    };
    let mut t = cmdterm_vt::Terminal::new(cmdterm_vt::Size { rows: 3, cols: 5 }, cfg);
    t.advance(b"1\r\n2\r\n3");
    let s = t.stable_index(2);
    t.advance(b"\r\n4");
    assert_eq!(t.row_text(t.row_by_stable(s).unwrap()), "3");
    assert_eq!(t.stable_index(1), s);
    let mut t = term(3, 5, "1\r\n2\r\n3\x1b[?1049h\x1b[3;1HX");
    let s = t.stable_index(2);
    t.advance(b"\r\nY");
    assert_eq!(t.stable_index(1), s);
}

#[test]
fn su_inside_margins_on_blank_screen_touches_nothing() {
    let mut t = term(4, 10, "");
    let s = seqnos(&t);
    t.advance(b"\x1b[?69h\x1b[3;6s\x1b[1S\x1b[1T");
    assert_eq!(seqnos(&t), s);
}

#[test]
fn su_inside_margins_touches_only_changed_rows() {
    let mut t = term(4, 10, "\x1b[3;3Hxy");
    let s = seqnos(&t);
    t.advance(b"\x1b[?69h\x1b[3;6s\x1b[1;4r\x1b[1S");
    let n = seqnos(&t);
    assert_eq!(n[0], s[0]);
    assert_ne!(n[1], s[1]);
    assert_ne!(n[2], s[2]);
    assert_eq!(n[3], s[3]);
}

#[test]
fn dch_and_ich_that_change_nothing_keep_seqno() {
    let mut t = term(2, 10, "ab");
    let s = seqnos(&t);
    t.advance(b"\x1b[8G\x1b[3P\x1b[8G\x1b[3@");
    assert_eq!(seqnos(&t), s);
    // x is inside the stored row, but the margin-limited span holds blanks
    // that stay blanks, so the row is not touched.
    let mut t = term(1, 10, "\x1b[9Gz");
    let s = seqnos(&t);
    t.advance(b"\x1b[?69h\x1b[1;5s\x1b[2G\x1b[2@\x1b[2G\x1b[2P");
    assert_eq!(seqnos(&t), s);
}

#[test]
fn decrc_restores_shift_state() {
    let mut t = term(2, 6, "\x1b)0\x0e\x1b7\x0fx\x1b8");
    assert_eq!(t.gl(), 1, "shift out saved by DECSC comes back");
    t.advance(b"\x1bc\x1b8");
    assert_eq!(t.gl(), 0, "restore with nothing saved resets the shift");
}

#[test]
fn alt_screen_1049_restores_cursor_and_pen() {
    let mut t = term(
        3,
        6,
        "\x1b[3;4H\x1b[31m\x1b[?1049h\x1b[1;1H\x1b[44mx\x1b[?1049l",
    );
    assert_eq!(pos(&t), (2, 3), "cursor back where 1049 h saved it");
    assert_eq!(t.pen().fg, Color::Palette(1));
    assert_eq!(t.pen().bg, Color::Default);
    t.advance(b"\x1b[?47h\x1b[2;2H\x1b[?47l");
    assert_eq!(pos(&t), (1, 1), "47 leaves the cursor alone");
}

#[test]
fn lnm_makes_line_feed_return_to_column_zero() {
    run(&[
        Case {
            name: "lnm on",
            rows: 3,
            cols: 10,
            input: "\x1b[20hab\ncd",
            text: &["ab", "cd", ""],
            cursor: (1, 2),
        },
        Case {
            name: "lnm off",
            rows: 3,
            cols: 10,
            input: "\x1b[20h\x1b[20lab\ncd",
            text: &["ab", "  cd", ""],
            cursor: (1, 4),
        },
    ]);
}
