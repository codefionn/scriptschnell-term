//! OSC strings, DCS requests and the device reports that are not a plain cursor report.
//!
//! These are cold paths: they may allocate. Replies go through `Core::reply`, events
//! through `Core::event`, so both stay bounded.

use super::*;
use crate::color::Palette;
use crate::osc::{
    Mark, Progress, SemanticKind, base64_decode, base64_encode, format_color, parse_color,
    parse_file_url,
};
use std::fmt::Write as _;

/// Longest DCS request body kept.
const DCS_MAX: usize = 256;
/// Titles the stack keeps, as in xterm.
const TITLE_STACK: usize = 10;

/// A DCS request being collected.
#[derive(Debug, Clone, Default)]
pub(super) enum Dcs {
    #[default]
    None,
    /// DECRQSS, `DCS $ q Pt ST`.
    Rqss,
    /// XTGETTCAP, `DCS + q Pt ST`.
    Tcap,
}

/// One entry of the title stack.
#[derive(Debug, Clone)]
pub(super) struct TitleEntry {
    title: Option<String>,
    icon: Option<String>,
}

fn terminator(end: OscEnd) -> &'static str {
    if end.bell { "\x07" } else { "\x1b\\" }
}

fn parse_num(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

fn lossy(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn hex_decode(s: &[u8]) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    s.chunks(2)
        .map(|p| Some(hex_val(p[0])? << 4 | hex_val(p[1])?))
        .collect()
}

fn hex_encode(s: &[u8]) -> String {
    s.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02X}");
        out
    })
}

/// Terminal capabilities XTGETTCAP answers, by terminfo and termcap name.
fn capability(name: &[u8]) -> Option<&'static str> {
    match name {
        b"TN" | b"name" => Some("xterm-256color"),
        b"Co" | b"colors" => Some("256"),
        b"RGB" => Some("8/8/8"),
        _ => None,
    }
}

impl Core {
    // OSC ---------------------------------------------------------------------------------

    pub(super) fn osc(&mut self, payload: &[u8], end: OscEnd) {
        let code = osc_split(payload).next().unwrap_or(&[]);
        let rest = payload.get(code.len() + 1..).unwrap_or(&[]);
        if end.truncated && is_known(code) {
            return;
        }
        if !self.osc_known(code, rest, end) {
            self.event(Event::Osc {
                code: lossy(code),
                payload: rest.to_vec(),
                end,
            });
        }
    }

    /// Handles `code` and returns true, or returns false for a passthrough.
    fn osc_known(&mut self, code: &[u8], rest: &[u8], end: OscEnd) -> bool {
        match code {
            b"0" | b"2" => {
                let t = lossy(rest);
                self.title.clone_from(&t);
                if code == b"0" {
                    self.icon.clone_from(&t);
                }
                self.event(Event::Title(t));
            }
            b"1" => {
                let t = lossy(rest);
                self.icon.clone_from(&t);
                self.event(Event::IconName(t));
            }
            b"4" => self.osc_palette(rest, end),
            b"104" => self.osc_reset_palette(rest),
            b"10" => self.osc_dynamic(10, rest, end),
            b"11" => self.osc_dynamic(11, rest, end),
            b"12" => self.osc_dynamic(12, rest, end),
            b"110" => self.reset_dynamic(10),
            b"111" => self.reset_dynamic(11),
            b"112" => self.reset_dynamic(12),
            b"7" => {
                if let Some(dir) = parse_file_url(rest) {
                    self.set_cwd(dir);
                }
            }
            b"8" => self.osc_hyperlink(rest),
            b"52" => self.osc_clipboard(rest, end),
            b"9" => self.osc_notify(rest),
            b"777" => {
                let Some(args) = rest.strip_prefix(b"notify;") else {
                    return false;
                };
                if end.truncated {
                    return true;
                }
                let (title, body) = match args.iter().position(|&b| b == b';') {
                    Some(i) => (&args[..i], &args[i + 1..]),
                    None => (args, &[][..]),
                };
                self.event(Event::Toast {
                    title: Some(lossy(title)),
                    body: lossy(body),
                });
            }
            b"1337" => {
                if end.truncated
                    && (rest.starts_with(b"SetUserVar=") || rest.starts_with(b"CurrentDir="))
                {
                    return true;
                }
                if let Some(arg) = rest.strip_prefix(b"SetUserVar=") {
                    // An invalid value is dropped, the way wezterm does.
                    if let Some(i) = arg.iter().position(|&b| b == b'=')
                        && let Some(value) = base64_decode(&arg[i + 1..])
                    {
                        self.event(Event::UserVar {
                            name: lossy(&arg[..i]),
                            value: lossy(&value),
                        });
                    }
                } else if let Some(path) = rest.strip_prefix(b"CurrentDir=") {
                    self.set_cwd(crate::osc::CurrentDir {
                        host: String::new(),
                        path: lossy(path),
                    });
                } else {
                    return false;
                }
            }
            b"133" => self.osc_semantic(rest),
            _ => return false,
        }
        true
    }

