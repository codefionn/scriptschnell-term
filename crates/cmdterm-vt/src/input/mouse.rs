//! Mouse reporting: tracking modes 9, 1000, 1002 and 1003 with the encodings 1005, 1006,
//! 1015 and 1016, and the alternate scroll mode 1007.
//!
//! The state that depends on earlier events (held buttons, the last reported position)
//! lives in [`MouseState`]; the terminal owns one and [`encode_mouse`] updates it.

use super::key::{KeyCode, KeyModes, encode_key};
use super::{Mods, push_dec};

/// What the program asked to be told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseTracking {
    #[default]
    Off,
    /// Mode 9: presses only, no modifiers.
    X10,
    /// Mode 1000: presses and releases.
    Normal,
    /// Mode 1002: also motion while a button is held.
    ButtonEvent,
    /// Mode 1003: all motion.
    AnyEvent,
}

/// How coordinates are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseEncoding {
    /// `CSI M` followed by three bytes.
    #[default]
    X10,
    /// Mode 1005: like X10 with UTF-8 coordinates.
    Utf8,
    /// Mode 1006: `CSI < b ; x ; y M` or `m`.
    Sgr,
    /// Mode 1015: `CSI b ; x ; y M`.
    Urxvt,
    /// Mode 1016: SGR with pixel coordinates.
    SgrPixels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// No button: a plain move, or a release that does not say which button.
    None,
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Back,
    Forward,
}

impl MouseButton {
    fn is_wheel(self) -> bool {
        matches!(
            self,
            MouseButton::WheelUp
                | MouseButton::WheelDown
                | MouseButton::WheelLeft
                | MouseButton::WheelRight
        )
    }

    fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::None => 3,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
            MouseButton::WheelLeft => 66,
            MouseButton::WheelRight => 67,
            MouseButton::Back => 128,
            MouseButton::Forward => 129,
        }
    }

    fn held_bit(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Middle => 2,
            MouseButton::Right => 4,
            MouseButton::Back => 8,
            MouseButton::Forward => 16,
            _ => 0,
        }
    }

    const HELD: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::Back,
        MouseButton::Forward,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    /// The pointer moved. With a held button this is a drag.
    Move,
}

/// A pointer event in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseEventKind,
    /// The button that changed. For [`MouseEventKind::Move`] and releases without a known
    /// button use [`MouseButton::None`]: the terminal then uses the button it saw pressed.
    pub button: MouseButton,
    pub mods: Mods,
    /// Zero-based cell column.
    pub col: usize,
    /// Zero-based cell row.
    pub row: usize,
    /// Pixels from the left edge of the cell, for mode 1016.
    pub dx: u32,
    /// Pixels from the top edge of the cell, for mode 1016.
    pub dy: u32,
}

impl MouseEvent {
    pub fn new(kind: MouseEventKind, button: MouseButton, col: usize, row: usize) -> Self {
        Self {
            kind,
            button,
            mods: Mods::NONE,
            col,
            row,
            dx: 0,
            dy: 0,
        }
    }
}

/// Mouse modes and what the encoder remembers between events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseState {
    pub tracking: MouseTracking,
    pub encoding: MouseEncoding,
    held: u8,
    last_held: Option<MouseButton>,
    last_pos: Option<(u32, u32)>,
}

impl MouseState {
    /// Forgets held buttons and the last position (after a reset).
    pub fn clear_history(&mut self) {
        self.held = 0;
        self.last_held = None;
        self.last_pos = None;
    }

    fn press(&mut self, b: MouseButton) {
        if b.held_bit() != 0 {
            self.held |= b.held_bit();
            self.last_held = Some(b);
        }
    }

    fn release(&mut self, b: MouseButton) {
        self.held &= !b.held_bit();
        if self.last_held == Some(b) {
            self.last_held = MouseButton::HELD
                .into_iter()
                .find(|h| self.held & h.held_bit() != 0);
        }
    }
}

