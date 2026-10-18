//! OSC strings: titles, colors, directory, hyperlinks, clipboard, notifications, user
//! variables, prompt marks and the passthrough of the rest.
mod common;

use cmdterm_vt::osc::{base64_decode, base64_encode};
use cmdterm_vt::{
    Cell, Config, CurrentDir, Event, OscEnd, Progress, Rgb, SemanticKind, Size, Terminal,
};
use common::term;

fn events(t: &mut Terminal) -> Vec<Event> {
    t.drain_events().collect()
}

fn replies(t: &mut Terminal) -> String {
    let mut out = Vec::new();
    t.take_replies(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

const BEL: OscEnd = OscEnd {
    truncated: false,
    bell: true,
};
const ST: OscEnd = OscEnd {
    truncated: false,
    bell: false,
};

// Titles --------------------------------------------------------------------------------

#[test]
fn title_stack_pushes_and_pops() {
    let mut t = term(
        2,
        10,
        "\x1b]2;one\x07\x1b[22;2t\x1b]2;two\x07\x1b[22;0t\x1b]2;three\x07",
    );
    assert_eq!(t.title(), "three");
    t.advance(b"\x1b[23;2t");
    assert_eq!(t.title(), "two");
    t.advance(b"\x1b[23t");
    assert_eq!(t.title(), "one");
    // The stack is empty now; popping again changes nothing.
    t.advance(b"\x1b[23;2t");
    assert_eq!(t.title(), "one");
    let titles: Vec<_> = events(&mut t)
        .into_iter()
        .filter_map(|e| match e {
            Event::Title(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["one", "two", "three", "two", "one"]);
}

#[test]
fn title_stack_keeps_icon_and_title_apart() {
    let mut t = term(
        2,
        10,
        "\x1b]0;both\x07\x1b[22;1t\x1b]1;other\x07\x1b]2;t2\x07",
    );
    assert_eq!((t.title(), t.icon_name()), ("t2", "other"));
    events(&mut t);
    t.advance(b"\x1b[23;1t");
    assert_eq!((t.title(), t.icon_name()), ("t2", "both"));
    assert_eq!(events(&mut t), [Event::IconName("both".into())]);
}

#[test]
fn title_stack_is_bounded() {
    let mut t = term(2, 10, "");
    for i in 0..30 {
        t.advance(format!("\x1b]2;t{i}\x07\x1b[22;2t").as_bytes());
    }
    let mut pops = 0;
    for _ in 0..30 {
        t.advance(b"\x1b[23;2t");
        pops += 1;
    }
    assert_eq!(pops, 30);
    // Ten entries survived; the oldest popped one is t20.
    assert_eq!(t.title(), "t20");
}

// Palette and default colors --------------------------------------------------------------

#[test]
fn osc4_sets_and_resets_palette_entries() {
    let mut t = term(2, 10, "\x1b]4;1;rgb:ff/00/80;2;#0a0b0c\x07");
    assert_eq!(t.palette().get(1), Rgb::new(255, 0, 128));
    assert_eq!(t.palette().get(2), Rgb::new(0x0a, 0x0b, 0x0c));
    assert_eq!(events(&mut t), [Event::PaletteChanged]);
    t.advance(b"\x1b]104;1\x07");
    assert_eq!(t.palette().get(1), Rgb::new(0xcd, 0, 0));
    assert_eq!(t.palette().get(2), Rgb::new(0x0a, 0x0b, 0x0c));
    t.advance(b"\x1b]104\x07");
    assert_eq!(t.palette().get(2), Rgb::new(0, 0xcd, 0));
    // Bad indexes and specs change nothing.
    let mut t = term(2, 10, "\x1b]4;256;#fff;3;nonsense;x;#fff\x07");
    assert_eq!(t.palette().get(3), Rgb::new(0xcd, 0xcd, 0));
    assert!(events(&mut t).is_empty());
}

#[test]
fn color_specs_scale_like_xterm() {
    for (spec, want) in [
        ("rgb:f/0/8", Rgb::new(255, 0, 136)),
        ("rgb:12/34/56", Rgb::new(0x12, 0x34, 0x56)),
        ("rgb:123/456/789", Rgb::new(0x12, 0x45, 0x78)),
        ("rgb:1234/5678/9abc", Rgb::new(0x12, 0x56, 0x9a)),
        ("#123", Rgb::new(0x11, 0x22, 0x33)),
        ("#123456", Rgb::new(0x12, 0x34, 0x56)),
        ("#123456789", Rgb::new(0x12, 0x45, 0x78)),
        ("#123456789abc", Rgb::new(0x12, 0x56, 0x9a)),
    ] {
        let t = term(2, 10, &format!("\x1b]4;9;{spec}\x07"));
        assert_eq!(t.palette().get(9), want, "{spec}");
    }
}

#[test]
fn osc4_query_echoes_the_terminator() {
    let mut t = term(2, 10, "\x1b]4;1;?\x07\x1b]4;1;?;2;?\x1b\\");
    assert_eq!(
        replies(&mut t),
        "\x1b]4;1;rgb:cdcd/0000/0000\x07\
         \x1b]4;1;rgb:cdcd/0000/0000\x1b\\\x1b]4;2;rgb:0000/cdcd/0000\x1b\\"
    );
    assert!(events(&mut t).is_empty());
}

#[test]
fn osc10_to_12_set_query_and_reset() {
    let mut t = term(2, 10, "\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07");
    assert_eq!(
        replies(&mut t),
        "\x1b]10;rgb:e5e5/e5e5/e5e5\x07\x1b]11;rgb:0000/0000/0000\x1b\\\x1b]12;rgb:e5e5/e5e5/e5e5\x07"
    );
    t.advance(b"\x1b]10;#102030\x07\x1b]11;rgb:40/50/60\x07\x1b]12;#708090\x07");
    let p = t.palette();
    assert_eq!(p.fg, Rgb::new(0x10, 0x20, 0x30));
    assert_eq!(p.bg, Rgb::new(0x40, 0x50, 0x60));
    assert_eq!(p.cursor, Rgb::new(0x70, 0x80, 0x90));
    assert_eq!(events(&mut t), vec![Event::PaletteChanged; 3]);
    // A second spec sets the next color in the row.
    t.advance(b"\x1b]10;#010101;#020202;#030303\x07");
    let p = t.palette();
    assert_eq!(p.fg, Rgb::new(1, 1, 1));
    assert_eq!(p.bg, Rgb::new(2, 2, 2));
    assert_eq!(p.cursor, Rgb::new(3, 3, 3));
    t.advance(b"\x1b]110\x07\x1b]111\x07\x1b]112\x07");
    let p = t.palette();
    assert_eq!(p.fg, Rgb::new(0xe5, 0xe5, 0xe5));
    assert_eq!(p.bg, Rgb::new(0, 0, 0));
    assert_eq!(p.cursor, Rgb::new(0xe5, 0xe5, 0xe5));
    // Chained query answers each.
    replies(&mut t);
    t.advance(b"\x1b]11;?;?\x07");
    assert_eq!(
        replies(&mut t),
        "\x1b]11;rgb:0000/0000/0000\x07\x1b]12;rgb:e5e5/e5e5/e5e5\x07"
    );
}

// Current directory -----------------------------------------------------------------------

#[test]
fn osc7_reports_the_directory() {
    let mut t = term(2, 10, "");
    assert_eq!(t.current_dir(), None);
    t.advance(b"\x1b]7;file://myhost/tmp/a%20b/%C3%A9\x07");
    let want = CurrentDir {
        host: "myhost".into(),
        path: "/tmp/a b/\u{e9}".into(),
    };
    assert_eq!(t.current_dir(), Some(&want));
    assert_eq!(events(&mut t), [Event::CurrentDir(want)]);
    t.advance(b"\x1b]7;file:///home/me\x1b\\");
    assert_eq!(t.current_dir().unwrap().path, "/home/me");
    assert_eq!(t.current_dir().unwrap().host, "");
    // Other schemes and malformed URLs are ignored, and are not passed through.
    events(&mut t);
    t.advance(b"\x1b]7;http://x/y\x07\x1b]7;file://nohost\x07\x1b]7\x07");
    assert_eq!(t.current_dir().unwrap().path, "/home/me");
    assert!(events(&mut t).is_empty());
}

// Hyperlinks ------------------------------------------------------------------------------

#[test]
fn osc8_attaches_links_to_cells() {
    let t = term(
        2,
        20,
        "a\x1b]8;id=x;https://e.org/1\x1b\\bc\x1b]8;;\x1b\\d\x1b]8;;https://e.org/2\x07e\x1b]8;;\x07",
    );
    let row = t.row(0).unwrap();
    let link = |x| t.hyperlink(&row.get(x));
    assert_eq!(link(0), None);
    assert_eq!(link(1), Some("https://e.org/1"));
    assert_eq!(link(2), Some("https://e.org/1"));
    assert_eq!(link(3), None);
    assert_eq!(link(4), Some("https://e.org/2"));
    assert_eq!(t.hyperlink_id(&row.get(1)), Some("x"));
    assert_eq!(t.hyperlink_id(&row.get(4)), None);
    // The text is untouched and the escape produced no event.
    assert_eq!(t.screen_text()[0], "abcde");
}

#[test]
fn hyperlink_survives_sgr_and_cursor_restore() {
    let t = term(2, 20, "\x1b7\x1b]8;;http://u\x07a\x1b[1;31mb\x1b[0mc\x1b8d");
    let row = t.row(0).unwrap();
    let got: Vec<_> = (0..3).map(|x| t.hyperlink(&row.get(x))).collect();
    // The restore moved the cursor back to column 0 and wrote d over a, still linked.
    assert_eq!(got, [Some("http://u"); 3]);
    assert_eq!(row.get(0).ch(), 'd');
}

#[test]
fn hyperlink_ids_intern_and_cells_stay_small() {
    let t = term(
        1,
        20,
        "\x1b]8;id=k;http://u\x07a\x1b]8;;\x07b\x1b]8;id=k;http://u\x07c\x1b]8;id=j;http://u\x07d",
    );
    let row = t.row(0).unwrap();
    // Same id and URI share one table entry, hence one style id.
    assert_eq!(row.get(0).style, row.get(2).style);
    assert_ne!(row.get(0).style, row.get(3).style);
    assert_eq!(std::mem::size_of::<Cell>(), 12);
}

#[test]
fn osc8_rejects_bad_input() {
    let t = term(1, 20, "\x1b]8;nosemicolon\x07a\x1b]8;;\x07b");
    let row = t.row(0).unwrap();
    assert_eq!(t.hyperlink(&row.get(0)), None);
    assert_eq!(t.hyperlink(&row.get(1)), None);
}

// Clipboard -------------------------------------------------------------------------------

#[test]
fn osc52_set_decodes_and_keeps_the_selection() {
    let mut t = term(
        1,
        10,
        "\x1b]52;c;aGVsbG8=\x07\x1b]52;pq;aGk\x1b\\\x1b]52;;eA==\x07\x1b]52;c;!!!\x07\x1b]52;z;eA==\x07",
    );
    assert_eq!(
        events(&mut t),
        [
            Event::Clipboard {
                selection: "c".into(),
                data: b"hello".to_vec()
            },
            Event::Clipboard {
                selection: "pq".into(),
                data: b"hi".to_vec()
            },
            Event::Clipboard {
                selection: "s0".into(),
                data: b"x".to_vec()
            },
        ]
    );
}

#[test]
fn osc52_query_is_refused_unless_enabled() {
    let mut t = term(1, 10, "\x1b]52;c;?\x07");
    assert!(events(&mut t).is_empty());
    assert_eq!(replies(&mut t), "");

    let config = Config {
        allow_clipboard_query: true,
        ..Config::default()
    };
    let mut t = Terminal::new(Size { rows: 1, cols: 10 }, config);
    t.advance(b"\x1b]52;c;?\x1b\\");
    assert_eq!(
        events(&mut t),
        [Event::ClipboardQuery {
            selection: "c".into(),
            bell: false
        }]
    );
    // The terminal does not know the clipboard; the owner answers.
    assert_eq!(replies(&mut t), "");
    t.answer_clipboard("c", b"hi", false);
    assert_eq!(replies(&mut t), "\x1b]52;c;aGk=\x1b\\");
    t.answer_clipboard("c", b"hi", true);
    assert_eq!(replies(&mut t), "\x1b]52;c;aGk=\x07");
}

#[test]
fn base64_round_trips() {
    for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
        let enc = base64_encode(s.as_bytes());
        assert_eq!(base64_decode(enc.as_bytes()).unwrap(), s.as_bytes());
    }
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_decode(b"Zm9vYg").unwrap(), b"foob");
    assert!(base64_decode(b"Zm9vY").is_none());
    assert!(base64_decode(b"Zm9v=Yg").is_none());
    assert!(base64_decode(b"Zm9*").is_none());
}

// Notifications, progress and user variables -----------------------------------------------

#[test]
fn osc9_and_osc777_notify() {
    let mut t = term(
        1,
        10,
        "\x1b]9;build done\x07\x1b]777;notify;Title;Body;more\x1b\\\x1b]777;notify;Only\x07",
    );
    assert_eq!(
        events(&mut t),
        [
            Event::Toast {
                title: None,
                body: "build done".into()
            },
            Event::Toast {
                title: Some("Title".into()),
                body: "Body;more".into()
            },
            Event::Toast {
                title: Some("Only".into()),
                body: String::new()
            },
        ]
    );
}

#[test]
fn osc9_4_reports_progress() {
    let mut t = term(
        1,
        10,
        "\x1b]9;4;0\x07\x1b]9;4;1;50\x07\x1b]9;4;2;30\x07\x1b]9;4;3\x07\x1b]9;4;4;10\x07\
         \x1b]9;4;1;200\x07\x1b]9;4;1\x07\x1b]9;4;9;1\x07",
    );
    assert_eq!(
        events(&mut t),
        [
            Event::Progress(Progress::None),
            Event::Progress(Progress::Percentage(50)),
            Event::Progress(Progress::Error(30)),
            Event::Progress(Progress::Indeterminate),
            Event::Progress(Progress::Paused(10)),
            Event::Progress(Progress::Percentage(100)),
            Event::Progress(Progress::Percentage(0)),
        ]
    );
}

#[test]
fn osc1337_set_user_var_decodes_the_value() {
    let value = base64_encode(b"busy:working on it");
    let mut t = term(
        1,
        10,
        &format!("\x1b]1337;SetUserVar=cmdterm_agent={value}\x07\x1b]1337;SetUserVar=bad=%%%\x07"),
    );
    assert_eq!(
        events(&mut t),
        [Event::UserVar {
            name: "cmdterm_agent".into(),
            value: "busy:working on it".into()
        }]
    );
    // Other 1337 commands pass through.
    t.advance(b"\x1b]1337;File=inline=1:xx\x07");
    assert_eq!(
        events(&mut t),
        [Event::Osc {
            code: "1337".into(),
            payload: b"File=inline=1:xx".to_vec(),
            end: BEL
        }]
    );
}

#[test]
fn osc1337_current_dir() {
    let t = term(1, 10, "\x1b]1337;CurrentDir=/srv/x\x07");
    assert_eq!(t.current_dir().unwrap().path, "/srv/x");
}

// Semantic prompts ------------------------------------------------------------------------

/// One shell cycle: prompt, typed command, output of `lines` lines, exit status.
fn cycle(aid: u32, lines: usize, status: i32) -> String {
    let mut s = format!("\x1b]133;A;aid={aid}\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07");
    for i in 0..lines {
        s.push_str(&format!("out{i}\r\n"));
    }
    s.push_str(&format!("\x1b]133;D;{status}\x07"));
    s
}

type Shape = (SemanticKind, (u64, usize), (u64, usize));

fn zone_shape(t: &Terminal) -> Vec<Shape> {
    t.semantic_zones()
        .iter()
        .map(|z| (z.kind, (z.start_row, z.start_col), (z.end_row, z.end_col)))
        .collect()
}

#[test]
fn semantic_zones_split_prompt_input_and_output() {
    use SemanticKind::*;
    let mut t = term(4, 20, "");
    assert!(t.semantic_zones().is_empty());
    t.advance(cycle(1, 1, 0).as_bytes());
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(
        zone_shape(&t),
        [
            (Prompt, (0, 0), (0, 1)),
            (Input, (0, 2), (0, 19)),
            (Output, (1, 0), (1, 19)),
            (Prompt, (2, 0), (2, 1)),
            (Input, (2, 2), (3, 19)),
        ]
    );
    let zones = t.semantic_zones();
    assert_eq!(zones[0].aid.as_deref(), Some("1"));
    assert_eq!(zones[2].exit_status, Some(0));
    assert_eq!(zones[4].exit_status, None);
}

#[test]
fn semantic_zones_keep_stable_rows_through_scrollback() {
    use SemanticKind::*;
    let mut t = term(4, 20, "");
    t.advance(cycle(1, 1, 0).as_bytes());
    t.advance(cycle(2, 2, 1).as_bytes());
    t.advance(b"\x1b]133;A;aid=3\x07$ \x1b]133;B\x07");
    assert!(t.scrollback_len() > 0);
    let zones = t.semantic_zones();
    // Command 1 started on the first line ever written and still does.
    assert_eq!(zones[0].kind, Prompt);
    assert_eq!((zones[0].start_row, zones[0].start_col), (0, 0));
    let text = |row: u64| t.row_text(t.row_by_stable(row).unwrap());
    assert_eq!(text(0), "$ ls");
    assert_eq!(text(1), "out0");
    assert_eq!(text(zones[3].start_row), "$ ls");
    assert_eq!(zones[3].aid.as_deref(), Some("2"));
    assert_eq!(zones[5].exit_status, Some(1));
    let last = zones.last().unwrap();
    assert_eq!(last.kind, Input);
    // Input shares the aid of its prompt.
    assert_eq!(last.aid.as_deref(), Some("3"));
    assert_eq!(last.end_row, t.stable_index(3));
    // Zones are ordered and do not overlap.
    for w in zones.windows(2) {
        assert!((w[0].end_row, w[0].end_col) < (w[1].start_row, w[1].start_col));
    }
    // The command runs: the last zone becomes output.
    t.advance(b"x\r\n\x1b]133;C\x07");
    assert_eq!(t.semantic_zones().last().unwrap().kind, Output);
}

#[test]
fn semantic_zones_clamp_to_the_kept_history() {
    let config = Config {
        scrollback: 3,
        ..Config::default()
    };
    let mut t = Terminal::new(Size { rows: 4, cols: 20 }, config);
    for i in 0..20 {
        t.advance(cycle(i, 2, 0).as_bytes());
    }
    t.advance(b"\x1b]133;A\x07$ ");
    let first = t.screen().first_stable();
    assert!(first > 0);
    let zones = t.semantic_zones();
    assert_eq!(zones[0].start_row, first);
    assert!(zones.iter().all(|z| z.start_row >= first));
    assert!(zones.len() < 20 * 3);
    assert_eq!(zones.last().unwrap().kind, SemanticKind::Prompt);
}

#[test]
fn a_redrawn_prompt_replaces_the_old_marks() {
    let mut t = term(4, 20, "");
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07abc");
    // The shell clears the line and draws the prompt again.
    t.advance(b"\r\x1b[K\x1b]133;A\x07> \x1b]133;B\x07");
    let zones = t.semantic_zones();
    let kinds: Vec<_> = zones.iter().map(|z| z.kind).collect();
    assert_eq!(kinds, [SemanticKind::Prompt, SemanticKind::Input]);
    assert_eq!((zones[1].start_row, zones[1].start_col), (0, 2));
}

#[test]
fn marks_on_the_alternate_screen_are_ignored() {
    let mut t = term(
        4,
        20,
        "\x1b]133;A\x07$ \x1b[?1049h\x1b]133;C\x07\x1b[?1049l",
    );
    assert_eq!(t.semantic_zones().len(), 1);
    assert_eq!(t.semantic_zones()[0].kind, SemanticKind::Prompt);
    // And they carry no event.
    assert!(events(&mut t).is_empty());
}

// Passthrough -----------------------------------------------------------------------------

#[test]
fn unknown_osc_keeps_payload_and_terminator() {
    let mut t = term(
        1,
        10,
        "\x1b]7501;probe;1;ab\x07\x1b]7501;report;busy\x1b\\\x1b]7501\x07\x1b]777;other;x\x07",
    );
    assert_eq!(
        events(&mut t),
        [
            Event::Osc {
                code: "7501".into(),
                payload: b"probe;1;ab".to_vec(),
                end: BEL
            },
            Event::Osc {
                code: "7501".into(),
                payload: b"report;busy".to_vec(),
                end: ST
            },
            Event::Osc {
                code: "7501".into(),
                payload: Vec::new(),
                end: BEL
            },
            Event::Osc {
                code: "777".into(),
                payload: b"other;x".to_vec(),
                end: BEL
            },
        ]
    );
}

#[test]
fn truncated_osc_is_marked() {
    let mut t = term(1, 10, "");
    let mut input = b"\x1b]7501;".to_vec();
    input.resize(input.len() + (1 << 20) + 100, b'a');
    input.push(0x07);
    t.advance(&input);
    let ev = events(&mut t);
    assert_eq!(ev.len(), 1);
    match &ev[0] {
        Event::Osc { end, payload, .. } => {
            assert!(end.truncated);
            assert!(payload.len() < (1 << 20));
        }
        e => panic!("{e:?}"),
    }
    // A truncated OSC the terminal handles is dropped, not applied half.
    let mut input = b"\x1b]2;".to_vec();
    input.resize(input.len() + (1 << 20) + 100, b'a');
    input.push(0x07);
    t.advance(&input);
    assert!(events(&mut t).is_empty());
    assert_eq!(t.title(), "");
    // A cut-off SetUserVar or notify must not reach the app as a partial value.
    for head in ["\x1b]1337;SetUserVar=cmdterm_agent=", "\x1b]777;notify;t;"] {
        let mut input = head.as_bytes().to_vec();
        input.resize(input.len() + (1 << 20) + 100, b'A');
        input.push(0x07);
        t.advance(&input);
        assert!(events(&mut t).is_empty(), "{head}");
    }
}

#[test]
fn osc_state_resets_with_ris() {
    let mut t = term(
        2,
        10,
        "\x1b]2;x\x07\x1b]4;1;#fff\x07\x1b]133;A\x07\x1b]7;file:///a\x07\x1bc",
    );
    assert_eq!(t.title(), "");
    assert_eq!(t.palette().get(1), Rgb::new(0xcd, 0, 0));
    assert!(t.semantic_zones().is_empty());
    assert_eq!(t.current_dir(), None);
    events(&mut t);
}

#[test]
fn exit_status_of_an_empty_command_does_not_reach_the_previous_output() {
    use SemanticKind::*;
    let mut t = term(8, 20, "");
    t.advance(cycle(1, 1, 0).as_bytes());
    // Enter on an empty line: A, B, D with no C.
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07\r\n\x1b]133;D;7\x07");
    let zones = t.semantic_zones();
    let outputs: Vec<_> = zones.iter().filter(|z| z.kind == Output).collect();
    assert_eq!(outputs[0].exit_status, Some(0));
    assert!(outputs.iter().skip(1).all(|z| z.exit_status.is_none()));
}

/// `osc-empty-zone` in `COMPAT.md`: wezterm derives zones from cells, so a prompt with
/// nothing typed yet has no input zone. cmdterm-vt records the mark, so the zone exists
/// from the mark on.
#[test]
fn an_empty_input_zone_exists_from_the_mark_on() {
    let cycle = "\x1b]133;A\x07$ \x1b]133;B\x07cmd0\r\n\x1b]133;C\x07out0\r\n\x1b]133;D;0\x07";
    let input = format!("{cycle}\x1b]133;A\x07$ \x1b]133;B\x07");
    let t = term(6, 20, &input);
    let mut kinds: Vec<SemanticKind> = Vec::new();
    for z in t.semantic_zones() {
        if kinds.last() != Some(&z.kind) {
            kinds.push(z.kind);
        }
    }
    assert_eq!(
        kinds,
        [
            SemanticKind::Prompt,
            SemanticKind::Input,
            SemanticKind::Output,
            SemanticKind::Prompt,
            SemanticKind::Input
        ]
    );
}
