//! DEC modes, DECRQM, and the key, mouse, paste and focus encoders.
mod common;

use cmdterm_vt::{
    Config, KeyCode, KeyEventKind, KeypadKey, MediaKey, ModifierKey, Mods, MouseButton,
    MouseEncoding, MouseEvent, MouseEventKind, MouseTracking, Size, Terminal,
};
use common::term;

const SHIFT: Mods = Mods::SHIFT;
const ALT: Mods = Mods::ALT;
const CTRL: Mods = Mods::CTRL;
const NONE: Mods = Mods::NONE;

fn replies(t: &mut Terminal) -> String {
    let mut out = Vec::new();
    t.take_replies(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn feed(t: &mut Terminal, s: &str) {
    t.advance(s.as_bytes());
}

fn kev(t: &Terminal, key: KeyCode, mods: Mods, kind: KeyEventKind, text: Option<&str>) -> String {
    let mut out = Vec::new();
    t.encode_key(key, mods, kind, text, &mut out);
    String::from_utf8_lossy(&out).into_owned()
}

fn key(t: &Terminal, key: KeyCode, mods: Mods) -> String {
    kev(t, key, mods, KeyEventKind::Press, None)
}

fn rqm(t: &mut Terminal, private: bool, mode: u16) -> u8 {
    let q = if private { "?" } else { "" };
    feed(t, &format!("\x1b[{q}{mode}$p"));
    let r = replies(t);
    let prefix = format!("\x1b[{q}{mode};");
    let v = r.strip_prefix(&prefix).and_then(|s| s.strip_suffix("$y"));
    v.unwrap_or_else(|| panic!("bad reply {r:?}"))
        .parse()
        .unwrap()
}

// DECRQM and mode state --------------------------------------------------------------------

#[test]
fn every_mode_sets_resets_and_answers_decrqm() {
    // (mode, set by default)
    let modes: &[(u16, bool)] = &[
        (1, false),
        (5, false),
        (6, false),
        (7, true),
        (9, false),
        (12, false),
        (25, true),
        (40, false),
        (47, false),
        (66, false),
        (69, false),
        (1000, false),
        (1002, false),
        (1003, false),
        (1004, false),
        (1005, false),
        (1006, false),
        (1007, false),
        (1015, false),
        (1016, false),
        (1047, false),
        (1049, false),
        (2004, false),
        (2026, false),
    ];
    for &(m, default) in modes {
        let mut t = term(5, 20, "");
        let want = |on: bool| if on { 1 } else { 2 };
        assert_eq!(rqm(&mut t, true, m), want(default), "initial {m}");
        feed(&mut t, &format!("\x1b[?{m}h"));
        assert_eq!(rqm(&mut t, true, m), 1, "set {m}");
        feed(&mut t, &format!("\x1b[?{m}l"));
        assert_eq!(rqm(&mut t, true, m), 2, "reset {m}");
    }
}

#[test]
fn every_mode_saves_and_restores() {
    let modes: &[u16] = &[
        1, 3, 5, 6, 7, 9, 12, 25, 40, 66, 69, 1000, 1002, 1003, 1004, 1005, 1006, 1007, 1015, 1016,
        2004, 2026,
    ];
    for &m in modes {
        for saved_on in [true, false] {
            let mut t = term(5, 20, "\x1b[?40h");
            let (save_val, other) = if saved_on { ('h', 'l') } else { ('l', 'h') };
            let want = if saved_on { 1 } else { 2 };
            feed(
                &mut t,
                &format!("\x1b[?{m}{save_val}\x1b[?{m}s\x1b[?{m}{other}"),
            );
            assert_eq!(
                rqm(&mut t, true, m),
                3 - want,
                "flipped {m} saved_on={saved_on}"
            );
            feed(&mut t, &format!("\x1b[?{m}r"));
            assert_eq!(
                rqm(&mut t, true, m),
                want,
                "restored {m} saved_on={saved_on}"
            );
        }
    }
}

#[test]
fn deccolm_needs_mode_40() {
    let mut t = term(5, 20, "text\x1b[?3h");
    assert_eq!(rqm(&mut t, true, 3), 2);
    assert!(!t.modes().columns_132);
    assert_eq!(t.screen_text()[0], "text");
    feed(&mut t, "\x1b[?40h\x1b[3;3H\x1b[?3h");
    assert!(t.modes().columns_132);
    assert_eq!(rqm(&mut t, true, 3), 1);
    assert_eq!(t.screen_text()[0], "");
    assert_eq!((t.cursor().row, t.cursor().col), (0, 0));
    feed(&mut t, "\x1b[?3l");
    assert_eq!(rqm(&mut t, true, 3), 2);
}

#[test]
fn unknown_and_fixed_modes() {
    let mut t = term(5, 20, "");
    assert_eq!(rqm(&mut t, true, 9999), 0);
    assert_eq!(rqm(&mut t, true, 0), 0);
    assert_eq!(rqm(&mut t, false, 9999), 0);
    // Highlight tracking is recognized and stays off.
    feed(&mut t, "\x1b[?1001h");
    assert_eq!(rqm(&mut t, true, 1001), 4);
    assert!(!t.is_mouse_grabbed());
    // Pinned by the app's capability test.
    assert_eq!(rqm(&mut t, true, 2026), 2);
}

#[test]
fn ansi_modes() {
    let mut t = term(5, 20, "");
    assert_eq!(rqm(&mut t, false, 4), 2);
    assert_eq!(rqm(&mut t, false, 20), 2);
    feed(&mut t, "\x1b[4;20h");
    assert_eq!(rqm(&mut t, false, 4), 1);
    assert_eq!(rqm(&mut t, false, 20), 1);
    assert!(t.modes().newline && t.modes().insert);
    feed(&mut t, "\x1b[20l");
    assert!(!t.modes().newline);
}

#[test]
fn modes_struct_reflects_state() {
    let mut t = term(5, 20, "");
    let m = t.modes();
    assert!(!m.app_cursor && !m.app_keypad && !m.reverse_video && !m.bracketed_paste);
    assert_eq!(m.mouse, MouseTracking::Off);
    assert_eq!(m.mouse_encoding, MouseEncoding::X10);
    feed(
        &mut t,
        "\x1b[?1h\x1b=\x1b[?5h\x1b[?1004h\x1b[?1007h\x1b[?2004h\x1b[?2026h\x1b[?1002h\x1b[?1006h",
    );
    let m = t.modes();
    assert!(m.app_cursor && m.app_keypad && m.reverse_video);
    assert!(m.focus_events && m.alternate_scroll && m.bracketed_paste && m.synchronized_output);
    assert_eq!(m.mouse, MouseTracking::ButtonEvent);
    assert_eq!(m.mouse_encoding, MouseEncoding::Sgr);
    assert!(t.is_mouse_grabbed());
    feed(&mut t, "\x1b>");
    assert!(!t.modes().app_keypad);
}

#[test]
fn mouse_tracking_modes_replace_each_other_and_any_reset_turns_off() {
    let mut t = term(5, 20, "\x1b[?1000h");
    assert_eq!(t.modes().mouse, MouseTracking::Normal);
    feed(&mut t, "\x1b[?1003h");
    assert_eq!(t.modes().mouse, MouseTracking::AnyEvent);
    assert_eq!(rqm(&mut t, true, 1000), 2);
    assert_eq!(rqm(&mut t, true, 1003), 1);
    feed(&mut t, "\x1b[?9h");
    assert_eq!(t.modes().mouse, MouseTracking::X10);
    feed(&mut t, "\x1b[?1000l");
    assert_eq!(t.modes().mouse, MouseTracking::Off);
    assert!(!t.is_mouse_grabbed());
}

#[test]
fn save_and_restore_private_modes() {
    let mut t = term(5, 20, "\x1b[?1000;1006;2004;1h");
    feed(&mut t, "\x1b[?1000;1006;2004;1;7s");
    feed(&mut t, "\x1b[?1000;1006;2004;1;7l");
    let m = t.modes();
    assert!(!m.app_cursor && !m.bracketed_paste && !m.autowrap);
    assert_eq!(m.mouse, MouseTracking::Off);
    feed(&mut t, "\x1b[?1000;1006;2004;1;7r");
    let m = t.modes();
    assert!(m.app_cursor && m.bracketed_paste && m.autowrap);
    assert_eq!(m.mouse, MouseTracking::Normal);
    assert_eq!(m.mouse_encoding, MouseEncoding::Sgr);
}

#[test]
fn restore_of_the_full_mouse_list_keeps_tracking_and_encoding() {
    let all = "1000;1002;1003;1005;1006;1015;1016";
    for list in [all, "1016;1015;1006;1005;1003;1002;1000"] {
        let mut t = term(5, 20, "\x1b[?1002h\x1b[?1006h");
        feed(&mut t, &format!("\x1b[?{all}s"));
        feed(&mut t, "\x1b[?1l\x1b[?1000l");
        feed(&mut t, &format!("\x1b[?{list}r"));
        let m = t.modes();
        assert_eq!(m.mouse, MouseTracking::ButtonEvent);
        assert_eq!(m.mouse_encoding, MouseEncoding::Sgr);
    }
}

#[test]
fn resetting_another_encoding_keeps_the_active_one() {
    let mut t = term(5, 20, "\x1b[?1006h\x1b[?1015l");
    assert_eq!(t.modes().mouse_encoding, MouseEncoding::Sgr);
    feed(&mut t, "\x1b[?1006l");
    assert_eq!(t.modes().mouse_encoding, MouseEncoding::X10);
}

#[test]
fn restore_of_all_alt_screen_modes_switches_once() {
    let mut t = term(5, 20, "\x1b[?1049h");
    feed(&mut t, "\x1b[?47;1047;1049s\x1b[?1049l");
    assert!(!t.modes().alt_screen);
    feed(&mut t, "\x1b[?47;1047;1049r");
    assert!(t.modes().alt_screen);
}

#[test]
fn restore_brings_back_reset_state_and_ignores_unsaved_modes() {
    let mut t = term(5, 20, "\x1b[?25;2004s\x1b[?25l\x1b[?2004h\x1b[?1h");
    feed(&mut t, "\x1b[?25;2004;1r");
    assert!(t.cursor().visible);
    assert!(!t.modes().bracketed_paste);
    // Mode 1 was never saved, so it keeps its value.
    assert!(t.modes().app_cursor);
}

#[test]
fn private_save_does_not_touch_the_cursor_save() {
    let mut t = term(5, 20, "\x1b[2;3H\x1b[s\x1b[?1s\x1b[5;5H\x1b[u");
    assert_eq!((t.cursor().row, t.cursor().col), (1, 2));
    feed(&mut t, "\x1b[?1h\x1b[?1r");
    assert!(!t.modes().app_cursor);
}

#[test]
fn cursor_style_and_blink() {
    use cmdterm_vt::CursorShape::*;
    for (seq, shape, blink) in [
        ("\x1b[0 q", Block, true),
        ("\x1b[1 q", Block, true),
        ("\x1b[2 q", Block, false),
        ("\x1b[3 q", Underline, true),
        ("\x1b[4 q", Underline, false),
        ("\x1b[5 q", Bar, true),
        ("\x1b[6 q", Bar, false),
        ("\x1b[ q", Block, true),
    ] {
        let t = term(5, 20, &format!("\x1b[4 q{seq}"));
        let c = t.cursor();
        assert_eq!((c.shape, c.blink), (shape, blink), "{seq:?}");
        assert_eq!(t.modes().cursor_blink, blink);
    }
    // Out of range values are ignored.
    let t = term(5, 20, "\x1b[4 q\x1b[9 q");
    assert_eq!(t.cursor().shape, Underline);
    // Mode 12 changes only the blink.
    let t = term(5, 20, "\x1b[6 q\x1b[?12h");
    assert_eq!((t.cursor().shape, t.cursor().blink), (Bar, true));
}

#[test]
fn fresh_terminal_has_a_steady_cursor() {
    use cmdterm_vt::CursorShape::Block;
    let mut t = term(5, 20, "\x1b[?12$p");
    let c = t.cursor();
    assert_eq!((c.shape, c.blink), (Block, false));
    assert!(!t.modes().cursor_blink);
    let mut out = Vec::new();
    t.take_replies(&mut out);
    // Reset, like xterm.
    assert_eq!(out, b"\x1b[?12;2$y");
    // DECSCUSR 0 and 1 turn blinking on; so does a reset (RIS) go back to steady.
    for seq in ["\x1b[0 q", "\x1b[1 q"] {
        let t = term(5, 20, seq);
        assert!(t.cursor().blink, "{seq:?}");
    }
    let t = term(5, 20, "\x1b[1 q\x1bc");
    assert!(!t.cursor().blink);
}

#[test]
fn soft_reset_keeps_mouse_and_paste_modes() {
    let mut t = term(
        5,
        20,
        "\x1b[?1h\x1b=\x1b[20h\x1b[>4;2m\x1b[?1000h\x1b[?2004h\x1b[!p",
    );
    let m = t.modes();
    assert!(!m.app_cursor && !m.app_keypad);
    // xterm's DECSTR does not clear LINEFEED or modifyOtherKeys.
    assert!(m.newline);
    assert_eq!(m.modify_other_keys, 2);
    assert_eq!(m.mouse, MouseTracking::Normal);
    assert!(m.bracketed_paste);
    feed(&mut t, "\x1bc");
    assert_eq!(t.modes().mouse, MouseTracking::Off);
    assert!(!t.modes().bracketed_paste);
}

// Keys: legacy -----------------------------------------------------------------------------

struct K {
    setup: &'static str,
    key: KeyCode,
    mods: Mods,
    want: &'static str,
}

const fn k(setup: &'static str, key: KeyCode, mods: Mods, want: &'static str) -> K {
    K {
        setup,
        key,
        mods,
        want,
    }
}

fn run_keys(cases: &[K]) {
    for c in cases {
        let t = term(5, 20, c.setup);
        let got = key(&t, c.key, c.mods);
        assert_eq!(got, c.want, "{:?} {:?} after {:?}", c.key, c.mods, c.setup);
    }
}

use KeyCode as C;

#[test]
fn legacy_cursor_navigation_and_function_keys() {
    const APP: &str = "\x1b[?1h";
    run_keys(&[
        k("", C::Up, NONE, "\x1b[A"),
        k("", C::Down, NONE, "\x1b[B"),
        k("", C::Right, NONE, "\x1b[C"),
        k("", C::Left, NONE, "\x1b[D"),
        k(APP, C::Up, NONE, "\x1bOA"),
        k(APP, C::Left, NONE, "\x1bOD"),
        k(APP, C::Home, NONE, "\x1bOH"),
        k(APP, C::End, NONE, "\x1bOF"),
        k("", C::Up, SHIFT, "\x1b[1;2A"),
        k("", C::Up, ALT, "\x1b[1;3A"),
        k("", C::Up, CTRL, "\x1b[1;5A"),
        k(APP, C::Up, CTRL, "\x1b[1;5A"),
        k("", C::Right, Mods::from_bits(6), "\x1b[1;7C"),
        k("", C::Right, Mods::SUPER, "\x1b[1;9C"),
        k("", C::Home, NONE, "\x1b[H"),
        k("", C::End, NONE, "\x1b[F"),
        k("", C::Insert, NONE, "\x1b[2~"),
        k("", C::Delete, NONE, "\x1b[3~"),
        k("", C::PageUp, NONE, "\x1b[5~"),
        k("", C::PageDown, NONE, "\x1b[6~"),
        k("", C::Delete, SHIFT, "\x1b[3;2~"),
        k("", C::PageUp, CTRL, "\x1b[5;5~"),
        k("", C::F(1), NONE, "\x1bOP"),
        k("", C::F(2), NONE, "\x1bOQ"),
        k("", C::F(3), NONE, "\x1bOR"),
        k("", C::F(4), NONE, "\x1bOS"),
        k(APP, C::F(1), NONE, "\x1bOP"),
        k("", C::F(1), SHIFT, "\x1b[1;2P"),
        k("", C::F(4), CTRL, "\x1b[1;5S"),
        k("", C::F(5), NONE, "\x1b[15~"),
        k("", C::F(6), NONE, "\x1b[17~"),
        k("", C::F(7), NONE, "\x1b[18~"),
        k("", C::F(8), NONE, "\x1b[19~"),
        k("", C::F(9), NONE, "\x1b[20~"),
        k("", C::F(10), NONE, "\x1b[21~"),
        k("", C::F(11), NONE, "\x1b[23~"),
        k("", C::F(12), NONE, "\x1b[24~"),
        k("", C::F(5), CTRL, "\x1b[15;5~"),
        k("", C::F(12), SHIFT | ALT, "\x1b[24;4~"),
        k("", C::F(13), NONE, "\x1b[25~"),
        k("", C::F(20), NONE, "\x1b[34~"),
        k("", C::F(24), NONE, "\x1b[45~"),
        k("", C::F(25), NONE, ""),
        k("", C::F(0), NONE, ""),
    ]);
}

#[test]
fn legacy_text_and_control_keys() {
    run_keys(&[
        k("", C::Char('a'), NONE, "a"),
        k("", C::Char('A'), SHIFT, "A"),
        k("", C::Char(' '), NONE, " "),
        k("", C::Char('é'), NONE, "é"),
        k("", C::Char('a'), CTRL, "\x01"),
        k("", C::Char('z'), CTRL, "\x1a"),
        k("", C::Char('A'), CTRL | SHIFT, "\x01"),
        k("", C::Char('a'), ALT, "\x1ba"),
        k("", C::Char('a'), ALT | CTRL, "\x1b\x01"),
        k("", C::Char('é'), ALT, "\x1bé"),
        k("", C::Char(' '), CTRL, "\0"),
        k("", C::Char('['), CTRL, "\x1b"),
        k("", C::Char('\\'), CTRL, "\x1c"),
        k("", C::Char(']'), CTRL, "\x1d"),
        k("", C::Char('^'), CTRL, "\x1e"),
        k("", C::Char('_'), CTRL, "\x1f"),
        k("", C::Char('?'), CTRL, "\x7f"),
        k("", C::Char('8'), CTRL, "\x7f"),
        k("", C::Char('1'), CTRL, "1"),
        k("", C::Char(';'), CTRL, ";"),
        k("", C::Char('a'), Mods::SUPER, "a"),
        k("", C::Char('a'), CTRL | Mods::CAPS_LOCK, "\x01"),
        k("", C::Enter, NONE, "\r"),
        k("", C::Char('\r'), NONE, "\r"),
        k("\x1b[20h", C::Enter, NONE, "\r\n"),
        k("", C::Enter, ALT, "\x1b\r"),
        k("", C::Enter, CTRL, "\r"),
        k("", C::Tab, NONE, "\t"),
        k("", C::Char('\t'), NONE, "\t"),
        k("", C::Tab, SHIFT, "\x1b[Z"),
        k("", C::Tab, ALT, "\x1b\t"),
        k("", C::Tab, CTRL, "\t"),
        k("", C::Tab, CTRL | SHIFT, "\x1b[1;6Z"),
        k("", C::Backspace, NONE, "\x7f"),
        k("", C::Char('\x08'), NONE, "\x7f"),
        k("", C::Backspace, CTRL, "\x08"),
        k("", C::Backspace, ALT, "\x1b\x7f"),
        k("", C::Backspace, SHIFT, "\x7f"),
        k("", C::Escape, NONE, "\x1b"),
        k("", C::Char('\x1b'), NONE, "\x1b"),
        k("", C::Escape, ALT, "\x1b\x1b"),
        k("", C::Char('\x7f'), NONE, "\x1b[3~"),
        k("", C::Menu, NONE, ""),
        k("", C::CapsLock, NONE, ""),
        k("", C::Media(MediaKey::Play), NONE, ""),
        k("", C::Modifier(ModifierKey::LeftShift), SHIFT, ""),
    ]);
}

#[test]
fn legacy_keypad() {
    use KeypadKey::*;
    const APP: &str = "\x1b=";
    run_keys(&[
        k("", C::Keypad(Digit(5)), NONE, "5"),
        k("", C::Keypad(Decimal), NONE, "."),
        k("", C::Keypad(Divide), NONE, "/"),
        k("", C::Keypad(Multiply), NONE, "*"),
        k("", C::Keypad(Subtract), NONE, "-"),
        k("", C::Keypad(Add), NONE, "+"),
        k("", C::Keypad(Enter), NONE, "\r"),
        k("", C::Keypad(Equal), NONE, "="),
        k("", C::Keypad(Separator), NONE, ","),
        k("", C::Keypad(Up), NONE, "\x1b[A"),
        k("", C::Keypad(PageDown), NONE, "\x1b[6~"),
        k(APP, C::Keypad(Digit(0)), NONE, "\x1bOp"),
        k(APP, C::Keypad(Digit(9)), NONE, "\x1bOy"),
        k(APP, C::Keypad(Enter), NONE, "\x1bOM"),
        k(APP, C::Keypad(Add), NONE, "\x1bOk"),
        k(APP, C::Keypad(Subtract), NONE, "\x1bOm"),
        k(APP, C::Keypad(Multiply), NONE, "\x1bOj"),
        k(APP, C::Keypad(Divide), NONE, "\x1bOo"),
        k(APP, C::Keypad(Decimal), NONE, "\x1bOn"),
        k(APP, C::Keypad(Separator), NONE, "\x1bOl"),
        k(APP, C::Keypad(Equal), NONE, "\x1bOX"),
        k(APP, C::Keypad(Digit(0)), SHIFT, "\x1bO2p"),
        k("\x1b=\x1b>", C::Keypad(Digit(1)), NONE, "1"),
    ]);
}

#[test]
fn release_sends_nothing_in_legacy_mode() {
    let t = term(5, 20, "");
    assert_eq!(kev(&t, C::Char('a'), NONE, KeyEventKind::Release, None), "");
    assert_eq!(kev(&t, C::Up, NONE, KeyEventKind::Release, None), "");
    assert_eq!(kev(&t, C::Char('a'), NONE, KeyEventKind::Repeat, None), "a");
}

#[test]
fn modify_other_keys_levels() {
    const L1: &str = "\x1b[>4;1m";
    const L2: &str = "\x1b[>4;2m";
    run_keys(&[
        k(L1, C::Char('a'), CTRL, "\x01"),
        k(L1, C::Char('1'), CTRL, "\x1b[27;5;49~"),
        k(L1, C::Char(';'), CTRL, "\x1b[27;5;59~"),
        k(L1, C::Char('x'), ALT, "\x1bx"),
        k(L1, C::Char('A'), SHIFT, "A"),
        k(L1, C::Enter, SHIFT, "\x1b[27;2;13~"),
        k(L1, C::Enter, CTRL, "\x1b[27;5;13~"),
        k(L1, C::Enter, NONE, "\r"),
        k(L1, C::Tab, CTRL, "\x1b[27;5;9~"),
        k(L1, C::Backspace, CTRL, "\x08"),
        k(L1, C::Escape, SHIFT, "\x1b"),
        k(L1, C::Up, CTRL, "\x1b[1;5A"),
        k(L2, C::Char('a'), CTRL, "\x1b[27;5;97~"),
        k(L2, C::Char('x'), ALT, "\x1b[27;3;120~"),
        k(L2, C::Char('a'), CTRL | ALT, "\x1b[27;7;97~"),
        k(L2, C::Char('A'), SHIFT, "A"),
        k(L2, C::Char('a'), NONE, "a"),
        k(L2, C::Enter, SHIFT, "\x1b[27;2;13~"),
        k(L2, C::Enter, NONE, "\r"),
        k(L2, C::Tab, SHIFT, "\x1b[27;2;9~"),
        k(L2, C::Backspace, CTRL, "\x1b[27;5;127~"),
        k(L2, C::Escape, CTRL, "\x1b[27;5;27~"),
        k("\x1b[>4;2m\x1b[>4m", C::Char('a'), CTRL, "\x01"),
        k("\x1b[>4;2m\x1b[>4;0m", C::Char('a'), CTRL, "\x01"),
        k("\x1b[>4;2m\x1b[>4;7m", C::Char('a'), CTRL, "\x1b[27;5;97~"),
    ]);
    let mut t = term(5, 20, L2);
    feed(&mut t, "\x1b[?4m");
    assert_eq!(replies(&mut t), "\x1b[>4;2m");
    assert_eq!(t.modes().modify_other_keys, 2);
}

// Keys: kitty ------------------------------------------------------------------------------

fn kitty(flags: u8) -> Terminal {
    term(5, 20, &format!("\x1b[>{flags}u"))
}

#[test]
fn kitty_flag_stack_push_pop_set_query() {
    let mut t = term(5, 20, "");
    let q = |t: &mut Terminal| {
        feed(t, "\x1b[?u");
        replies(t)
    };
    assert_eq!(q(&mut t), "\x1b[?0u");
    feed(&mut t, "\x1b[>1u");
    assert_eq!(q(&mut t), "\x1b[?1u");
    feed(&mut t, "\x1b[>5u");
    assert_eq!(q(&mut t), "\x1b[?5u");
    assert_eq!(t.modes().kitty_flags, 5);
    feed(&mut t, "\x1b[<u");
    assert_eq!(q(&mut t), "\x1b[?1u");
    feed(&mut t, "\x1b[<u");
    assert_eq!(q(&mut t), "\x1b[?0u");
    // Popping an empty stack stays at the base.
    feed(&mut t, "\x1b[<5u");
    assert_eq!(q(&mut t), "\x1b[?0u");
    // Set: all, then add, then clear.
    feed(&mut t, "\x1b[=9u");
    assert_eq!(q(&mut t), "\x1b[?9u");
    feed(&mut t, "\x1b[=2;2u");
    assert_eq!(q(&mut t), "\x1b[?11u");
    feed(&mut t, "\x1b[=8;3u");
    assert_eq!(q(&mut t), "\x1b[?3u");
    feed(&mut t, "\x1b[=4;1u");
    assert_eq!(q(&mut t), "\x1b[?4u");
    // Unknown mode and extra bits are ignored.
    feed(&mut t, "\x1b[=1;9u");
    assert_eq!(q(&mut t), "\x1b[?4u");
    feed(&mut t, "\x1b[=255u");
    assert_eq!(q(&mut t), "\x1b[?31u");
    // Pop of several entries.
    feed(&mut t, "\x1b[=0u\x1b[>1u\x1b[>2u\x1b[>4u\x1b[<2u");
    assert_eq!(q(&mut t), "\x1b[?1u");
}

#[test]
fn kitty_flag_stack_is_bounded() {
    let mut t = term(5, 20, "");
    for v in 1..=9 {
        feed(&mut t, &format!("\x1b[>{v}u"));
    }
    feed(&mut t, "\x1b[<7u\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?2u");
    feed(&mut t, "\x1b[<u\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?0u");
}

#[test]
fn kitty_flags_are_per_screen_and_reset_by_ris() {
    let mut t = term(5, 20, "\x1b[>1u");
    feed(&mut t, "\x1b[?1049h\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?0u");
    feed(&mut t, "\x1b[>8u\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?8u");
    assert_eq!(t.modes().kitty_flags, 8);
    feed(&mut t, "\x1b[?1049l\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?1u");
    feed(&mut t, "\x1b[!p\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?1u");
    feed(&mut t, "\x1bc\x1b[?u");
    assert_eq!(replies(&mut t), "\x1b[?0u");
}

#[test]
fn kitty_disambiguate() {
    use KeypadKey::*;
    let d = "\x1b[>1u";
    run_keys(&[
        k(d, C::Escape, NONE, "\x1b[27u"),
        k(d, C::Char('\x1b'), NONE, "\x1b[27u"),
        k(d, C::Escape, ALT, "\x1b[27;3u"),
        k(d, C::Enter, NONE, "\r"),
        k(d, C::Enter, CTRL, "\x1b[13;5u"),
        k(d, C::Enter, SHIFT, "\x1b[13;2u"),
        k(d, C::Tab, NONE, "\t"),
        k(d, C::Tab, SHIFT, "\x1b[9;2u"),
        k(d, C::Backspace, NONE, "\x7f"),
        k(d, C::Backspace, CTRL, "\x1b[127;5u"),
        k(d, C::Char('a'), NONE, "a"),
        k(d, C::Char('A'), SHIFT, "A"),
        k(d, C::Char('é'), NONE, "é"),
        k(d, C::Char('a'), CTRL, "\x1b[97;5u"),
        k(d, C::Char('a'), ALT, "\x1b[97;3u"),
        k(d, C::Char('a'), ALT | CTRL, "\x1b[97;7u"),
        k(d, C::Char('A'), CTRL | SHIFT, "\x1b[97;6u"),
        k(d, C::Char('a'), Mods::SUPER, "\x1b[97;9u"),
        k(d, C::Char('a'), CTRL | Mods::CAPS_LOCK, "\x1b[97;69u"),
        k(d, C::Char('a'), Mods::CAPS_LOCK, "a"),
        k(d, C::Char('['), CTRL, "\x1b[91;5u"),
        k(d, C::Char('é'), CTRL, "\x1b[233;5u"),
        k(d, C::Up, NONE, "\x1b[A"),
        k("\x1b[?1h\x1b[>1u", C::Up, NONE, "\x1bOA"),
        k(d, C::Up, SHIFT, "\x1b[1;2A"),
        k(d, C::Home, CTRL, "\x1b[1;5H"),
        k(d, C::F(1), NONE, "\x1bOP"),
        k(d, C::F(1), SHIFT, "\x1b[1;2P"),
        k(d, C::F(3), SHIFT, "\x1b[13;2~"),
        k(d, C::F(5), CTRL, "\x1b[15;5~"),
        k(d, C::Delete, ALT, "\x1b[3;3~"),
        k(d, C::F(13), NONE, "\x1b[57376u"),
        k(d, C::F(35), NONE, "\x1b[57398u"),
        k(d, C::F(35), CTRL, "\x1b[57398;5u"),
        k(d, C::CapsLock, NONE, "\x1b[57358u"),
        k(d, C::ScrollLock, NONE, "\x1b[57359u"),
        k(d, C::NumLock, NONE, "\x1b[57360u"),
        k(d, C::PrintScreen, NONE, "\x1b[57361u"),
        k(d, C::Pause, NONE, "\x1b[57362u"),
        k(d, C::Menu, NONE, "\x1b[57363u"),
        k(d, C::Media(MediaKey::Play), NONE, "\x1b[57428u"),
        k(d, C::Media(MediaKey::MuteVolume), NONE, "\x1b[57440u"),
        // Kitty spec, "Disambiguate escape codes": all keypad keys are reported as
        // separate keys with CSI u.
        k(d, C::Keypad(Digit(0)), NONE, "\x1b[57399u"),
        k(d, C::Keypad(Enter), NONE, "\x1b[57414u"),
        k(d, C::Keypad(Digit(1)), CTRL, "\x1b[57400;5u"),
        k(d, C::Keypad(Left), NONE, "\x1b[57417u"),
        k(d, C::Keypad(Begin), NONE, "\x1b[57427u"),
        k(d, C::Modifier(ModifierKey::LeftShift), SHIFT, ""),
    ]);
}

#[test]
fn kitty_report_all_keys() {
    use KeypadKey::*;
    let a = "\x1b[>8u";
    run_keys(&[
        k(a, C::Char('a'), NONE, "\x1b[97u"),
        k(a, C::Char('A'), SHIFT, "\x1b[97;2u"),
        k(a, C::Char('a'), CTRL, "\x1b[97;5u"),
        k(a, C::Char(' '), NONE, "\x1b[32u"),
        k(a, C::Enter, NONE, "\x1b[13u"),
        k(a, C::Tab, NONE, "\x1b[9u"),
        k(a, C::Backspace, NONE, "\x1b[127u"),
        k(a, C::Escape, NONE, "\x1b[27u"),
        k(a, C::Up, NONE, "\x1b[A"),
        k(a, C::F(1), NONE, "\x1b[P"),
        k(a, C::F(3), NONE, "\x1b[13~"),
        k(a, C::F(12), NONE, "\x1b[24~"),
        k(a, C::Delete, NONE, "\x1b[3~"),
        k(a, C::Keypad(Digit(5)), NONE, "\x1b[57404u"),
        k(a, C::Keypad(Enter), NONE, "\x1b[57414u"),
        k(
            a,
            C::Modifier(ModifierKey::LeftShift),
            NONE,
            "\x1b[57441;2u",
        ),
        k(
            a,
            C::Modifier(ModifierKey::LeftControl),
            SHIFT,
            "\x1b[57442;6u",
        ),
        k(a, C::Modifier(ModifierKey::RightAlt), NONE, "\x1b[57449;3u"),
        k(
            a,
            C::Modifier(ModifierKey::LeftSuper),
            NONE,
            "\x1b[57444;9u",
        ),
        k(
            a,
            C::Modifier(ModifierKey::LeftHyper),
            NONE,
            "\x1b[57445;17u",
        ),
        k(
            a,
            C::Modifier(ModifierKey::LeftMeta),
            NONE,
            "\x1b[57446;33u",
        ),
        k(
            a,
            C::Modifier(ModifierKey::IsoLevel3Shift),
            NONE,
            "\x1b[57453u",
        ),
        k(
            a,
            C::Modifier(ModifierKey::IsoLevel5Shift),
            NONE,
            "\x1b[57454u",
        ),
    ]);
}

#[test]
fn kitty_event_types() {
    use KeyEventKind::*;
    let cases: &[(u8, KeyCode, Mods, KeyEventKind, &str)] = &[
        (3, C::Char('a'), NONE, Press, "a"),
        (3, C::Char('a'), NONE, Repeat, "a"),
        (3, C::Char('a'), NONE, Release, ""),
        (3, C::Char('a'), CTRL, Press, "\x1b[97;5u"),
        (3, C::Char('a'), CTRL, Repeat, "\x1b[97;5:2u"),
        (3, C::Char('a'), CTRL, Release, "\x1b[97;5:3u"),
        (3, C::Up, NONE, Press, "\x1b[A"),
        (3, C::Up, NONE, Repeat, "\x1b[1;1:2A"),
        (3, C::Up, NONE, Release, "\x1b[1;1:3A"),
        (3, C::Up, SHIFT, Release, "\x1b[1;2:3A"),
        (3, C::F(3), NONE, Release, "\x1b[13;1:3~"),
        (3, C::Enter, NONE, Release, ""),
        (3, C::Tab, NONE, Release, ""),
        (3, C::Backspace, NONE, Release, ""),
        (3, C::Escape, NONE, Press, "\x1b[27u"),
        (3, C::Escape, NONE, Release, "\x1b[27;1:3u"),
        (11, C::Char('a'), NONE, Press, "\x1b[97u"),
        (11, C::Char('a'), NONE, Repeat, "\x1b[97;1:2u"),
        (11, C::Char('a'), NONE, Release, "\x1b[97;1:3u"),
        (11, C::Char('A'), SHIFT, Press, "\x1b[97;2u"),
        (11, C::Char('A'), SHIFT, Release, "\x1b[97;2:3u"),
        (11, C::Enter, NONE, Release, "\x1b[13;1:3u"),
        (11, C::Backspace, NONE, Press, "\x1b[127u"),
        (
            11,
            C::Modifier(ModifierKey::LeftShift),
            SHIFT,
            Press,
            "\x1b[57441;2u",
        ),
        (
            11,
            C::Modifier(ModifierKey::LeftShift),
            SHIFT,
            Release,
            "\x1b[57441;1:3u",
        ),
        (
            11,
            C::Modifier(ModifierKey::LeftControl),
            CTRL | SHIFT,
            Release,
            "\x1b[57442;2:3u",
        ),
        // Without the event types flag, repeats look like presses and releases vanish.
        (1, C::Char('a'), CTRL, Repeat, "\x1b[97;5u"),
        (1, C::Char('a'), CTRL, Release, ""),
        (8, C::Char('a'), NONE, Release, ""),
    ];
    for &(flags, key, mods, kind, want) in cases {
        let t = kitty(flags);
        assert_eq!(
            kev(&t, key, mods, kind, None),
            want,
            "flags {flags} {key:?} {kind:?}"
        );
    }
}

#[test]
fn kitty_alternate_keys_and_text() {
    let cases: &[(u8, KeyCode, Mods, Option<&str>, &str)] = &[
        // Alternate keys: key code, then the shifted key.
        (13, C::Char('A'), SHIFT, None, "\x1b[97:65;2u"),
        (13, C::Char('a'), SHIFT, None, "\x1b[97:65;2u"),
        (13, C::Char('!'), SHIFT, None, "\x1b[49:33;2u"),
        (13, C::Char('a'), NONE, None, "\x1b[97u"),
        (13, C::Char('A'), SHIFT | CTRL, None, "\x1b[97:65;6u"),
        (5, C::Char('A'), SHIFT | CTRL, None, "\x1b[97:65;6u"),
        (9, C::Char('A'), SHIFT, None, "\x1b[97;2u"),
        // Associated text needs report all keys.
        (24, C::Char('a'), NONE, None, "\x1b[97;;97u"),
        (24, C::Char('A'), SHIFT, None, "\x1b[97;2;65u"),
        (24, C::Char('a'), NONE, Some("a"), "\x1b[97;;97u"),
        (24, C::Char('a'), NONE, Some("é"), "\x1b[97;;233u"),
        (24, C::Char('a'), NONE, Some("ab"), "\x1b[97;;97:98u"),
        (24, C::Char('a'), CTRL, None, "\x1b[97;5u"),
        // Control characters are dropped from the text; no text left omits the field.
        (24, C::Char('a'), NONE, Some("\x01a"), "\x1b[97;;97u"),
        (
            24,
            C::Char('a'),
            NONE,
            Some("a\x7fb\u{85}"),
            "\x1b[97;;97:98u",
        ),
        (24, C::Char('a'), NONE, Some("\r"), "\x1b[97u"),
        (24, C::Char('a'), SHIFT, Some("\x1b\x7f"), "\x1b[97;2u"),
        (29, C::Char('A'), SHIFT, None, "\x1b[97:65;2;65u"),
        (24, C::Enter, NONE, None, "\x1b[13u"),
        (24, C::Up, NONE, None, "\x1b[A"),
        (16, C::Char('a'), NONE, None, "a"),
        (17, C::Char('a'), CTRL, None, "\x1b[97;5u"),
        (27, C::Char('a'), NONE, None, "\x1b[97;;97u"),
    ];
    for &(flags, key, mods, text, want) in cases {
        let t = kitty(flags);
        assert_eq!(
            kev(&t, key, mods, KeyEventKind::Press, text),
            want,
            "flags {flags} {key:?} {mods:?}"
        );
    }
    // No text on release.
    let t = kitty(27);
    assert_eq!(
        kev(&t, C::Char('a'), NONE, KeyEventKind::Release, Some("a")),
        "\x1b[97;1:3u"
    );
}

#[test]
fn kitty_flags_without_escape_codes_keep_legacy_for_modified_keys() {
    // Flags 2 and 4 alone do not turn on escape codes for text keys.
    let t = kitty(2);
    assert_eq!(key(&t, C::Char('a'), CTRL), "\x01");
    assert_eq!(key(&t, C::Char('a'), ALT), "\x1ba");
    assert_eq!(key(&t, C::Escape, NONE), "\x1b");
    assert_eq!(key(&t, C::Enter, NONE), "\r");
    let t = kitty(4);
    assert_eq!(key(&t, C::Char('a'), CTRL), "\x01");
    // Keys without a legacy-only form above still use their legacy encoding.
    let legacy = term(5, 20, "");
    for flags in [2, 4, 16] {
        let t = kitty(flags);
        for (code, mods) in [
            (C::F(13), NONE),
            (C::F(24), NONE),
            (C::Keypad(KeypadKey::Left), NONE),
            (C::Keypad(KeypadKey::Left), CTRL),
            (C::Keypad(KeypadKey::Begin), NONE),
        ] {
            let got = key(&t, code, mods);
            assert!(
                !got.is_empty(),
                "flags {flags} {code:?} {mods:?} sent nothing"
            );
            assert_eq!(
                got,
                key(&legacy, code, mods),
                "flags {flags} {code:?} {mods:?}"
            );
        }
    }
    assert_eq!(key(&legacy, C::F(13), NONE), "\x1b[25~");
}

#[test]
fn kitty_spec_examples() {
    // From the "Progressive enhancement" and "Key events" sections of the protocol.
    // Disambiguate: Escape and modified text keys use CSI u.
    let t = kitty(1);
    assert_eq!(key(&t, C::Escape, NONE), "\x1b[27u");
    assert_eq!(key(&t, C::Char('c'), CTRL), "\x1b[99;5u");
    assert_eq!(key(&t, C::Char('i'), CTRL), "\x1b[105;5u");
    assert_eq!(key(&t, C::Char('m'), CTRL), "\x1b[109;5u");
    assert_eq!(key(&t, C::Char('['), CTRL), "\x1b[91;5u");
    assert_eq!(key(&t, C::Tab, NONE), "\t");
    // Report all keys with event types: the shift key itself.
    let t = kitty(11);
    assert_eq!(
        kev(
            &t,
            C::Modifier(ModifierKey::LeftShift),
            SHIFT,
            KeyEventKind::Press,
            None
        ),
        "\x1b[57441;2u"
    );
    assert_eq!(
        kev(
            &t,
            C::Modifier(ModifierKey::LeftShift),
            NONE,
            KeyEventKind::Release,
            None
        ),
        "\x1b[57441;1:3u"
    );
    // The functional key table: Home, End, F3 use the legacy final bytes.
    let t = kitty(8);
    assert_eq!(key(&t, C::Home, NONE), "\x1b[H");
    assert_eq!(key(&t, C::End, NONE), "\x1b[F");
    assert_eq!(key(&t, C::Insert, NONE), "\x1b[2~");
    assert_eq!(key(&t, C::PageUp, SHIFT), "\x1b[5;2~");
    // Alternate keys example: shift+a pressed with a US layout.
    let t = kitty(1 | 4 | 8);
    assert_eq!(key(&t, C::Char('A'), SHIFT), "\x1b[97:65;2u");
}

// Mouse ------------------------------------------------------------------------------------

fn mev(kind: MouseEventKind, button: MouseButton, col: usize, row: usize) -> MouseEvent {
    MouseEvent::new(kind, button, col, row)
}

fn mouse(t: &mut Terminal, ev: MouseEvent) -> Option<String> {
    let mut out = Vec::new();
    if t.encode_mouse(&ev, &mut out) {
        Some(String::from_utf8_lossy(&out).into_owned())
    } else {
        assert!(out.is_empty());
        None
    }
}

use MouseButton as B;
use MouseEventKind::{Move, Press, Release};

#[test]
fn mouse_is_not_encoded_unless_grabbed() {
    let mut t = term(24, 80, "");
    assert!(!t.is_mouse_grabbed());
    assert_eq!(mouse(&mut t, mev(Press, B::Left, 1, 1)), None);
    feed(&mut t, "\x1b[?1000h");
    assert!(t.is_mouse_grabbed());
    assert!(mouse(&mut t, mev(Press, B::Left, 1, 1)).is_some());
    feed(&mut t, "\x1b[?1000l");
    assert!(!t.is_mouse_grabbed());
    assert_eq!(mouse(&mut t, mev(Press, B::Left, 1, 1)), None);
}

#[test]
fn mouse_default_encoding() {
    let mut t = term(24, 80, "\x1b[?1000h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 0, 0)).unwrap(),
        "\x1b[M !!"
    );
    assert_eq!(
        mouse(&mut t, mev(Release, B::Left, 0, 0)).unwrap(),
        "\x1b[M#!!"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 4, 2)).unwrap(),
        "\x1b[M %#"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::Middle, 4, 2)).unwrap(),
        "\x1b[M!%#"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::Right, 4, 2)).unwrap(),
        "\x1b[M\"%#"
    );
    let mut ev = mev(Press, B::Left, 4, 2);
    ev.mods = SHIFT | ALT | CTRL;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[M<%#");
    assert_eq!(
        mouse(&mut t, mev(Press, B::WheelUp, 4, 2)).unwrap(),
        "\x1b[M`%#"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::WheelDown, 4, 2)).unwrap(),
        "\x1b[Ma%#"
    );
    // Wheel events have no release.
    assert_eq!(mouse(&mut t, mev(Release, B::WheelUp, 4, 2)), None);
    // Column 222 is the last one a byte can hold.
    let mut t = term(24, 300, "\x1b[?1000h");
    assert!(mouse(&mut t, mev(Press, B::Left, 222, 0)).is_some());
    assert_eq!(mouse(&mut t, mev(Press, B::Left, 223, 0)), None);
}