/// Encodes a pointer event. `size` is `(rows, cols)` and `cell_px` the cell size in pixels
/// (zero when unknown). Returns whether bytes were written.
///
/// The state is updated even when tracking is off, so a drag that starts before the
/// program turns tracking on still has its button.
pub fn encode_mouse(
    state: &mut MouseState,
    ev: &MouseEvent,
    size: (usize, usize),
    cell_px: (u32, u32),
    out: &mut Vec<u8>,
) -> bool {
    let col = ev.col.min(size.1.saturating_sub(1)) as u32;
    let row = ev.row.min(size.0.saturating_sub(1)) as u32;
    let wheel = ev.button.is_wheel();

    let button = match ev.kind {
        MouseEventKind::Press => ev.button,
        _ if ev.button != MouseButton::None => ev.button,
        _ => state.last_held.unwrap_or(MouseButton::None),
    };
    if ev.kind == MouseEventKind::Press && button == MouseButton::None {
        return false;
    }
    match ev.kind {
        MouseEventKind::Press => state.press(button),
        MouseEventKind::Release => state.release(button),
        MouseEventKind::Move => {}
    }
    if state.tracking == MouseTracking::Off {
        return false;
    }

    let mut mods = 0;
    if state.tracking != MouseTracking::X10 {
        let m = ev.mods;
        mods = u32::from(m.contains(Mods::SHIFT)) * 4
            + u32::from(m.contains(Mods::ALT)) * 8
            + u32::from(m.contains(Mods::CTRL)) * 16;
    }
    let sgr = matches!(
        state.encoding,
        MouseEncoding::Sgr | MouseEncoding::SgrPixels
    );

    let (code, release) = match ev.kind {
        MouseEventKind::Press => {
            if state.tracking == MouseTracking::X10 && (wheel || button.code() > 2) {
                return false;
            }
            (button.code() + mods, false)
        }
        MouseEventKind::Release => {
            if wheel || state.tracking == MouseTracking::X10 || (sgr && button == MouseButton::None)
            {
                return false;
            }
            if sgr {
                (button.code() + mods, true)
            } else {
                (3 + mods, true)
            }
        }
        MouseEventKind::Move => {
            let drag = match state.tracking {
                MouseTracking::ButtonEvent | MouseTracking::AnyEvent => {
                    if button == MouseButton::None && state.tracking != MouseTracking::AnyEvent {
                        return false;
                    }
                    button
                }
                _ => return false,
            };
            if wheel {
                return false;
            }
            (32 + drag.code() + mods, false)
        }
    };

    let pixels = state.encoding == MouseEncoding::SgrPixels && cell_px.0 > 0 && cell_px.1 > 0;
    let (x, y) = if pixels {
        (col * cell_px.0 + ev.dx, row * cell_px.1 + ev.dy)
    } else {
        (col, row)
    };
    if ev.kind == MouseEventKind::Move && state.last_pos == Some((x, y)) {
        return false;
    }

    let start = out.len();
    match state.encoding {
        MouseEncoding::X10 | MouseEncoding::Utf8 => {
            let utf8 = state.encoding == MouseEncoding::Utf8;
            let limit = if utf8 { 0x7ff } else { 0xff };
            let (bx, by) = (x + 33, y + 33);
            if code + 32 > limit || bx > limit || by > limit {
                return false;
            }
            out.extend_from_slice(b"\x1b[M");
            for v in [code + 32, bx, by] {
                push_coord(out, v, utf8);
            }
        }
        MouseEncoding::Urxvt => {
            out.extend_from_slice(b"\x1b[");
            push_dec(out, code + 32);
            out.push(b';');
            push_dec(out, x + 1);
            out.push(b';');
            push_dec(out, y + 1);
            out.push(b'M');
        }
        MouseEncoding::Sgr | MouseEncoding::SgrPixels => {
            out.extend_from_slice(b"\x1b[<");
            push_dec(out, code);
            out.push(b';');
            push_dec(out, x + 1);
            out.push(b';');
            push_dec(out, y + 1);
            out.push(if release { b'm' } else { b'M' });
        }
    }
    state.last_pos = Some((x, y));
    out.len() > start
}

fn push_coord(out: &mut Vec<u8>, v: u32, utf8: bool) {
    if utf8 && v >= 0x80 {
        out.push(0xc0 | (v >> 6) as u8);
        out.push(0x80 | (v & 0x3f) as u8);
    } else {
        out.push(v as u8);
    }
}

/// Alternate scroll (mode 1007): a wheel notch becomes one cursor key. The caller checks
/// that the alternate screen is active, the mode is on and the mouse is not grabbed.
pub fn encode_alt_scroll(modes: &KeyModes, button: MouseButton, out: &mut Vec<u8>) -> bool {
    let key = match button {
        MouseButton::WheelUp => KeyCode::Up,
        MouseButton::WheelDown => KeyCode::Down,
        MouseButton::WheelLeft => KeyCode::Left,
        MouseButton::WheelRight => KeyCode::Right,
        _ => return false,
    };
    let plain = KeyModes {
        kitty_flags: 0,
        modify_other_keys: 0,
        ..*modes
    };
    encode_key(
        &plain,
        key,
        Mods::NONE,
        super::key::KeyEventKind::Press,
        None,
        out,
    )
}
