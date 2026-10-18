//! Byte-level VT parser: the DEC ANSI state machine (Paul Williams' model) with colon
//! subparameters and UTF-8 decoding.
//!
//! # Allocation
//!
//! The parser allocates nothing per byte or per sequence. Parameters live in a fixed
//! array of [`MAX_PARAMS`] values (extra ones are dropped and flagged), intermediates in
//! an array of [`MAX_INTERMEDIATES`] bytes (extra ones set the `ignore` flag), and OSC
//! payloads in one `Vec` that the parser keeps between sequences. That buffer holds at
//! most [`OSC_MAX_LEN`] bytes; longer payloads are truncated and flagged, never grown
//! further. Its capacity only grows to the largest payload seen so far.
//!
//! # Input handling
//!
//! * Ground state decodes UTF-8. Invalid input prints U+FFFD (see [`crate::utf8`]).
//! * C1 controls are recognized only as the characters U+0080 to U+009F, which is how
//!   UTF-8 encodes them. A raw 0x80 to 0x9F byte that continues a multi-byte character
//!   is never a control. Inside OSC, DCS passthrough and SOS/PM/APC strings the bytes are
//!   raw, so only BEL (OSC), ESC, CAN and SUB end a string there.
//! * Inside strings, the UTF-8 pair C2 9C is also ST (U+009C), as in ground state. A raw
//!   0x9C byte and other pairs such as C3 9C stay payload. The parser keeps one pending
//!   flag for a C2 so the pair may be split across `advance` calls.
//! * OSC payloads are handed to [`Perform::osc_dispatch`] whole and split lazily with
//!   [`osc_split`], so dispatch never allocates.
//! * Outside strings, characters at or above U+00A0 are dropped when a sequence is in
//!   progress.
//! * CAN and SUB abort the current sequence. Outside strings they are also passed to
//!   [`Perform::execute`]; inside strings they end it silently, and an unfinished DCS is
//!   unhooked.
//! * ESC ends a string. An OSC is dispatched first, a DCS unhooked, and the ESC then
//!   starts a new escape sequence, so `ESC \` reaches [`Perform::esc_dispatch`] with
//!   `\`, which performers should ignore.
//! * 0x7F is ignored everywhere.

use crate::utf8::{Step, Utf8Decoder};

/// Most parameter values kept per sequence. Later ones are dropped and
/// [`Params::overflowed`] reports it.
pub const MAX_PARAMS: usize = 32;
/// Most intermediate bytes kept per sequence. More set the `ignore` flag.
pub const MAX_INTERMEDIATES: usize = 2;
/// Largest OSC payload kept, in bytes. Longer payloads are truncated.
pub const OSC_MAX_LEN: usize = 1 << 20;
const OSC_INITIAL_CAPACITY: usize = 256;
const PARAM_MAX_VALUE: u32 = u16::MAX as u32;

/// How an OSC string ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OscEnd {
    /// The payload exceeded [`OSC_MAX_LEN`] and was cut.
    pub truncated: bool,
    /// BEL ended the string (as opposed to ESC \). Replies should end the same way.
    pub bell: bool,
}

/// Numeric parameters of a CSI or DCS sequence.
///
/// Values are stored flat. A value that follows a colon belongs to the group of the value
/// before it, so `38:2::1:2:3;4` is two groups, `[38, 2, 0, 1, 2, 3]` and `[4]`. Empty
/// values are 0. Values saturate at 65535.
#[derive(Clone)]
pub struct Params {
    values: [u16; MAX_PARAMS],
    sub: u32,
    len: usize,
    cur: u32,
    started: bool,
    next_sub: bool,
    overflow: bool,
}

impl Params {
    const fn new() -> Self {
        Self {
            values: [0; MAX_PARAMS],
            sub: 0,
            len: 0,
            cur: 0,
            started: false,
            next_sub: false,
            overflow: false,
        }
    }

    fn clear(&mut self) {
        self.sub = 0;
        self.len = 0;
        self.cur = 0;
        self.started = false;
        self.next_sub = false;
        self.overflow = false;
    }

    fn push(&mut self) {
        if self.len < MAX_PARAMS {
            self.values[self.len] = self.cur as u16;
            if self.next_sub {
                self.sub |= 1 << self.len;
            }
            self.len += 1;
        } else {
            self.overflow = true;
        }
        self.cur = 0;
    }

    fn digit(&mut self, d: u8) {
        self.started = true;
        self.cur = (self.cur * 10 + u32::from(d)).min(PARAM_MAX_VALUE);
    }

    fn separator(&mut self, colon: bool) {
        self.started = true;
        self.push();
        self.next_sub = colon;
    }

    fn finish(&mut self) {
        if self.started {
            self.push();
            self.started = false;
        }
    }

    /// All values, flat, subparameters included.
    pub fn values(&self) -> &[u16] {
        &self.values[..self.len]
    }

    /// Number of values, subparameters included.
    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True when more than [`MAX_PARAMS`] values arrived and the rest were dropped.
    pub const fn overflowed(&self) -> bool {
        self.overflow
    }

    /// The groups: each is a main value followed by its colon subparameters.
    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter {
            params: self,
            pos: 0,
        }
    }
}

impl std::fmt::Debug for Params {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> IntoIterator for &'a Params {
    type Item = &'a [u16];
    type IntoIter = ParamsIter<'a>;
    fn into_iter(self) -> ParamsIter<'a> {
        self.iter()
    }
}