#[test]
fn mouse_x10_compat_mode_9() {
    let mut t = term(24, 80, "\x1b[?9h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 0, 0)).unwrap(),
        "\x1b[M !!"
    );
    let mut ev = mev(Press, B::Right, 0, 0);
    ev.mods = CTRL;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[M\"!!");
    assert_eq!(mouse(&mut t, mev(Release, B::Left, 0, 0)), None);
    assert_eq!(mouse(&mut t, mev(Press, B::WheelUp, 0, 0)), None);
    assert_eq!(mouse(&mut t, mev(Move, B::None, 1, 0)), None);
}

#[test]
fn mouse_sgr() {
    let mut t = term(24, 80, "\x1b[?1000h\x1b[?1006h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 4, 2)).unwrap(),
        "\x1b[<0;5;3M"
    );
    assert_eq!(
        mouse(&mut t, mev(Release, B::Left, 4, 2)).unwrap(),
        "\x1b[<0;5;3m"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::Right, 4, 2)).unwrap(),
        "\x1b[<2;5;3M"
    );
    assert_eq!(
        mouse(&mut t, mev(Release, B::Right, 4, 2)).unwrap(),
        "\x1b[<2;5;3m"
    );
    // Release without a button uses the one that was pressed.
    mouse(&mut t, mev(Press, B::Middle, 4, 2)).unwrap();
    assert_eq!(
        mouse(&mut t, mev(Release, B::None, 4, 2)).unwrap(),
        "\x1b[<1;5;3m"
    );
    let mut ev = mev(Press, B::Right, 4, 2);
    ev.mods = CTRL | SHIFT;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<22;5;3M");
    assert_eq!(
        mouse(&mut t, mev(Press, B::WheelUp, 79, 23)).unwrap(),
        "\x1b[<64;80;24M"
    );
    let mut ev = mev(Press, B::WheelDown, 0, 0);
    ev.mods = ALT;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<73;1;1M");
    assert_eq!(
        mouse(&mut t, mev(Press, B::WheelLeft, 0, 0)).unwrap(),
        "\x1b[<66;1;1M"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::WheelRight, 0, 0)).unwrap(),
        "\x1b[<67;1;1M"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::Back, 0, 0)).unwrap(),
        "\x1b[<128;1;1M"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::Forward, 0, 0)).unwrap(),
        "\x1b[<129;1;1M"
    );
    // Large coordinates and coordinates past the screen.
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 999, 999)).unwrap(),
        "\x1b[<0;80;24M"
    );
    let mut t = term(500, 500, "\x1b[?1000h\x1b[?1006h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 399, 299)).unwrap(),
        "\x1b[<0;400;300M"
    );
}