    fn set_cwd(&mut self, dir: crate::osc::CurrentDir) {
        self.cwd = Some(dir.clone());
        self.event(Event::CurrentDir(dir));
    }

    fn reply_osc(&mut self, body: &str, end: OscEnd) {
        let mut out = Vec::with_capacity(body.len() + 4);
        out.extend_from_slice(b"\x1b]");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(terminator(end).as_bytes());
        self.reply(&out);
    }

    fn osc_palette(&mut self, rest: &[u8], end: OscEnd) {
        let mut it = osc_split(rest);
        let mut changed = false;
        while let (Some(index), Some(spec)) = (it.next(), it.next()) {
            let Some(i) = parse_num(index).filter(|&i| i < 256) else {
                continue;
            };
            let i = i as u8;
            if spec == b"?" {
                let c = format_color(self.palette.get(i));
                self.reply_osc(&format!("4;{i};{c}"), end);
            } else if let Some(c) = parse_color(spec) {
                self.palette.set(i, c);
                changed = true;
            }
        }
        if changed {
            self.event(Event::PaletteChanged);
        }
    }

    fn osc_reset_palette(&mut self, rest: &[u8]) {
        let defaults = Palette::new(&self.config);
        if rest.is_empty() {
            for i in 0..=255u8 {
                self.palette.set(i, defaults.get(i));
            }
        } else {
            for index in osc_split(rest) {
                if let Some(i) = parse_num(index).filter(|&i| i < 256) {
                    self.palette.set(i as u8, defaults.get(i as u8));
                }
            }
        }
        self.event(Event::PaletteChanged);
    }

    /// OSC 10, 11 and 12. Extra specs set the next colors in the row, as xterm does.
    fn osc_dynamic(&mut self, first: u8, rest: &[u8], end: OscEnd) {
        let mut changed = false;
        for (k, spec) in osc_split(rest).enumerate() {
            let target = first + k as u8;
            if target > 12 {
                break;
            }
            if spec == b"?" {
                let c = match target {
                    10 => self.palette.fg,
                    11 => self.palette.bg,
                    _ => self.palette.cursor,
                };
                self.reply_osc(&format!("{target};{}", format_color(c)), end);
            } else if let Some(c) = parse_color(spec) {
                match target {
                    10 => self.palette.fg = c,
                    11 => self.palette.bg = c,
                    _ => self.palette.cursor = c,
                }
                changed = true;
            }
        }
        if changed {
            self.event(Event::PaletteChanged);
        }
    }

    fn reset_dynamic(&mut self, target: u8) {
        match target {
            10 => self.palette.fg = self.config.fg,
            11 => self.palette.bg = self.config.bg,
            _ => self.palette.cursor = self.config.cursor,
        }
        self.event(Event::PaletteChanged);
    }

    fn osc_hyperlink(&mut self, rest: &[u8]) {
        let Some(semi) = rest.iter().position(|&b| b == b';') else {
            return;
        };
        let (params, uri) = (&rest[..semi], &rest[semi + 1..]);
        if uri.is_empty() {
            self.style.hyperlink = 0;
        } else {
            if uri.iter().any(|&b| b < 0x20 || b == 0x7f) {
                return;
            }
            let id = params
                .split(|&b| b == b':')
                .find_map(|p| p.strip_prefix(b"id="))
                .map(String::from_utf8_lossy);
            let uri = String::from_utf8_lossy(uri);
            let mut link = self.links.intern(id.as_deref(), &uri);
            if link.is_none() && uri.len() <= crate::osc::MAX_URI {
                // The table is full. Links that no cell uses any more may free room,
                // but a full table of live links is not rescanned per sequence.
                if self.screen_seq().abs_diff(self.tables_seq) >= super::SWEEP_RETRY_WRITES {
                    self.collect_tables();
                    link = self.links.intern(id.as_deref(), &uri);
                }
            }
            self.style.hyperlink = link.unwrap_or(0);
        }
        self.refresh_style();
    }