/// Iterator over the parameter groups.
pub struct ParamsIter<'a> {
    params: &'a Params,
    pos: usize,
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];
    fn next(&mut self) -> Option<&'a [u16]> {
        let p = self.params;
        if self.pos >= p.len {
            return None;
        }
        let start = self.pos;
        let mut end = start + 1;
        while end < p.len && p.sub & (1 << end) != 0 {
            end += 1;
        }
        self.pos = end;
        Some(&p.values[start..end])
    }
}

/// Receives what the parser recognizes. Every method defaults to doing nothing.
pub trait Perform {
    /// A printable character.
    fn print(&mut self, _c: char) {}
    /// A C0 control, or a C1 control as its value 0x80 to 0x9F.
    fn execute(&mut self, _byte: u8) {}
    /// A complete CSI sequence. `private` is the `<`, `=`, `>` or `?` marker that came
    /// first, if any. `ignore` is set when more than [`MAX_INTERMEDIATES`] intermediates
    /// arrived; the sequence should then be dropped.
    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _private: Option<u8>,
        _ignore: bool,
        _action: u8,
    ) {
    }
    /// A complete ESC sequence.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
    /// A complete OSC string. `payload` is the raw text after `ESC ]`; use [`osc_split`]
    /// to cut it at `;`.
    fn osc_dispatch(&mut self, _payload: &[u8], _end: OscEnd) {}
    /// A DCS header was parsed; `put` calls follow, then `unhook`.
    fn hook(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _private: Option<u8>,
        _ignore: bool,
        _action: u8,
    ) {
    }
    /// One byte of DCS data.
    fn put(&mut self, _byte: u8) {}
    /// The DCS string ended, normally or by abort.
    fn unhook(&mut self) {}
}

/// Splits an OSC payload at `;`.
pub fn osc_split(payload: &[u8]) -> impl Iterator<Item = &[u8]> {
    payload.split(|&b| b == b';')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsIgnore,
    DcsPassthrough,
    OscString,
    SosPmApc,
}

impl State {
    /// States whose bytes are raw payload and bypass the UTF-8 decoder.
    const fn is_string(self) -> bool {
        matches!(
            self,
            State::DcsIgnore | State::DcsPassthrough | State::OscString | State::SosPmApc
        )
    }
}