#[test]
fn mouse_urxvt() {
    let mut t = term(24, 80, "\x1b[?1000h\x1b[?1015h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 4, 2)).unwrap(),
        "\x1b[32;5;3M"
    );
    assert_eq!(
        mouse(&mut t, mev(Release, B::Left, 4, 2)).unwrap(),
        "\x1b[35;5;3M"
    );
    assert_eq!(
        mouse(&mut t, mev(Press, B::WheelUp, 4, 2)).unwrap(),
        "\x1b[96;5;3M"
    );
}

#[test]
fn mouse_utf8() {
    let mut t = term(24, 400, "\x1b[?1000h\x1b[?1005h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 4, 2)).unwrap(),
        "\x1b[M %#"
    );
    // Column 200 + 33 = 233 = U+00E9, two bytes.
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 200, 0))
            .unwrap()
            .as_bytes(),
        b"\x1b[M \xc3\xa9!"
    );
    // Column 300 + 33 = 333 = U+014D.
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 300, 0))
            .unwrap()
            .as_bytes(),
        b"\x1b[M \xc5\x8d!"
    );
    assert_eq!(t.modes().mouse_encoding, MouseEncoding::Utf8);
}

#[test]
fn mouse_sgr_pixels() {
    let mut t = term(24, 80, "\x1b[?1000h\x1b[?1016h");
    // Without a cell size the report falls back to cells.
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 4, 2)).unwrap(),
        "\x1b[<0;5;3M"
    );
    t.set_cell_pixels(8, 16);
    let mut ev = mev(Press, B::Left, 4, 2);
    ev.dx = 3;
    ev.dy = 5;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<0;36;38M");
    ev.kind = Release;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<0;36;38m");
    let cfg = Config {
        cell_pixels: (10, 20),
        ..Config::default()
    };
    let mut t = Terminal::new(Size { rows: 24, cols: 80 }, cfg);
    feed(&mut t, "\x1b[?1000h\x1b[?1016h");
    assert_eq!(
        mouse(&mut t, mev(Press, B::Left, 1, 1)).unwrap(),
        "\x1b[<0;11;21M"
    );
}