    fn osc_clipboard(&mut self, rest: &[u8], end: OscEnd) {
        let Some(semi) = rest.iter().position(|&b| b == b';') else {
            return;
        };
        let (sel, data) = (&rest[..semi], &rest[semi + 1..]);
        if !sel.iter().all(|b| b"cpqs01234567".contains(b)) {
            return;
        }
        let selection = if sel.is_empty() {
            "s0".to_string()
        } else {
            lossy(sel)
        };
        if data == b"?" {
            if self.config.allow_clipboard_query {
                self.event(Event::ClipboardQuery {
                    selection,
                    bell: end.bell,
                });
            }
        } else if let Some(data) = base64_decode(data) {
            self.event(Event::Clipboard { selection, data });
        }
    }

    pub(super) fn clipboard_reply(&mut self, selection: &str, data: &[u8], bell: bool) {
        let end = OscEnd {
            truncated: false,
            bell,
        };
        self.reply_osc(&format!("52;{selection};{}", base64_encode(data)), end);
    }

    fn osc_notify(&mut self, rest: &[u8]) {
        if rest == b"4" || rest.starts_with(b"4;") {
            let mut it = osc_split(rest).skip(1);
            let state = it.next().and_then(parse_num);
            let pct = it
                .next()
                .and_then(parse_num)
                .map_or(0, |p| p.min(100) as u8);
            let progress = match state {
                Some(0) => Progress::None,
                Some(1) => Progress::Percentage(pct),
                Some(2) => Progress::Error(pct),
                Some(3) => Progress::Indeterminate,
                Some(4) => Progress::Paused(pct),
                _ => return,
            };
            self.event(Event::Progress(progress));
        } else {
            self.event(Event::Toast {
                title: None,
                body: lossy(rest),
            });
        }
    }

    fn osc_semantic(&mut self, rest: &[u8]) {
        if self.active != 0 {
            return;
        }
        let mut it = osc_split(rest);
        let kind = match it.next().unwrap_or(&[]) {
            b"A" => SemanticKind::Prompt,
            b"B" => SemanticKind::Input,
            b"C" => SemanticKind::Output,
            b"D" => {
                let status = it
                    .next()
                    .filter(|s| !s.contains(&b'='))
                    .and_then(|s| std::str::from_utf8(s).ok())
                    .and_then(|s| s.parse::<i32>().ok());
                if let Some(status) = status {
                    self.marks.set_exit(status);
                }
                return;
            }
            _ => return,
        };
        let aid = it
            .find_map(|p| p.strip_prefix(b"aid="))
            .map(|a| lossy(a).into_boxed_str());
        self.prune_marks();
        self.marks.push(Mark {
            row: self.screens[0].stable_index(self.row),
            col: self.col,
            kind,
            aid,
            exit: None,
        });
    }

    pub(super) fn semantic_zones(&self) -> Vec<crate::osc::SemanticZone> {
        use crate::osc::SemanticZone;
        let screen = &self.screens[0];
        let first = screen.first_stable();
        let last = (screen.next_stable() - 1, self.cols - 1);
        // Marks that scrolled out of the history: the newest one still decides what the
        // oldest kept line is.
        let mut starts: Vec<(&Mark, (u64, usize))> = Vec::new();
        for m in self.marks.iter() {
            if m.row < first {
                starts.clear();
                starts.push((m, (first, 0)));
            } else {
                starts.push((m, (m.row, m.col)));
            }
        }
        let mut zones = Vec::new();
        let before = |(row, col): (u64, usize)| {
            if col > 0 {
                (row, col - 1)
            } else {
                (row.saturating_sub(1), self.cols - 1)
            }
        };
        if let Some(&(_, pos)) = starts.first()
            && pos > (first, 0)
        {
            let end = before(pos);
            zones.push(SemanticZone {
                kind: SemanticKind::Output,
                start_row: first,
                start_col: 0,
                end_row: end.0,
                end_col: end.1,
                aid: None,
                exit_status: None,
            });
        }
        // Input and output belong to the prompt before them, so they share its aid.
        let mut aid: Option<&str> = None;
        for (i, &(m, pos)) in starts.iter().enumerate() {
            if m.kind == SemanticKind::Prompt || m.aid.is_some() {
                aid = m.aid.as_deref();
            }
            let end = match starts.get(i + 1) {
                Some(&(_, next)) if next <= pos => continue,
                Some(&(_, next)) => before(next),
                None => last,
            };
            zones.push(SemanticZone {
                kind: m.kind,
                start_row: pos.0,
                start_col: pos.1,
                end_row: end.0,
                end_col: end.1,
                aid: aid.map(String::from),
                exit_status: m.exit,
            });
        }
        zones
    }

