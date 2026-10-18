//! Replies to device queries: attributes, status, size reports, DECRQSS, XTGETTCAP.
mod common;

use cmdterm_vt::{Config, Event, Size, Terminal};
use common::term;

fn reply(t: &mut Terminal) -> String {
    let mut out = Vec::new();
    t.take_replies(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn ask(input: &str) -> String {
    let mut t = term(5, 10, input);
    reply(&mut t)
}

fn pixel_term(input: &str) -> Terminal {
    let config = Config {
        cell_pixels: (9, 18),
        ..Config::default()
    };
    let mut t = Terminal::new(Size { rows: 5, cols: 10 }, config);
    t.advance(input.as_bytes());
    t
}

#[test]
fn device_attributes() {
    assert_eq!(ask("\x1b[c"), "\x1b[?62;22c");
    assert_eq!(ask("\x1b[0c"), "\x1b[?62;22c");
    assert_eq!(ask("\x1b[>c"), "\x1b[>1;10;0c");
    assert_eq!(ask("\x1b[>0c"), "\x1b[>1;10;0c");
    assert_eq!(ask("\x1b[=c"), "\x1bP!|00000000\x1b\\");
    // Other parameters are not a request.
    assert_eq!(ask("\x1b[1c\x1b[>1c\x1b[=1c"), "");
}

#[test]
fn status_reports() {
    assert_eq!(ask("\x1b[5n"), "\x1b[0n");
    assert_eq!(ask("\x1b[3;4H\x1b[6n"), "\x1b[3;4R");
    assert_eq!(ask("\x1b[3;4H\x1b[?6n"), "\x1b[?3;4;1R");
    // Origin mode reports relative to the margins.
    assert_eq!(ask("\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[6n"), "\x1b[2;3R");
    assert_eq!(ask("\x1b[2;4r\x1b[?6h\x1b[2;3H\x1b[?6n"), "\x1b[?2;3;1R");
    assert_eq!(ask("\x1b[99n"), "");
}

#[test]
fn xtversion_names_the_terminal() {
    let r = ask("\x1b[>q");
    assert!(
        r.starts_with("\x1bP>|cmdterm-vt(") && r.ends_with(")\x1b\\"),
        "{r:?}"
    );
    assert_eq!(ask("\x1b[>0q"), r);
    let config = Config {
        xtversion: "scriptschnell-term(1.2.3)".into(),
        ..Config::default()
    };
    let mut t = Terminal::new(Size { rows: 2, cols: 2 }, config);
    t.advance(b"\x1b[>q");
    assert_eq!(reply(&mut t), "\x1bP>|scriptschnell-term(1.2.3)\x1b\\");
}

#[test]
fn window_size_reports() {
    let mut t = pixel_term("\x1b[14t\x1b[16t\x1b[18t\x1b[19t");
    assert_eq!(
        reply(&mut t),
        "\x1b[4;90;90t\x1b[6;18;9t\x1b[8;5;10t\x1b[9;5;10t"
    );
    // The reports follow resizes and cell size changes.
    t.resize(Size { rows: 3, cols: 7 });
    t.set_cell_pixels(10, 20);
    t.advance(b"\x1b[14t\x1b[14;2t\x1b[16t\x1b[18t");
    assert_eq!(
        reply(&mut t),
        "\x1b[4;60;70t\x1b[4;60;70t\x1b[6;20;10t\x1b[8;3;7t"
    );
}

#[test]
fn pixel_reports_need_a_known_cell_size() {
    // Without a cell size, only the character reports answer.
    assert_eq!(ask("\x1b[14t\x1b[16t"), "");
    assert_eq!(ask("\x1b[18t"), "\x1b[8;5;10t");
}

#[test]
fn other_window_operations_are_ignored() {
    let mut t = pixel_term(
        "\x1b[1t\x1b[2t\x1b[3;10;10t\x1b[4;100;100t\x1b[8;50;50t\x1b[15t\x1b[20t\x1b[21t\x1b[t",
    );
    assert_eq!(reply(&mut t), "");
    assert_eq!(t.size(), Size { rows: 5, cols: 10 });
    assert_eq!(t.title(), "");
    assert!(t.drain_events().next().is_none());
}

#[test]
fn decrqss_answers_the_settings() {
    assert_eq!(ask("\x1bP$qm\x1b\\"), "\x1bP1$r0m\x1b\\");
    assert_eq!(
        ask("\x1b[1;3;4;7;9;31;48;2;1;2;3m\x1bP$qm\x1b\\"),
        "\x1bP1$r0;1;3;4;7;9;31;48:2::1:2:3m\x1b\\"
    );
    assert_eq!(
        ask("\x1b[2;53;38;5;200;100;58;5;9m\x1bP$qm\x1b\\"),
        "\x1bP1$r0;2;38:5:200;100;58:5:9m\x1b\\"
    );
    assert_eq!(ask("\x1bP$qr\x1b\\"), "\x1bP1$r1;5r\x1b\\");
    assert_eq!(ask("\x1b[2;4r\x1bP$qr\x1b\\"), "\x1bP1$r2;4r\x1b\\");
    assert_eq!(
        ask("\x1b[?69h\x1b[3;6s\x1bP$qs\x1b\\"),
        "\x1bP1$r3;6s\x1b\\"
    );
    assert_eq!(ask("\x1bP$qs\x1b\\"), "\x1bP1$r1;10s\x1b\\");
}

#[test]
fn xtgettcap_overflow_echoes_the_first_name() {
    let long = "41".repeat(1000);
    assert_eq!(
        ask(&format!("\x1bP+q{long}\x1b\\")),
        format!("\x1bP0+r{}\x1b\\", &long[..256])
    );
    assert_eq!(ask("\x1bP+q\x1b\\"), "\x1bP0+r\x1b\\");
}

#[test]
fn decrqss_cursor_style_matches_decscusr() {
    // A fresh terminal has a steady block.
    assert_eq!(ask("\x1bP$q q\x1b\\"), "\x1bP1$r2 q\x1b\\");
    for n in 1..=6 {
        assert_eq!(
            ask(&format!("\x1b[{n} q\x1bP$q q\x1b\\")),
            format!("\x1bP1$r{n} q\x1b\\")
        );
    }
    assert_eq!(ask("\x1b[0 q\x1bP$q q\x1b\\"), "\x1bP1$r1 q\x1b\\");
}

#[test]
fn decrqss_refuses_what_it_does_not_know() {
    assert_eq!(ask("\x1bP$qxyz\x1b\\"), "\x1bP0$r\x1b\\");
    assert_eq!(ask("\x1bP$q\x1b\\"), "\x1bP0$r\x1b\\");
    // Too long a request is refused, not truncated into a different one.
    let long = "m".repeat(1000);
    assert_eq!(ask(&format!("\x1bP$q{long}\x1b\\")), "\x1bP0$r\x1b\\");
}

#[test]
fn dcs_requests_survive_split_input() {
    let mut t = term(5, 10, "");
    for chunk in ["\x1b", "P", "$", "q", "r", "\x1b", "\\"] {
        t.advance(chunk.as_bytes());
    }
    assert_eq!(reply(&mut t), "\x1bP1$r1;5r\x1b\\");
    // Other DCS strings are swallowed without a reply.
    t.advance(b"\x1bP1;2|data\x1b\\\x1bP$q");
    assert_eq!(reply(&mut t), "");
}

#[test]
fn xtgettcap_answers_a_small_set() {
    // TN, Co and RGB, hex encoded.
    assert_eq!(
        ask("\x1bP+q544e\x1b\\"),
        "\x1bP1+r544e=787465726D2D323536636F6C6F72\x1b\\"
    );
    assert_eq!(ask("\x1bP+q436f\x1b\\"), "\x1bP1+r436f=323536\x1b\\");
    assert_eq!(
        ask("\x1bP+q524742\x1b\\"),
        "\x1bP1+r524742=382F382F38\x1b\\"
    );
    // Several names in one request answer one by one; unknown and malformed ones are
    // not found.
    assert_eq!(
        ask("\x1bP+q436f;6e6f7065;zz\x1b\\"),
        "\x1bP1+r436f=323536\x1b\\\x1bP0+r6e6f7065\x1b\\\x1bP0+rzz\x1b\\"
    );
}

#[test]
fn enq_has_an_empty_answerback() {
    let mut t = term(2, 10, "a\x05b");
    assert_eq!(reply(&mut t), "");
    assert_eq!(t.screen_text()[0], "ab");
    assert!(t.drain_events().next().is_none());
}

#[test]
fn bell_stays_an_event() {
    let mut t = term(2, 10, "\x07");
    assert_eq!(t.drain_events().collect::<Vec<_>>(), [Event::Bell]);
    assert_eq!(reply(&mut t), "");
}