#[test]
fn mouse_motion_modes() {
    // 1000 reports no motion.
    let mut t = term(24, 80, "\x1b[?1000h\x1b[?1006h");
    mouse(&mut t, mev(Press, B::Left, 1, 1)).unwrap();
    assert_eq!(mouse(&mut t, mev(Move, B::None, 2, 1)), None);

    // 1002 reports motion with a button held, once per cell.
    let mut t = term(24, 80, "\x1b[?1002h\x1b[?1006h");
    assert_eq!(mouse(&mut t, mev(Move, B::None, 2, 1)), None);
    mouse(&mut t, mev(Press, B::Left, 1, 1)).unwrap();
    assert_eq!(
        mouse(&mut t, mev(Move, B::None, 2, 1)).unwrap(),
        "\x1b[<32;3;2M"
    );
    assert_eq!(mouse(&mut t, mev(Move, B::None, 2, 1)), None);
    assert_eq!(
        mouse(&mut t, mev(Move, B::None, 3, 1)).unwrap(),
        "\x1b[<32;4;2M"
    );
    // A move may name the button itself.
    assert_eq!(
        mouse(&mut t, mev(Move, B::Right, 4, 1)).unwrap(),
        "\x1b[<34;5;2M"
    );
    mouse(&mut t, mev(Release, B::Left, 4, 1)).unwrap();
    assert_eq!(mouse(&mut t, mev(Move, B::None, 5, 1)), None);

    // 1003 reports all motion; with no button the code is 3 + 32.
    let mut t = term(24, 80, "\x1b[?1003h\x1b[?1006h");
    assert_eq!(
        mouse(&mut t, mev(Move, B::None, 2, 1)).unwrap(),
        "\x1b[<35;3;2M"
    );
    let mut ev = mev(Move, B::None, 3, 1);
    ev.mods = SHIFT;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<39;4;2M");

    // Default encoding: 32 + 35 = 67 = 'C'.
    let mut t = term(24, 80, "\x1b[?1003h");
    assert_eq!(
        mouse(&mut t, mev(Move, B::None, 0, 0)).unwrap(),
        "\x1b[MC!!"
    );
}

