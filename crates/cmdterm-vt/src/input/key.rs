//! Keyboard encoding: legacy xterm, modifyOtherKeys and the kitty keyboard protocol.
//!
//! References: xterm's `ctlseqs` (cursor, keypad and function keys, `modifyOtherKeys`)
//! and <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.
//!
//! [`encode_key`] appends bytes to a caller buffer and does not allocate once the buffer
//! has grown. It returns whether it wrote anything.

use super::{Mods, push_char, push_dec};

/// A key on the keyboard.
///
/// `Char` carries the character the key produces, with Shift already applied (`'A'`,
/// not `'a'` plus Shift). The control characters `'\r'`, `'\t'`, `'\x08'`, `'\x1b'` and
/// `'\x7f'` are accepted and mean Enter, Tab, Backspace, Escape and Delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    /// Sends DEL (0x7f), as xterm does by default.
    Backspace,
    Escape,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    /// F1 to F35.
    F(u8),
    Keypad(KeypadKey),
    Menu,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Media(MediaKey),
    /// A modifier key on its own. Only the kitty protocol reports these.
    Modifier(ModifierKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeypadKey {
    /// 0 to 9.
    Digit(u8),
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Enter,
    Equal,
    Separator,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
    Begin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKey {
    Play,
    Pause,
    PlayPause,
    Reverse,
    Stop,
    FastForward,
    Rewind,
    TrackNext,
    TrackPrevious,
    Record,
    LowerVolume,
    RaiseVolume,
    MuteVolume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModifierKey {
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

impl ModifierKey {
    fn code(self) -> u32 {
        57441 + self as u32
    }

    fn bit(self) -> Mods {
        use ModifierKey::*;
        match self {
            LeftShift | RightShift => Mods::SHIFT,
            LeftControl | RightControl => Mods::CTRL,
            LeftAlt | RightAlt => Mods::ALT,
            LeftSuper | RightSuper => Mods::SUPER,
            LeftHyper | RightHyper => Mods::HYPER,
            LeftMeta | RightMeta => Mods::META,
            IsoLevel3Shift | IsoLevel5Shift => Mods::NONE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyEventKind {
    #[default]
    Press,
    Repeat,
    Release,
}

/// The terminal modes that change how keys are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyModes {
    /// DECCKM.
    pub app_cursor: bool,
    /// DECKPAM.
    pub app_keypad: bool,
    /// LNM: Enter sends CR LF.
    pub newline: bool,
    /// `modifyOtherKeys` level, 0 to 2.
    pub modify_other_keys: u8,
    /// Kitty keyboard protocol flags of the active screen.
    pub kitty_flags: u8,
}

/// Kitty flag bits.
pub mod kitty {
    pub const DISAMBIGUATE: u8 = 1;
    pub const EVENT_TYPES: u8 = 2;
    pub const ALTERNATE_KEYS: u8 = 4;
    pub const ALL_KEYS: u8 = 8;
    pub const TEXT: u8 = 16;
    pub const ALL: u8 = 31;
}

const ESC: u8 = 0x1b;

/// The kitty keyboard flag stack of one screen. At most [`KittyStack::DEPTH`] entries; a
/// push on a full stack drops the oldest. The bottom entry is the base value (0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KittyStack {
    entries: [u8; Self::DEPTH],
    len: usize,
}

impl KittyStack {
    /// The depth kitty itself uses.
    pub const DEPTH: usize = 8;

    pub const fn new() -> Self {
        Self {
            entries: [0; Self::DEPTH],
            len: 1,
        }
    }

    /// The flags in effect.
    pub fn flags(&self) -> u8 {
        self.entries[self.len - 1]
    }

    /// Replaces the flags in effect.
    pub fn set(&mut self, flags: u8) {
        self.entries[self.len - 1] = flags & kitty::ALL;
    }

    pub fn push(&mut self, flags: u8) {
        if self.len == Self::DEPTH {
            self.entries.copy_within(1.., 0);
            self.len -= 1;
        }
        self.entries[self.len] = flags & kitty::ALL;
        self.len += 1;
    }

    /// Removes `n` entries. Popping more than there are leaves the base value.
    pub fn pop(&mut self, n: usize) {
        if n >= self.len {
            self.len = 1;
            self.entries[0] = 0;
        } else {
            self.len -= n;
        }
    }
}

impl Default for KittyStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Encodes one key event.
///
/// `text` is the text the key produces, when the caller knows it. The kitty protocol
/// reports it when the "associated text" flag is on. For `Char` keys it defaults to the
/// character itself.
pub fn encode_key(
    modes: &KeyModes,
    key: KeyCode,
    mods: Mods,
    kind: KeyEventKind,
    text: Option<&str>,
    out: &mut Vec<u8>,
) -> bool {
    let start = out.len();
    let key = normalize(key);
    if let KeyCode::F(n) = key
        && !(1..=35).contains(&n)
    {
        return false;
    }
    if modes.kitty_flags & kitty::ALL != 0 {
        kitty_encode(modes, key, mods, kind, text, out);
    } else if kind != KeyEventKind::Release {
        legacy(modes, key, mods, out);
    }
    out.len() > start
}

fn normalize(key: KeyCode) -> KeyCode {
    match key {
        KeyCode::Char('\r') => KeyCode::Enter,
        KeyCode::Char('\t') => KeyCode::Tab,
        KeyCode::Char('\x08') => KeyCode::Backspace,
        KeyCode::Char('\x1b') => KeyCode::Escape,
        KeyCode::Char('\x7f') => KeyCode::Delete,
        k => k,
    }
}

// Legacy ---------------------------------------------------------------------------------

/// The xterm modifier parameter: 1 plus shift 1, alt 2, ctrl 4, super or meta 8.
fn xterm_param(mods: Mods) -> u32 {
    let m = mods.without_locks();
    let mut v = u32::from(m.bits() & 0x0f);
    if m.intersects(Mods::HYPER | Mods::META) {
        v |= 8;
    }
    v + 1
}

/// What Ctrl turns a character into, as xterm does.
pub fn ctrl_mapping(c: char) -> Option<u8> {
    Some(match c {
        '@' | '`' | ' ' | '2' => 0x00,
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '[' | '3' | '{' => 0x1b,
        '\\' | '4' | '|' => 0x1c,
        ']' | '5' | '}' => 0x1d,
        '^' | '6' | '~' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

fn csi27(out: &mut Vec<u8>, param: u32, code: u32) {
    out.extend_from_slice(b"\x1b[27;");
    push_dec(out, param);
    out.push(b';');
    push_dec(out, code);
    out.push(b'~');
}

/// `CSI letter`, `SS3 letter`, or `CSI 1 ; param letter`.
fn letter(out: &mut Vec<u8>, ss3: bool, param: u32, final_byte: u8) {
    if param > 1 {
        out.extend_from_slice(b"\x1b[1;");
        push_dec(out, param);
    } else {
        out.extend_from_slice(if ss3 { b"\x1bO" } else { b"\x1b[" });
    }
    out.push(final_byte);
}

/// `CSI n ~` or `CSI n ; param ~`.
fn tilde(out: &mut Vec<u8>, n: u32, param: u32) {
    out.extend_from_slice(b"\x1b[");
    push_dec(out, n);
    if param > 1 {
        out.push(b';');
        push_dec(out, param);
    }
    out.push(b'~');
}

fn fkey_tilde(n: u8) -> Option<u32> {
    Some(match n {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        13 => 25,
        14 => 26,
        15 => 28,
        16 => 29,
        17 => 31,
        18 => 32,
        19 => 33,
        20 => 34,
        21 => 42,
        22 => 43,
        23 => 44,
        24 => 45,
        _ => return None,
    })
}

/// Enter, Tab, Backspace and Escape with modifiers under `modifyOtherKeys`.
fn mok_applies_named(level: u8, key: KeyCode, mods: Mods) -> bool {
    let m = mods.without_locks();
    match level {
        0 => false,
        1 => {
            matches!(key, KeyCode::Enter | KeyCode::Tab)
                && m.intersects(Mods::SHIFT | Mods::CTRL | Mods::SUPER | Mods::HYPER | Mods::META)
        }
        _ => !m.is_empty(),
    }
}

fn legacy(modes: &KeyModes, key: KeyCode, mods: Mods, out: &mut Vec<u8>) {
    let m = mods.without_locks();
    let p = xterm_param(m);
    let alt = m.contains(Mods::ALT);
    let ctrl = m.contains(Mods::CTRL);
    let level = modes.modify_other_keys;
    let ss3 = modes.app_cursor;
    match key {
        KeyCode::Char(c) => {
            let others = m.without(Mods::SHIFT);
            let csi = match level {
                0 => false,
                1 => {
                    others.intersects(Mods::CTRL | Mods::SUPER | Mods::HYPER | Mods::META)
                        && !(ctrl && ctrl_mapping(c).is_some())
                }
                _ => !others.is_empty(),
            };
            if csi {
                csi27(out, p, c as u32);
                return;
            }
            if ctrl && let Some(b) = ctrl_mapping(c) {
                if alt {
                    out.push(ESC);
                }
                out.push(b);
                return;
            }
            if alt {
                out.push(ESC);
            }
            push_char(out, c);
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Escape
            if mok_applies_named(level, key, m) =>
        {
            let code = match key {
                KeyCode::Enter => 13,
                KeyCode::Tab => 9,
                KeyCode::Backspace => 127,
                _ => 27,
            };
            csi27(out, p, code);
        }
        KeyCode::Enter => {
            if alt {
                out.push(ESC);
            }
            out.push(b'\r');
            if modes.newline {
                out.push(b'\n');
            }
        }
        KeyCode::Tab => {
            if alt {
                out.push(ESC);
            }
            let rest = m.without(Mods::ALT);
            if rest == Mods::SHIFT {
                out.extend_from_slice(b"\x1b[Z");
            } else if rest.contains(Mods::SHIFT) {
                letter(out, false, xterm_param(rest), b'Z');
            } else {
                out.push(b'\t');
            }
        }
        KeyCode::Backspace => {
            if alt {
                out.push(ESC);
            }
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Escape => {
            if alt {
                out.push(ESC);
            }
            out.push(ESC);
        }
        KeyCode::Delete => tilde(out, 3, p),
        KeyCode::Insert => tilde(out, 2, p),
        KeyCode::PageUp => tilde(out, 5, p),
        KeyCode::PageDown => tilde(out, 6, p),
        KeyCode::Up => letter(out, ss3, p, b'A'),
        KeyCode::Down => letter(out, ss3, p, b'B'),
        KeyCode::Right => letter(out, ss3, p, b'C'),
        KeyCode::Left => letter(out, ss3, p, b'D'),
        KeyCode::Home => letter(out, ss3, p, b'H'),
        KeyCode::End => letter(out, ss3, p, b'F'),
        KeyCode::F(n @ 1..=4) => letter(out, true, p, b'P' + n - 1),
        KeyCode::F(n) => {
            if let Some(code) = fkey_tilde(n) {
                tilde(out, code, p);
            }
        }
        KeyCode::Keypad(k) => legacy_keypad(modes, k, m, out),
        _ => {}
    }
}

fn legacy_keypad(modes: &KeyModes, k: KeypadKey, m: Mods, out: &mut Vec<u8>) {
    use KeypadKey::*;
    let p = xterm_param(m);
    let ss3 = modes.app_cursor;
    match k {
        Left => return letter(out, ss3, p, b'D'),
        Right => return letter(out, ss3, p, b'C'),
        Up => return letter(out, ss3, p, b'A'),
        Down => return letter(out, ss3, p, b'B'),
        Home => return letter(out, ss3, p, b'H'),
        End => return letter(out, ss3, p, b'F'),
        Begin => return letter(out, ss3, p, b'E'),
        PageUp => return tilde(out, 5, p),
        PageDown => return tilde(out, 6, p),
        Insert => return tilde(out, 2, p),
        Delete => return tilde(out, 3, p),
        _ => {}
    }
    if modes.app_keypad {
        let f = match k {
            Digit(n) => b'p' + n.min(9),
            Decimal => b'n',
            Divide => b'o',
            Multiply => b'j',
            Add => b'k',
            Separator => b'l',
            Subtract => b'm',
            Enter => b'M',
            Equal => b'X',
            _ => return,
        };
        out.extend_from_slice(b"\x1bO");
        if p > 1 {
            push_dec(out, p);
        }
        out.push(f);
        return;
    }
    if m.contains(Mods::ALT) {
        out.push(ESC);
    }
    match k {
        Digit(n) => out.push(b'0' + n.min(9)),
        Decimal => out.push(b'.'),
        Divide => out.push(b'/'),
        Multiply => out.push(b'*'),
        Add => out.push(b'+'),
        Separator => out.push(b','),
        Subtract => out.push(b'-'),
        Equal => out.push(b'='),
        Enter => {
            out.push(b'\r');
            if modes.newline {
                out.push(b'\n');
            }
        }
        _ => {}
    }
}

// Kitty ----------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Form {
    /// `CSI 1 ; mods letter`.
    Letter(u8),
    /// `CSI n ; mods ~`.
    Tilde(u32),
    /// `CSI code ; mods u`.
    U(u32),
}

fn form(key: KeyCode) -> Option<Form> {
    use Form::*;
    Some(match key {
        KeyCode::Up => Letter(b'A'),
        KeyCode::Down => Letter(b'B'),
        KeyCode::Right => Letter(b'C'),
        KeyCode::Left => Letter(b'D'),
        KeyCode::Home => Letter(b'H'),
        KeyCode::End => Letter(b'F'),
        KeyCode::Insert => Tilde(2),
        KeyCode::Delete => Tilde(3),
        KeyCode::PageUp => Tilde(5),
        KeyCode::PageDown => Tilde(6),
        KeyCode::F(1) => Letter(b'P'),
        KeyCode::F(2) => Letter(b'Q'),
        KeyCode::F(3) => Tilde(13),
        KeyCode::F(4) => Letter(b'S'),
        KeyCode::F(n @ 5..=12) => Tilde(fkey_tilde(n)?),
        KeyCode::F(n @ 13..=35) => U(57376 + u32::from(n) - 13),
        KeyCode::Enter => U(13),
        KeyCode::Tab => U(9),
        KeyCode::Backspace => U(127),
        KeyCode::Escape => U(27),
        KeyCode::CapsLock => U(57358),
        KeyCode::ScrollLock => U(57359),
        KeyCode::NumLock => U(57360),
        KeyCode::PrintScreen => U(57361),
        KeyCode::Pause => U(57362),
        KeyCode::Menu => U(57363),
        KeyCode::Keypad(k) => U(keypad_code(k)),
        KeyCode::Media(k) => U(57428 + k as u32),
        KeyCode::Modifier(k) => U(k.code()),
        KeyCode::Char(c) => U(unshifted(c, Mods::NONE).0 as u32),
        KeyCode::F(_) => return None,
    })
}

fn keypad_code(k: KeypadKey) -> u32 {
    use KeypadKey::*;
    match k {
        Digit(n) => 57399 + u32::from(n.min(9)),
        Decimal => 57409,
        Divide => 57410,
        Multiply => 57411,
        Subtract => 57412,
        Add => 57413,
        Enter => 57414,
        Equal => 57415,
        Separator => 57416,
        Left => 57417,
        Right => 57418,
        Up => 57419,
        Down => 57420,
        PageUp => 57421,
        PageDown => 57422,
        Home => 57423,
        End => 57424,
        Insert => 57425,
        Delete => 57426,
        Begin => 57427,
    }
}

/// The key a character is on without Shift, and the character Shift gives, on a US layout.
fn unshifted(c: char, mods: Mods) -> (char, Option<char>) {
    let shift = mods.contains(Mods::SHIFT);
    if c.is_uppercase() {
        let mut lower = c.to_lowercase();
        if let (Some(l), None) = (lower.next(), lower.next()) {
            return (l, shift.then_some(c));
        }
    } else if shift && c.is_lowercase() {
        let mut upper = c.to_uppercase();
        if let (Some(u), None) = (upper.next(), upper.next()) {
            return (c, Some(u));
        }
    }
    if shift {
        let base = match c {
            '!' => '1',
            '@' => '2',
            '#' => '3',
            '$' => '4',
            '%' => '5',
            '^' => '6',
            '&' => '7',
            '*' => '8',
            '(' => '9',
            ')' => '0',
            '_' => '-',
            '+' => '=',
            '{' => '[',
            '}' => ']',
            '|' => '\\',
            ':' => ';',
            '"' => '\'',
            '<' => ',',
            '>' => '.',
            '?' => '/',
            '~' => '`',
            _ => return (c, None),
        };
        return (base, Some(c));
    }
    (c, None)
}

fn is_text_keypad(k: KeypadKey) -> bool {
    use KeypadKey::*;
    matches!(
        k,
        Digit(_) | Decimal | Divide | Multiply | Subtract | Add | Equal | Separator | Enter
    )
}

fn kitty_encode(
    modes: &KeyModes,
    key: KeyCode,
    mods: Mods,
    kind: KeyEventKind,
    text: Option<&str>,
    out: &mut Vec<u8>,
) {
    use kitty::*;
    let f = modes.kitty_flags;
    let all = f & ALL_KEYS != 0;
    let disambiguate = f & (DISAMBIGUATE | ALL_KEYS) != 0;
    let kind = if f & EVENT_TYPES == 0 {
        if kind == KeyEventKind::Release {
            return;
        }
        KeyEventKind::Press
    } else {
        kind
    };

    let mut mods = mods;
    if let KeyCode::Modifier(k) = key {
        if !all {
            return;
        }
        if kind == KeyEventKind::Release {
            mods = mods.without(k.bit());
        } else {
            mods |= k.bit();
        }
    }
    let core = mods.without_locks();

    if !all {
        match key {
            KeyCode::Char(c) => {
                if core.without(Mods::SHIFT).is_empty() {
                    if kind != KeyEventKind::Release {
                        match text {
                            Some(t) if !t.is_empty() => out.extend_from_slice(t.as_bytes()),
                            _ => push_char(out, c),
                        }
                    }
                    return;
                }
                if !disambiguate {
                    if kind != KeyEventKind::Release {
                        legacy(modes, key, mods, out);
                    }
                    return;
                }
            }
            KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace => {
                if kind == KeyEventKind::Release {
                    return;
                }
                if core.is_empty() || !disambiguate {
                    legacy(modes, key, mods, out);
                    return;
                }
            }
            KeyCode::Escape => {
                if !disambiguate {
                    if kind != KeyEventKind::Release {
                        legacy(modes, key, mods, out);
                    }
                    return;
                }
            }
            KeyCode::Keypad(k) if is_text_keypad(k) => {
                // Kitty spec, "Disambiguate escape codes": with flag 1 all keypad keys
                // are reported as separate keys with CSI u and their own numbers.
                if !disambiguate {
                    if kind != KeyEventKind::Release {
                        legacy(modes, key, mods, out);
                    }
                    return;
                }
            }
            KeyCode::Modifier(_) => return,
            _ => {
                // Functional keys: legacy form while it is unambiguous.
                let legacy_able = matches!(
                    key,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Home
                        | KeyCode::End
                        | KeyCode::Insert
                        | KeyCode::Delete
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                        | KeyCode::F(1..=12)
                );
                if legacy_able {
                    if core.is_empty() && kind == KeyEventKind::Press {
                        legacy(modes, key, mods, out);
                        return;
                    }
                } else if !disambiguate {
                    // Without disambiguate, keys keep their legacy encoding.
                    if kind != KeyEventKind::Release {
                        legacy(modes, key, mods, out);
                    }
                    return;
                }
            }
        }
    }

    // Escape code.
    let Some(form) = form(key) else { return };
    let param = 1 + u32::from(mods.bits());
    let want_event = kind != KeyEventKind::Press;
    let event = match kind {
        KeyEventKind::Press => 1,
        KeyEventKind::Repeat => 2,
        KeyEventKind::Release => 3,
    };
    let mods_field = |out: &mut Vec<u8>| {
        if param > 1 || want_event {
            push_dec(out, param);
        }
        if want_event {
            out.push(b':');
            push_dec(out, event);
        }
    };
    match form {
        Form::Letter(b) => {
            out.extend_from_slice(b"\x1b[");
            if param > 1 || want_event {
                out.extend_from_slice(b"1;");
                mods_field(out);
            }
            out.push(b);
        }
        Form::Tilde(n) => {
            out.extend_from_slice(b"\x1b[");
            push_dec(out, n);
            if param > 1 || want_event {
                out.push(b';');
                mods_field(out);
            }
            out.push(b'~');
        }
        Form::U(code) => {
            out.extend_from_slice(b"\x1b[");
            let mut shifted = None;
            if let KeyCode::Char(c) = key {
                let (base, sh) = unshifted(c, core);
                push_dec(out, base as u32);
                shifted = sh;
            } else {
                push_dec(out, code);
            }
            if f & ALTERNATE_KEYS != 0
                && let Some(s) = shifted
            {
                out.push(b':');
                push_dec(out, s as u32);
            }
            let want_text = all && f & TEXT != 0 && kind != KeyEventKind::Release;
            let text_value: Option<&str> = match (key, text) {
                (_, Some(t)) if !t.is_empty() => Some(t),
                _ => None,
            };
            let mut one = [0u8; 4];
            let text_value = match (text_value, key) {
                (Some(t), _) => Some(t),
                (None, KeyCode::Char(c))
                    if core.without(Mods::SHIFT).is_empty() && !c.is_control() =>
                {
                    Some(&*c.encode_utf8(&mut one))
                }
                _ => None,
            };
            // Control characters never go in the text field; with none left it is omitted.
            let text_value = text_value.filter(|t| want_text && t.chars().any(|c| !c.is_control()));
            if param > 1 || want_event || text_value.is_some() {
                out.push(b';');
                mods_field(out);
            }
            if let Some(t) = text_value {
                out.push(b';');
                for (i, ch) in t.chars().filter(|c| !c.is_control()).enumerate() {
                    if i > 0 {
                        out.push(b':');
                    }
                    push_dec(out, ch as u32);
                }
            }
            out.push(b'u');
        }
    }
}