    // Title stack and window operations -----------------------------------------------

    pub(super) fn winops(&mut self, a: &[u16]) {
        let (rows, cols) = (self.rows, self.cols);
        let (cw, ch) = (
            self.config.cell_pixels.0 as usize,
            self.config.cell_pixels.1 as usize,
        );
        match a.first().copied() {
            Some(14) if cw > 0 && ch > 0 => {
                self.reply_fmt(format_args!("\x1b[4;{};{}t", rows * ch, cols * cw));
            }
            Some(16) if cw > 0 && ch > 0 => {
                self.reply_fmt(format_args!("\x1b[6;{ch};{cw}t"));
            }
            Some(18 | 19) => {
                let kind = a[0] - 10;
                self.reply_fmt(format_args!("\x1b[{kind};{rows};{cols}t"));
            }
            Some(22) => self.push_title(a.get(1).copied().unwrap_or(0)),
            Some(23) => self.pop_title(a.get(1).copied().unwrap_or(0)),
            _ => {}
        }
    }

    fn push_title(&mut self, which: u16) {
        if which > 2 {
            return;
        }
        if self.title_stack.len() >= TITLE_STACK {
            self.title_stack.remove(0);
        }
        self.title_stack.push(TitleEntry {
            title: (which != 1).then(|| self.title.clone()),
            icon: (which != 2).then(|| self.icon.clone()),
        });
    }

    fn pop_title(&mut self, which: u16) {
        if which > 2 {
            return;
        }
        let Some(entry) = self.title_stack.pop() else {
            return;
        };
        if which != 1
            && let Some(t) = entry.title
        {
            self.title.clone_from(&t);
            self.event(Event::Title(t));
        }
        if which != 2
            && let Some(i) = entry.icon
        {
            self.icon.clone_from(&i);
            self.event(Event::IconName(i));
        }
    }

    // Device reports -------------------------------------------------------------------

    pub(super) fn xtversion(&mut self) {
        let mut out = b"\x1bP>|".to_vec();
        out.extend_from_slice(self.config.xtversion.as_bytes());
        out.extend_from_slice(b"\x1b\\");
        self.reply(&out);
    }

    pub(super) fn dcs_hook(
        &mut self,
        intermediates: &[u8],
        private: Option<u8>,
        ignore: bool,
        action: u8,
    ) {
        self.dcs_data.clear();
        self.dcs_overflow = false;
        self.dcs = match (private, intermediates, action) {
            (None, [b'$'], b'q') if !ignore => Dcs::Rqss,
            (None, [b'+'], b'q') if !ignore => Dcs::Tcap,
            _ => Dcs::None,
        };
    }

    pub(super) fn dcs_put(&mut self, byte: u8) {
        if matches!(self.dcs, Dcs::None) {
            return;
        }
        if self.dcs_data.len() < DCS_MAX {
            self.dcs_data.push(byte);
        } else {
            self.dcs_overflow = true;
        }
    }

    pub(super) fn dcs_unhook(&mut self) {
        let data = std::mem::take(&mut self.dcs_data);
        match std::mem::take(&mut self.dcs) {
            Dcs::None => {}
            Dcs::Rqss => self.decrqss(&data),
            Dcs::Tcap => self.xtgettcap(&data),
        }
    }