#[test]
fn mouse_drag_across_pixels_reports_each_pixel_in_1016() {
    let mut t = term(24, 80, "\x1b[?1003h\x1b[?1016h");
    t.set_cell_pixels(8, 16);
    let mut ev = mev(Move, B::None, 1, 1);
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<35;9;17M");
    assert_eq!(mouse(&mut t, ev), None);
    ev.dx = 1;
    assert_eq!(mouse(&mut t, ev).unwrap(), "\x1b[<35;10;17M");
}

#[test]
fn mouse_history_survives_tracking_changes() {
    let mut t = term(24, 80, "");
    // A press while tracking is off still counts as held for a later drag.
    mouse(&mut t, mev(Press, B::Left, 1, 1));
    feed(&mut t, "\x1b[?1002h\x1b[?1006h");
    assert_eq!(
        mouse(&mut t, mev(Move, B::None, 2, 1)).unwrap(),
        "\x1b[<32;3;2M"
    );
}

#[test]
fn mouse_encoding_reset_returns_to_default() {
    let mut t = term(24, 80, "\x1b[?1000h\x1b[?1006h");
    assert_eq!(t.modes().mouse_encoding, MouseEncoding::Sgr);
    feed(&mut t, "\x1b[?1006l");
    assert_eq!(t.modes().mouse_encoding, MouseEncoding::X10);
}