/// The parser. Feed it bytes with [`Parser::advance`]; it calls a [`Perform`].
pub struct Parser {
    state: State,
    utf8: Utf8Decoder,
    params: Params,
    intermediates: [u8; MAX_INTERMEDIATES],
    inter_len: usize,
    ignore: bool,
    private: Option<u8>,
    osc: Vec<u8>,
    osc_truncated: bool,
    /// A C2 byte was seen in a string state and may start the UTF-8 form of ST.
    pending_c2: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            utf8: Utf8Decoder::new(),
            params: Params::new(),
            intermediates: [0; MAX_INTERMEDIATES],
            inter_len: 0,
            ignore: false,
            private: None,
            osc: Vec::with_capacity(OSC_INITIAL_CAPACITY),
            osc_truncated: false,
            pending_c2: false,
        }
    }

    /// Capacity of the OSC buffer, for tests that check it stays stable.
    pub fn osc_capacity(&self) -> usize {
        self.osc.capacity()
    }

    /// Drops any sequence in progress and returns to ground state.
    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.utf8.reset();
        self.clear();
        self.osc.clear();
        self.osc_truncated = false;
        self.pending_c2 = false;
    }

    /// Parses `bytes`. Input may be split anywhere; the result is the same as for the
    /// whole.
    pub fn advance<P: Perform>(&mut self, p: &mut P, bytes: &[u8]) {
        for &b in bytes {
            if self.state.is_string() {
                self.string_byte(p, b);
            } else if b < 0x80 && !self.utf8.pending() {
                self.ascii(p, b);
            } else {
                self.decode(p, b);
            }
        }
    }

    fn clear(&mut self) {
        self.params.clear();
        self.inter_len = 0;
        self.ignore = false;
        self.private = None;
    }

    fn collect(&mut self, b: u8) {
        if self.inter_len < MAX_INTERMEDIATES {
            self.intermediates[self.inter_len] = b;
            self.inter_len += 1;
        } else {
            self.ignore = true;
        }
    }

    fn enter_escape(&mut self) {
        self.clear();
        self.state = State::Escape;
    }

    fn enter_csi(&mut self) {
        self.clear();
        self.state = State::CsiEntry;
    }

    fn enter_dcs(&mut self) {
        self.clear();
        self.state = State::DcsEntry;
    }

    fn enter_osc(&mut self) {
        self.osc.clear();
        self.osc_truncated = false;
        self.state = State::OscString;
    }

    fn decode<P: Perform>(&mut self, p: &mut P, b: u8) {
        loop {
            match self.utf8.feed(b) {
                Step::Pending => return,
                Step::Char(c) => {
                    self.character(p, c);
                    return;
                }
                Step::Invalid => {
                    self.invalid(p);
                    return;
                }
                Step::InvalidRetry => self.invalid(p),
            }
        }
    }

    fn invalid<P: Perform>(&mut self, p: &mut P) {
        if self.state == State::Ground {
            p.print('\u{FFFD}');
        }
    }

    fn character<P: Perform>(&mut self, p: &mut P, c: char) {
        let u = c as u32;
        if u < 0x80 {
            self.ascii(p, u as u8);
        } else if u < 0xA0 {
            self.c1(p, u as u8);
        } else if self.state == State::Ground {
            p.print(c);
        }
    }

    fn c1<P: Perform>(&mut self, p: &mut P, b: u8) {
        match b {
            0x90 => self.enter_dcs(),
            0x9B => self.enter_csi(),
            0x9D => self.enter_osc(),
            0x98 | 0x9E | 0x9F => self.state = State::SosPmApc,
            0x9C => self.state = State::Ground,
            _ => {
                p.execute(b);
                self.state = State::Ground;
            }
        }
    }

    fn string_byte<P: Perform>(&mut self, p: &mut P, b: u8) {
        if self.pending_c2 {
            self.pending_c2 = false;
            if b == 0x9C {
                match self.state {
                    State::OscString => self.osc_end(p, false, State::Ground),
                    State::DcsPassthrough => {
                        p.unhook();
                        self.state = State::Ground;
                    }
                    _ => self.state = State::Ground,
                }
                return;
            }
            self.string_payload(p, 0xC2);
        }
        if b == 0xC2 {
            self.pending_c2 = true;
            return;
        }
        self.string_payload(p, b);
    }

    fn string_payload<P: Perform>(&mut self, p: &mut P, b: u8) {
        match (self.state, b) {
            (State::OscString, 0x07) => self.osc_end(p, true, State::Ground),
            (State::OscString, 0x1B) => {
                self.osc_end(p, false, State::Ground);
                self.enter_escape();
            }
            (State::DcsPassthrough, 0x1B) => {
                p.unhook();
                self.enter_escape();
            }
            (_, 0x1B) => self.enter_escape(),
            (State::DcsPassthrough, 0x18 | 0x1A) => {
                p.unhook();
                self.state = State::Ground;
            }
            (_, 0x18 | 0x1A) => self.state = State::Ground,
            (State::OscString, 0x00..=0x1F) => {}
            (State::OscString, _) => {
                if self.osc.len() < OSC_MAX_LEN {
                    self.osc.push(b);
                } else {
                    self.osc_truncated = true;
                }
            }
            (State::DcsPassthrough, 0x7F) => {}
            (State::DcsPassthrough, _) => p.put(b),
            _ => {}
        }
    }

    fn osc_end<P: Perform>(&mut self, p: &mut P, bell: bool, next: State) {
        let end = OscEnd {
            truncated: self.osc_truncated,
            bell,
        };
        p.osc_dispatch(&self.osc, end);
        self.osc_truncated = false;
        self.state = next;
    }

    /// One byte below 0x80 in a non-string state.
    fn ascii<P: Perform>(&mut self, p: &mut P, b: u8) {
        match b {
            0x18 | 0x1A => {
                p.execute(b);
                self.state = State::Ground;
                return;
            }
            0x1B => {
                self.enter_escape();
                return;
            }
            _ => {}
        }
        match self.state {
            State::Ground => match b {
                0x20..=0x7E => p.print(b as char),
                0x7F => {}
                _ => p.execute(b),
            },
            State::Escape => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x2F => {
                    self.collect(b);
                    self.state = State::EscapeIntermediate;
                }
                0x50 => self.enter_dcs(),
                0x5B => self.enter_csi(),
                0x5D => self.enter_osc(),
                0x58 | 0x5E | 0x5F => self.state = State::SosPmApc,
                0x7F => {}
                _ => self.esc_dispatch(p, b),
            },
            State::EscapeIntermediate => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x2F => self.collect(b),
                0x7F => {}
                _ => self.esc_dispatch(p, b),
            },
            State::CsiEntry | State::CsiParam | State::CsiIntermediate | State::CsiIgnore => {
                self.seq_byte(p, b, false);
            }
            State::DcsEntry | State::DcsParam | State::DcsIntermediate => {
                self.seq_byte(p, b, true);
            }
            State::DcsIgnore | State::DcsPassthrough | State::OscString | State::SosPmApc => {}
        }
    }

    fn esc_dispatch<P: Perform>(&mut self, p: &mut P, b: u8) {
        p.esc_dispatch(&self.intermediates[..self.inter_len], self.ignore, b);
        self.state = State::Ground;
    }

    /// Shared handling for CSI and DCS headers, which differ only in what a control
    /// byte and the final byte do.
    fn seq_byte<P: Perform>(&mut self, p: &mut P, b: u8, dcs: bool) {
        use State::*;
        let (entry, param, inter) = if dcs {
            (DcsEntry, DcsParam, DcsIntermediate)
        } else {
            (CsiEntry, CsiParam, CsiIntermediate)
        };
        let state = self.state;
        match b {
            0x00..=0x1F => {
                if !dcs {
                    p.execute(b);
                }
            }
            0x7F => {}
            0x20..=0x2F => {
                if state != CsiIgnore {
                    self.collect(b);
                    self.state = inter;
                }
            }
            0x30..=0x3B => {
                if state == entry || state == param {
                    match b {
                        0x3A => self.params.separator(true),
                        0x3B => self.params.separator(false),
                        _ => self.params.digit(b - b'0'),
                    }
                    self.state = param;
                } else if state == inter {
                    self.state = if dcs { DcsIgnore } else { CsiIgnore };
                }
            }
            0x3C..=0x3F => {
                if state == entry {
                    self.private = Some(b);
                    self.state = param;
                } else if state != CsiIgnore {
                    self.state = if dcs { DcsIgnore } else { CsiIgnore };
                }
            }
            _ => {
                if state == CsiIgnore {
                    self.state = Ground;
                    return;
                }
                self.params.finish();
                let inter = &self.intermediates[..self.inter_len];
                if dcs {
                    p.hook(&self.params, inter, self.private, self.ignore, b);
                    self.state = DcsPassthrough;
                } else {
                    p.csi_dispatch(&self.params, inter, self.private, self.ignore, b);
                    self.state = Ground;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Action {
        Print(char),
        Execute(u8),
        Csi {
            params: Vec<Vec<u16>>,
            inter: Vec<u8>,
            private: Option<u8>,
            ignore: bool,
            overflow: bool,
            action: u8,
        },
        Esc {
            inter: Vec<u8>,
            ignore: bool,
            byte: u8,
        },
        Osc {
            payload: Vec<u8>,
            truncated: bool,
            bell: bool,
        },
        Hook {
            params: Vec<Vec<u16>>,
            inter: Vec<u8>,
            private: Option<u8>,
            ignore: bool,
            action: u8,
        },
        Put(u8),
        Unhook,
    }

    #[derive(Default)]
    struct Rec(Vec<Action>);

    fn groups(p: &Params) -> Vec<Vec<u16>> {
        p.iter().map(<[u16]>::to_vec).collect()
    }

    impl Perform for Rec {
        fn print(&mut self, c: char) {
            self.0.push(Action::Print(c));
        }
        fn execute(&mut self, b: u8) {
            self.0.push(Action::Execute(b));
        }
        fn csi_dispatch(
            &mut self,
            p: &Params,
            i: &[u8],
            private: Option<u8>,
            ignore: bool,
            action: u8,
        ) {
            self.0.push(Action::Csi {
                params: groups(p),
                inter: i.to_vec(),
                private,
                ignore,
                overflow: p.overflowed(),
                action,
            });
        }
        fn esc_dispatch(&mut self, i: &[u8], ignore: bool, byte: u8) {
            self.0.push(Action::Esc {
                inter: i.to_vec(),
                ignore,
                byte,
            });
        }
        fn osc_dispatch(&mut self, payload: &[u8], end: OscEnd) {
            self.0.push(Action::Osc {
                payload: payload.to_vec(),
                truncated: end.truncated,
                bell: end.bell,
            });
        }
        fn hook(&mut self, p: &Params, i: &[u8], private: Option<u8>, ignore: bool, action: u8) {
            self.0.push(Action::Hook {
                params: groups(p),
                inter: i.to_vec(),
                private,
                ignore,
                action,
            });
        }
        fn put(&mut self, b: u8) {
            self.0.push(Action::Put(b));
        }
        fn unhook(&mut self) {
            self.0.push(Action::Unhook);
        }
    }

    fn run(chunks: &[&[u8]]) -> Vec<Action> {
        let mut parser = Parser::new();
        let mut rec = Rec::default();
        for c in chunks {
            parser.advance(&mut rec, c);
        }
        rec.0
    }

    fn csi(params: &[&[u16]], action: u8) -> Action {
        Action::Csi {
            params: params.iter().map(|g| g.to_vec()).collect(),
            inter: vec![],
            private: None,
            ignore: false,
            overflow: false,
            action,
        }
    }

    fn print_all(s: &str) -> Vec<Action> {
        s.chars().map(Action::Print).collect()
    }

    #[test]
    fn ground_print_and_execute() {
        let mut want = print_all("ab");
        want.push(Action::Execute(b'\n'));
        want.push(Action::Execute(b'\r'));
        want.extend(print_all("é"));
        assert_eq!(run(&[b"ab\n\r\xc3\xa9"]), want);
        // DEL is ignored.
        assert_eq!(run(&[b"a\x7fb"]), print_all("ab"));
    }

    #[test]
    fn escape_dispatch_and_intermediate() {
        assert_eq!(
            run(&[b"\x1b7"]),
            vec![Action::Esc {
                inter: vec![],
                ignore: false,
                byte: b'7'
            }]
        );
        assert_eq!(
            run(&[b"\x1b(0"]),
            vec![Action::Esc {
                inter: vec![b'('],
                ignore: false,
                byte: b'0'
            }]
        );
        // Too many intermediates set ignore.
        assert_eq!(
            run(&[b"\x1b   A"]),
            vec![Action::Esc {
                inter: vec![b' ', b' '],
                ignore: true,
                byte: b'A'
            }]
        );
        // Controls execute inside an escape sequence without ending it.
        assert_eq!(
            run(&[b"\x1b\n7"]),
            vec![
                Action::Execute(b'\n'),
                Action::Esc {
                    inter: vec![],
                    ignore: false,
                    byte: b'7'
                }
            ]
        );
        // A second ESC restarts the sequence.
        assert_eq!(
            run(&[b"\x1b\x1b7"]),
            vec![Action::Esc {
                inter: vec![],
                ignore: false,
                byte: b'7'
            }]
        );
    }

    #[test]
    fn csi_basic_params() {
        assert_eq!(run(&[b"\x1b[m"]), vec![csi(&[], b'm')]);
        assert_eq!(run(&[b"\x1b[1;31m"]), vec![csi(&[&[1], &[31]], b'm')]);
        assert_eq!(run(&[b"\x1b[;m"]), vec![csi(&[&[0], &[0]], b'm')]);
        assert_eq!(run(&[b"\x1b[1;m"]), vec![csi(&[&[1], &[0]], b'm')]);
        assert_eq!(run(&[b"\x1b[99999999H"]), vec![csi(&[&[65535]], b'H')]);
    }

    #[test]
    fn csi_private_and_intermediates() {
        let got = run(&[b"\x1b[?1049h"]);
        assert_eq!(
            got,
            vec![Action::Csi {
                params: vec![vec![1049]],
                inter: vec![],
                private: Some(b'?'),
                ignore: false,
                overflow: false,
                action: b'h'
            }]
        );
        let got = run(&[b"\x1b[0 q"]);
        assert_eq!(
            got,
            vec![Action::Csi {
                params: vec![vec![0]],
                inter: vec![b' '],
                private: None,
                ignore: false,
                overflow: false,
                action: b'q'
            }]
        );
        // Too many intermediates set ignore.
        match &run(&[b"\x1b[ !\"p"])[0] {
            Action::Csi { ignore, inter, .. } => {
                assert!(ignore);
                assert_eq!(inter.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn csi_ignore_state() {
        // A private marker after a parameter poisons the sequence; nothing dispatches.
        assert_eq!(run(&[b"\x1b[1?mA"]), print_all("A"));
        // A parameter byte after an intermediate does too.
        assert_eq!(run(&[b"\x1b[ 1mA"]), print_all("A"));
        // Controls still execute inside the ignored sequence.
        assert_eq!(
            run(&[b"\x1b[1?\nmA"]),
            vec![Action::Execute(b'\n'), Action::Print('A')]
        );
        // A private marker after another one is also ignored.
        assert_eq!(run(&[b"\x1b[?>1hA"]), print_all("A"));
    }

    #[test]
    fn csi_control_inside_executes_and_continues() {
        assert_eq!(
            run(&[b"\x1b[1\n;2m"]),
            vec![Action::Execute(b'\n'), csi(&[&[1], &[2]], b'm')]
        );
    }

    #[test]
    fn colon_subparameters() {
        assert_eq!(run(&[b"\x1b[4:3m"]), vec![csi(&[&[4, 3]], b'm')]);
        assert_eq!(
            run(&[b"\x1b[38:2::10:20:30;1m"]),
            vec![csi(&[&[38, 2, 0, 10, 20, 30], &[1]], b'm')]
        );
        assert_eq!(run(&[b"\x1b[:1m"]), vec![csi(&[&[0, 1]], b'm')]);
        assert_eq!(run(&[b"\x1b[1:m"]), vec![csi(&[&[1, 0]], b'm')]);
    }

    #[test]
    fn max_params_overflow() {
        let mut input = b"\x1b[".to_vec();
        for i in 0..(MAX_PARAMS + 5) {
            if i > 0 {
                input.push(b';');
            }
            input.extend_from_slice(i.to_string().as_bytes());
        }
        input.push(b'm');
        match &run(&[&input])[0] {
            Action::Csi {
                params, overflow, ..
            } => {
                assert!(overflow);
                assert_eq!(params.len(), MAX_PARAMS);
                assert_eq!(params[MAX_PARAMS - 1], vec![(MAX_PARAMS - 1) as u16]);
            }
            other => panic!("{other:?}"),
        }
        // Exactly MAX_PARAMS does not overflow.
        let mut input = b"\x1b[".to_vec();
        input.extend(std::iter::repeat_n(*b"1;", MAX_PARAMS - 1).flatten());
        input.extend_from_slice(b"1m");
        match &run(&[&input])[0] {
            Action::Csi {
                params, overflow, ..
            } => {
                assert!(!overflow);
                assert_eq!(params.len(), MAX_PARAMS);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn can_and_sub_abort() {
        assert_eq!(
            run(&[b"\x1b[1\x18m"]),
            vec![Action::Execute(0x18), Action::Print('m')]
        );
        assert_eq!(
            run(&[b"\x1b[1\x1am"]),
            vec![Action::Execute(0x1a), Action::Print('m')]
        );
        assert_eq!(
            run(&[b"\x1b\x18A"]),
            vec![Action::Execute(0x18), Action::Print('A')]
        );
        // In an OSC the string is discarded without dispatch.
        assert_eq!(run(&[b"\x1b]0;title\x18A"]), print_all("A"));
        assert_eq!(run(&[b"\x1b]0;title\x1aA"]), print_all("A"));
        // An unfinished DCS is unhooked.
        assert_eq!(
            run(&[b"\x1bPq ab\x18A"]),
            vec![
                Action::Hook {
                    params: vec![],
                    inter: vec![],
                    private: None,
                    ignore: false,
                    action: b'q'
                },
                Action::Put(b' '),
                Action::Put(b'a'),
                Action::Put(b'b'),
                Action::Unhook,
                Action::Print('A')
            ]
        );
        // SOS/PM/APC strings end silently.
        assert_eq!(run(&[b"\x1b_data\x18A"]), print_all("A"));
    }

    #[test]
    fn osc_terminators() {
        let bel = Action::Osc {
            payload: b"0;hi".to_vec(),
            truncated: false,
            bell: true,
        };
        assert_eq!(run(&[b"\x1b]0;hi\x07"]), vec![bel]);
        // ESC \ dispatches at the ESC; the backslash then arrives as an ESC sequence.
        assert_eq!(
            run(&[b"\x1b]0;hi\x1b\\"]),
            vec![
                Action::Osc {
                    payload: b"0;hi".to_vec(),
                    truncated: false,
                    bell: false
                },
                Action::Esc {
                    inter: vec![],
                    ignore: false,
                    byte: b'\\'
                }
            ]
        );
        // C0 controls other than the terminators are ignored in the payload.
        assert_eq!(
            run(&[b"\x1b]0;a\nb\x07"]),
            vec![Action::Osc {
                payload: b"0;ab".to_vec(),
                truncated: false,
                bell: true
            }]
        );
        // Non-ASCII payload bytes are kept raw.
        assert_eq!(
            run(&[b"\x1b]0;\xc3\xa9\x9c\x07"]),
            vec![Action::Osc {
                payload: b"0;\xc3\xa9\x9c".to_vec(),
                truncated: false,
                bell: true
            }]
        );
    }

    #[test]
    fn osc_split_on_semicolon() {
        let parts: Vec<&[u8]> = osc_split(b"8;id=1;http://x").collect();
        assert_eq!(parts, vec![&b"8"[..], b"id=1", b"http://x"]);
        let parts: Vec<&[u8]> = osc_split(b"").collect();
        assert_eq!(parts, vec![&b""[..]]);
    }

    #[test]
    fn osc_overlong_is_truncated() {
        let mut input = b"\x1b]52;c;".to_vec();
        input.resize(input.len() + OSC_MAX_LEN + 100, b'A');
        input.push(0x07);
        let got = run(&[&input]);
        match &got[0] {
            Action::Osc {
                payload, truncated, ..
            } => {
                assert!(truncated);
                assert_eq!(payload.len(), OSC_MAX_LEN);
            }
            other => panic!("{other:?}"),
        }
        // The flag does not leak into the next OSC.
        let mut parser = Parser::new();
        let mut rec = Rec::default();
        parser.advance(&mut rec, &input);
        parser.advance(&mut rec, b"\x1b]0;x\x07");
        assert_eq!(
            rec.0[1],
            Action::Osc {
                payload: b"0;x".to_vec(),
                truncated: false,
                bell: true
            }
        );
    }

    #[test]
    fn dcs_hook_put_unhook() {
        let want = vec![
            Action::Hook {
                params: vec![vec![1], vec![2]],
                inter: vec![b'$'],
                private: None,
                ignore: false,
                action: b'q',
            },
            Action::Put(b'x'),
            Action::Put(b'\n'),
            Action::Unhook,
            Action::Esc {
                inter: vec![],
                ignore: false,
                byte: b'\\',
            },
        ];
        assert_eq!(run(&[b"\x1bP1;2$qx\n\x1b\\"]), want);
        // DEL is not passed on.
        assert_eq!(run(&[b"\x1bPq\x7f\x1b\\"]).len(), 3);
        // A private marker is reported.
        match &run(&[b"\x1bP?1qz\x1b\\"])[0] {
            Action::Hook { private, .. } => assert_eq!(*private, Some(b'?')),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dcs_ignore_state() {
        // A private marker after a parameter poisons the header; no hook or put happens.
        assert_eq!(
            run(&[b"\x1bP1?qdata\x1b\\A"]),
            vec![
                Action::Esc {
                    inter: vec![],
                    ignore: false,
                    byte: b'\\'
                },
                Action::Print('A')
            ]
        );
    }

    #[test]
    fn sos_pm_apc_ignored() {
        for intro in *b"X^_" {
            let input = [0x1b, intro, b'h', b'i', 0x07, 0x1b, b'\\', b'A'];
            assert_eq!(
                run(&[&input]),
                vec![
                    Action::Esc {
                        inter: vec![],
                        ignore: false,
                        byte: b'\\'
                    },
                    Action::Print('A')
                ]
            );
        }
    }

    #[test]
    fn esc_inside_strings_starts_new_sequence() {
        assert_eq!(
            run(&[b"\x1b]0;a\x1b[1mB"]),
            vec![
                Action::Osc {
                    payload: b"0;a".to_vec(),
                    truncated: false,
                    bell: false
                },
                csi(&[&[1]], b'm'),
                Action::Print('B')
            ]
        );
        assert_eq!(
            run(&[b"\x1bPqz\x1b[1m"]),
            vec![
                Action::Hook {
                    params: vec![],
                    inter: vec![],
                    private: None,
                    ignore: false,
                    action: b'q'
                },
                Action::Put(b'z'),
                Action::Unhook,
                csi(&[&[1]], b'm')
            ]
        );
    }

    #[test]
    fn c1_as_utf8_characters() {
        // U+009B is CSI, U+009D OSC, U+009C ST (ignored outside strings), U+0085 NEL.
        assert_eq!(run(&[b"\xc2\x9b1m"]), vec![csi(&[&[1]], b'm')]);
        assert_eq!(
            run(&[b"\xc2\x9d0;t\x07"]),
            vec![Action::Osc {
                payload: b"0;t".to_vec(),
                truncated: false,
                bell: true
            }]
        );
        assert_eq!(run(&[b"\xc2\x85"]), vec![Action::Execute(0x85)]);
        assert_eq!(run(&[b"\xc2\x9cA"]), print_all("A"));
    }

    #[test]
    fn c1_bytes_that_continue_utf8_are_not_controls() {
        // U+20DC is E2 83 9C: 0x9C is a continuation byte and must not act as ST.
        assert_eq!(run(&[b"\xe2\x83\x9c"]), print_all("\u{20DC}"));
        // U+019B is C6 9B; 0x9B must not act as CSI.
        assert_eq!(run(&[b"\xc6\x9b1m"]), print_all("\u{19B}1m"));
        // U+1F49D is F0 9F 92 9D; 0x9D must not start an OSC.
        assert_eq!(run(&[b"\xf0\x9f\x92\x9dx"]), print_all("\u{1F49D}x"));
        // A lone 0x9B that continues nothing is invalid, not CSI.
        assert_eq!(run(&[b"\x9b1m"]), print_all("\u{FFFD}1m"));
    }

    #[test]
    fn invalid_and_overlong_utf8() {
        assert_eq!(run(&[b"\xc0\x80"]), print_all("\u{FFFD}\u{FFFD}"));
        assert_eq!(
            run(&[b"\xe0\x80\x80"]),
            print_all("\u{FFFD}\u{FFFD}\u{FFFD}")
        );
        assert_eq!(
            run(&[b"\xf8\x88\x80\x80\x80"]),
            print_all("\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}")
        );
        assert_eq!(
            run(&[b"\xed\xa0\x80"]),
            print_all("\u{FFFD}\u{FFFD}\u{FFFD}")
        );
        assert_eq!(run(&[b"\xff"]), print_all("\u{FFFD}"));
        // A truncated sequence is cut short by the next byte, which is handled normally.
        assert_eq!(run(&[b"\xe2\x82A"]), print_all("\u{FFFD}A"));
        let mut want = print_all("\u{FFFD}");
        want.push(Action::Execute(b'\n'));
        assert_eq!(run(&[b"\xe2\x82\n"]), want);
        // ESC cuts a sequence short too and starts an escape sequence.
        assert_eq!(
            run(&[b"\xe2\x1b7"]),
            vec![
                Action::Print('\u{FFFD}'),
                Action::Esc {
                    inter: vec![],
                    ignore: false,
                    byte: b'7'
                }
            ]
        );
    }

    #[test]
    fn utf8_split_across_advance_calls() {
        let euro = "€".as_bytes();
        assert_eq!(run(&[&euro[..1], &euro[1..]]), print_all("€"));
        assert_eq!(run(&[&euro[..2], &euro[2..]]), print_all("€"));
        let emoji = "😀".as_bytes();
        assert_eq!(
            run(&[&emoji[..1], &emoji[1..2], &emoji[2..3], &emoji[3..]]),
            print_all("😀")
        );
        // Split before a following invalid byte.
        assert_eq!(run(&[&euro[..2], b"A"]), print_all("\u{FFFD}A"));
    }

    #[test]
    fn non_ascii_inside_sequences_is_dropped() {
        assert_eq!(run(&[b"\x1b[1\xc3\xa9m"]), vec![csi(&[&[1]], b'm')]);
        assert_eq!(
            run(&[b"\x1b\xc3\xa97"]),
            vec![Action::Esc {
                inter: vec![],
                ignore: false,
                byte: b'7'
            }]
        );
    }

    #[test]
    fn csi_after_incomplete_sequences() {
        // A new CSI introducer abandons the old one.
        assert_eq!(run(&[b"\x1b[1\x1b[2m"]), vec![csi(&[&[2]], b'm')]);
        // The params of the old sequence do not leak.
        assert_eq!(run(&[b"\x1b[1;2\x1b[m"]), vec![csi(&[], b'm')]);
    }

    #[test]
    fn buffers_stay_stable() {
        let mut parser = Parser::new();
        let mut rec = Rec::default();
        let mut big = b"\x1b]52;c;".to_vec();
        big.resize(big.len() + 5000, b'A');
        big.push(0x07);
        parser.advance(&mut rec, &big);
        let cap = parser.osc_capacity();
        assert!(cap >= 5000);
        for _ in 0..100 {
            rec.0.clear();
            parser.advance(&mut rec, &big);
            parser.advance(
                &mut rec,
                b"\x1b]0;short\x07\x1b[1;2;3m\x1bPqabc\x1b\\plain text",
            );
            assert_eq!(parser.osc_capacity(), cap);
        }
        // A bounded buffer never exceeds its documented limit by more than doubling.
        let mut huge = b"\x1b]0;".to_vec();
        huge.resize(huge.len() + OSC_MAX_LEN * 2, b'x');
        parser.advance(&mut rec, &huge);
        assert!(parser.osc_capacity() <= OSC_MAX_LEN * 2);
        let cap = parser.osc_capacity();
        parser.advance(&mut rec, &huge);
        assert_eq!(parser.osc_capacity(), cap);
    }

    #[test]
    fn dcs_intermediate_without_params_hooks() {
        match &run(&[b"\x1bP$qx\x1b\\"])[0] {
            Action::Hook { params, inter, .. } => {
                assert_eq!(inter, &vec![b'$']);
                assert!(params.is_empty(), "{params:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn dcs_param_after_intermediate_is_ignored() {
        let want = vec![
            Action::Esc {
                inter: vec![],
                ignore: false,
                byte: b'\\',
            },
            Action::Print('A'),
        ];
        assert_eq!(run(&[b"\x1bP$1qdata\x1b\\A"]), want);
    }

    #[test]
    fn utf8_st_inside_dcs_ignore() {
        // `1?q` has a private marker and a final byte nothing handles: DcsIgnore.
        assert_eq!(run(&[b"\x1bP1?qdata\xc2\x9cA"]), print_all("A"));
        // Split across calls, between C2 and 9C.
        assert_eq!(run(&[b"\x1bP1?qdata\xc2", b"\x9cA"]), print_all("A"));
        // Split right after the introducer.
        assert_eq!(run(&[b"\x1bP1?q\xc2", b"\x9cA"]), print_all("A"));
        // Raw 0x9C and C3 9C are payload, and the string is still open for the real ST.
        assert_eq!(run(&[b"\x1bP1?q\x9c\xc3\x9cA"]), vec![]);
        let a = run(&[b"\x1bP1?q\x9c\xc3\x9cA\x1b\\B"]);
        assert_eq!(a.last(), Some(&Action::Print('B')));
        assert!(!a.contains(&Action::Print('A')));
        // C2 followed by another byte is payload, and the string stays open.
        assert_eq!(run(&[b"\x1bP1?q\xc2xA"]), vec![]);
    }

    #[test]
    fn can_ends_dcs_ignore() {
        assert_eq!(run(&[b"\x1bP1?qdata\x18A"]), print_all("A"));
    }

    #[test]
    fn utf8_st_inside_strings() {
        let osc = |bell| Action::Osc {
            payload: b"0;t".to_vec(),
            truncated: false,
            bell,
        };
        assert_eq!(run(&[b"\x1b]0;t\xc2\x9cA"]), {
            let mut v = vec![osc(false)];
            v.extend(print_all("A"));
            v
        });
        // Split across calls.
        assert_eq!(run(&[b"\x1b]0;t\xc2", b"\x9cA"]), {
            let mut v = vec![osc(false)];
            v.extend(print_all("A"));
            v
        });
        // Raw 0x9C and C3 9C stay payload.
        assert_eq!(
            run(&[b"\x1b]0;\x9c\xc3\x9c\x07"]),
            vec![Action::Osc {
                payload: b"0;\x9c\xc3\x9c".to_vec(),
                truncated: false,
                bell: true
            }]
        );
        // C2 followed by another byte is payload, then the byte is handled normally.
        assert_eq!(
            run(&[b"\x1b]0;\xc2x\x07"]),
            vec![Action::Osc {
                payload: b"0;\xc2x".to_vec(),
                truncated: false,
                bell: true
            }]
        );
        // DCS passthrough: C2 9C unhooks; raw 0x9C and C3 9C are put.
        let a = run(&[b"\x1bPqa\xc2\x9cB"]);
        assert!(matches!(a[0], Action::Hook { .. }));
        assert_eq!(a[1], Action::Put(b'a'));
        assert_eq!(a[2], Action::Unhook);
        assert_eq!(a[3], Action::Print('B'));
        assert_eq!(a.len(), 4);
        let a = run(&[b"\x1bPq\x9c\xc3\x9c\x1b\\"]);
        let puts: Vec<_> = a.iter().filter(|x| matches!(x, Action::Put(_))).collect();
        assert_eq!(puts.len(), 3);
        // SOS, PM and APC.
        for intro in [&b"\x1bX"[..], b"\x1b^", b"\x1b_"] {
            let mut input = intro.to_vec();
            input.extend_from_slice(b"zz\xc2");
            assert_eq!(run(&[&input, b"\x9cA"]), print_all("A"));
            input.truncate(intro.len());
            input.extend_from_slice(b"\xc3\x9cA");
            assert_eq!(run(&[&input]), vec![], "C3 9C is payload");
        }
    }

    #[test]
    fn reset_clears_pending_c2() {
        let mut parser = Parser::new();
        let mut rec = Rec::default();
        parser.advance(&mut rec, b"\x1b]0;t\xc2");
        parser.reset();
        parser.advance(&mut rec, b"\x1b]0;t\x07");
        assert_eq!(
            rec.0,
            vec![Action::Osc {
                payload: b"0;t".to_vec(),
                truncated: false,
                bell: true
            }]
        );
    }

    #[test]
    fn reset_returns_to_ground() {
        let mut parser = Parser::new();
        let mut rec = Rec::default();
        parser.advance(&mut rec, b"\x1b[1;2\xe2\x82");
        parser.reset();
        parser.advance(&mut rec, b"m");
        assert_eq!(rec.0, print_all("m"));
    }

    fn arb_bytes() -> impl Strategy<Value = Vec<u8>> {
        // Mix fully random bytes with the ones that drive the state machine.
        let special = prop::sample::select(
            b"\x1b[]P_X^\\;:?<=> 0123456789mHhq\x07\x18\x1a\x7f\x9b\x9d\x9c\x90\xc2\xe2\xf0\x82\x80\n".to_vec(),
        );
        prop::collection::vec(prop_oneof![any::<u8>(), special.clone(), special], 0..200)
    }

    proptest! {
        #[test]
        fn never_panics(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
            run(&[&bytes]);
        }

        #[test]
        fn never_panics_on_structured_input(bytes in arb_bytes()) {
            run(&[&bytes]);
        }

        #[test]
        fn split_anywhere_is_equivalent(bytes in arb_bytes(), cut in any::<prop::sample::Index>()) {
            let k = cut.index(bytes.len() + 1);
            let whole = run(&[&bytes]);
            let split = run(&[&bytes[..k], &bytes[k..]]);
            prop_assert_eq!(whole, split);
        }

        #[test]
        fn byte_at_a_time_is_equivalent(bytes in arb_bytes()) {
            let whole = run(&[&bytes]);
            let singles: Vec<&[u8]> = bytes.chunks(1).collect();
            prop_assert_eq!(whole, run(&singles));
        }
    }
}