    fn decrqss(&mut self, pt: &[u8]) {
        let body = if self.dcs_overflow {
            None
        } else {
            match pt {
                b"m" => Some(format!("{}m", self.sgr_report())),
                b"r" => Some(format!("{};{}r", self.top + 1, self.bottom + 1)),
                b"s" => Some(format!("{};{}s", self.left + 1, self.right + 1)),
                b" q" => {
                    let n = match (self.shape, self.blink) {
                        (CursorShape::Block, true) => 1,
                        (CursorShape::Block, false) => 2,
                        (CursorShape::Underline, true) => 3,
                        (CursorShape::Underline, false) => 4,
                        (CursorShape::Bar, true) => 5,
                        (CursorShape::Bar, false) => 6,
                    };
                    Some(format!("{n} q"))
                }
                _ => None,
            }
        };
        match body {
            Some(b) => self.reply(format!("\x1bP1$r{b}\x1b\\").as_bytes()),
            None => self.reply(b"\x1bP0$r\x1b\\"),
        }
    }

    /// The SGR parameters that recreate the pen, led by 0 as xterm writes them.
    fn sgr_report(&self) -> String {
        let st = &self.style;
        let mut s = String::from("0");
        for (flag, code) in [
            (flags::BOLD, "1"),
            (flags::FAINT, "2"),
            (flags::ITALIC, "3"),
        ] {
            if st.has(flag) {
                s.push(';');
                s.push_str(code);
            }
        }
        s.push_str(match st.underline {
            Underline::None => "",
            Underline::Single => ";4",
            Underline::Double => ";4:2",
            Underline::Curly => ";4:3",
            Underline::Dotted => ";4:4",
            Underline::Dashed => ";4:5",
        });
        for (flag, code) in [
            (flags::BLINK, "5"),
            (flags::INVERSE, "7"),
            (flags::INVISIBLE, "8"),
            (flags::STRIKE, "9"),
        ] {
            if st.has(flag) {
                s.push(';');
                s.push_str(code);
            }
        }
        match st.fg {
            Color::Default => {}
            Color::Palette(n @ 0..=7) => {
                let _ = write!(s, ";{}", 30 + n);
            }
            Color::Palette(n @ 8..=15) => {
                let _ = write!(s, ";{}", 90 + n - 8);
            }
            Color::Palette(n) => {
                let _ = write!(s, ";38:5:{n}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(s, ";38:2::{r}:{g}:{b}");
            }
        }
        match st.bg {
            Color::Default => {}
            Color::Palette(n @ 0..=7) => {
                let _ = write!(s, ";{}", 40 + n);
            }
            Color::Palette(n @ 8..=15) => {
                let _ = write!(s, ";{}", 100 + n - 8);
            }
            Color::Palette(n) => {
                let _ = write!(s, ";48:5:{n}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(s, ";48:2::{r}:{g}:{b}");
            }
        }
        match st.underline_color {
            Color::Default => {}
            Color::Palette(n) => {
                let _ = write!(s, ";58:5:{n}");
            }
            Color::Rgb(r, g, b) => {
                let _ = write!(s, ";58:2::{r}:{g}:{b}");
            }
        }
        s
    }

    fn xtgettcap(&mut self, pt: &[u8]) {
        if self.dcs_overflow {
            // The data was cut at DCS_MAX. The first name is complete only if a ';'
            // follows it inside the kept bytes. Otherwise the reply echoes a cut name.
            let hex = pt.split(|&b| b == b';').next().unwrap_or_default();
            let reply = format!("\x1bP0+r{}\x1b\\", lossy(hex));
            self.reply(reply.as_bytes());
            return;
        }
        for hex in pt.split(|&b| b == b';') {
            let value = hex_decode(hex).and_then(|name| capability(&name));
            match value {
                Some(v) => {
                    let reply =
                        format!("\x1bP1+r{}={}\x1b\\", lossy(hex), hex_encode(v.as_bytes()));
                    self.reply(reply.as_bytes());
                }
                None => {
                    let reply = format!("\x1bP0+r{}\x1b\\", lossy(hex));
                    self.reply(reply.as_bytes());
                }
            }
        }
    }
}

fn is_known(code: &[u8]) -> bool {
    matches!(
        code,
        b"0" | b"1"
            | b"2"
            | b"4"
            | b"7"
            | b"8"
            | b"9"
            | b"10"
            | b"11"
            | b"12"
            | b"52"
            | b"104"
            | b"110"
            | b"111"
            | b"112"
            | b"133"
    )
}