#[test]
fn alternate_scroll_turns_wheel_into_cursor_keys() {
    let mut t = term(24, 80, "\x1b[?1007h");
    let mut out = Vec::new();
    // Not on the alternate screen.
    assert!(!t.encode_alt_scroll(B::WheelUp, &mut out));
    feed(&mut t, "\x1b[?1049h");
    assert!(t.encode_alt_scroll(B::WheelUp, &mut out));
    assert_eq!(out, b"\x1b[A");
    out.clear();
    assert!(t.encode_alt_scroll(B::WheelDown, &mut out));
    assert_eq!(out, b"\x1b[B");
    out.clear();
    feed(&mut t, "\x1b[?1h");
    assert!(t.encode_alt_scroll(B::WheelUp, &mut out));
    assert_eq!(out, b"\x1bOA");
    out.clear();
    assert!(!t.encode_alt_scroll(B::Left, &mut out));
    // Grabbed mouse: reports win.
    feed(&mut t, "\x1b[?1000h");
    assert!(!t.encode_alt_scroll(B::WheelUp, &mut out));
    feed(&mut t, "\x1b[?1000l\x1b[?1007l");
    assert!(!t.encode_alt_scroll(B::WheelUp, &mut out));
    assert!(out.is_empty());
}

// Paste and focus --------------------------------------------------------------------------

fn paste(t: &Terminal, text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    t.encode_paste(text, &mut out);
    out
}

#[test]
fn paste_without_brackets() {
    let t = term(5, 20, "");
    assert_eq!(paste(&t, "plain"), b"plain");
    assert_eq!(paste(&t, "a\nb"), b"a\rb");
    assert_eq!(paste(&t, "a\r\nb"), b"a\rb");
    assert_eq!(paste(&t, "a\r\n\r\nb"), b"a\r\rb");
    // Only brackets sanitize.
    assert_eq!(paste(&t, "a\x1bb"), b"a\x1bb");
    assert_eq!(paste(&t, ""), b"");
}

#[test]
fn bracketed_paste_wraps_and_sanitizes() {
    let t = term(5, 20, "\x1b[?2004h");
    assert_eq!(paste(&t, "hi"), b"\x1b[200~hi\x1b[201~");
    assert_eq!(paste(&t, ""), b"\x1b[200~\x1b[201~");
    assert_eq!(paste(&t, "a\nb"), b"\x1b[200~a\rb\x1b[201~");
    assert_eq!(paste(&t, "a\r\nb"), b"\x1b[200~a\rb\x1b[201~");
    // A pasted end marker cannot close the bracket.
    assert_eq!(paste(&t, "a\x1b[201~b"), b"\x1b[200~a[201~b\x1b[201~");
    assert_eq!(paste(&t, "a\x1b[200~b"), b"\x1b[200~a[200~b\x1b[201~");
    assert_eq!(paste(&t, "\x1b\x1b"), b"\x1b[200~\x1b[201~");
    // C1 CSI is dropped; other two-byte characters survive.
    assert_eq!(paste(&t, "x\u{9b}y"), b"\x1b[200~xy\x1b[201~");
    assert_eq!(
        paste(&t, "\u{e9}\u{9a}"),
        "\x1b[200~\u{e9}\u{9a}\x1b[201~".as_bytes()
    );
    assert_eq!(paste(&t, "日本"), "\x1b[200~日本\x1b[201~".as_bytes());
    assert_eq!(paste(&t, "tab\tok"), b"\x1b[200~tab\tok\x1b[201~");
}

#[test]
fn focus_reports() {
    let mut t = term(5, 20, "");
    let mut out = Vec::new();
    assert!(!t.encode_focus(true, &mut out));
    assert!(out.is_empty());
    feed(&mut t, "\x1b[?1004h");
    assert!(t.encode_focus(true, &mut out));
    assert_eq!(out, b"\x1b[I");
    out.clear();
    assert!(t.encode_focus(false, &mut out));
    assert_eq!(out, b"\x1b[O");
    feed(&mut t, "\x1b[?1004l");
    out.clear();
    assert!(!t.encode_focus(false, &mut out));
}

// Allocation -------------------------------------------------------------------------------

#[test]
fn encoders_do_not_allocate_once_the_buffer_is_warm() {
    let mut t = term(
        24,
        80,
        "\x1b[>31u\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h",
    );
    let text = "hello\nworld\x1b[201~ \u{9b}é";
    let keys = [
        (C::Char('a'), NONE),
        (C::Char('A'), SHIFT | CTRL),
        (C::Up, CTRL),
        (C::F(5), SHIFT),
        (C::F(20), NONE),
        (C::Enter, NONE),
        (C::Modifier(ModifierKey::LeftShift), SHIFT),
        (C::Keypad(KeypadKey::Digit(3)), NONE),
    ];
    let mut out = Vec::with_capacity(1024);
    let run = |t: &mut Terminal, out: &mut Vec<u8>| {
        for &(key, mods) in &keys {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                out.clear();
                t.encode_key(key, mods, kind, Some("x"), out);
            }
        }
        for (i, kind) in [Press, Move, Release].into_iter().enumerate() {
            out.clear();
            t.encode_mouse(&mev(kind, B::Left, 3 + i, 4), out);
        }
        out.clear();
        t.encode_paste(text, out);
        out.clear();
        t.encode_focus(true, out);
    };
    run(&mut t, &mut out);
    let cap = out.capacity();
    let info = allocation_counter::measure(|| run(&mut t, &mut out));
    assert_eq!(info.count_total, 0, "{info:?}");
    assert_eq!(out.capacity(), cap);

    // The legacy paths too.
    let mut t = term(24, 80, "\x1b[?1h\x1b=\x1b[>4;2m\x1b[?1000h");
    run(&mut t, &mut out);
    let info = allocation_counter::measure(|| run(&mut t, &mut out));
    assert_eq!(info.count_total, 0, "{info:?}");
}

// App keys ---------------------------------------------------------------------------------

#[test]
fn every_key_the_app_maps_today_has_an_equivalent() {
    // crates/cmdterm/src/term/keys.rs: Char, Insert, Home, End, PageUp, PageDown, the four
    // arrows, F1 to F12, and Enter, Tab, Space, Backspace, Escape and Delete sent as
    // control characters.
    let t = term(5, 20, "");
    let want: &[(KeyCode, &str)] = &[
        (C::Char('x'), "x"),
        (C::Insert, "\x1b[2~"),
        (C::Home, "\x1b[H"),
        (C::End, "\x1b[F"),
        (C::PageUp, "\x1b[5~"),
        (C::PageDown, "\x1b[6~"),
        (C::Left, "\x1b[D"),
        (C::Right, "\x1b[C"),
        (C::Up, "\x1b[A"),
        (C::Down, "\x1b[B"),
        (C::Char('\r'), "\r"),
        (C::Char('\t'), "\t"),
        (C::Char(' '), " "),
        (C::Char('\u{8}'), "\x7f"),
        (C::Char('\u{1b}'), "\x1b"),
        (C::Char('\u{7f}'), "\x1b[3~"),
    ];
    for &(k, s) in want {
        assert_eq!(key(&t, k, NONE), s, "{k:?}");
    }
    for n in 1..=12 {
        assert!(!key(&t, C::F(n), NONE).is_empty(), "F{n}");
    }
}
