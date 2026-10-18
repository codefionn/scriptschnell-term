//! Input encoders: keys, mouse, paste and focus.
//!
//! Nothing here does I/O. Each encoder appends bytes to a caller-provided `Vec<u8>` and
//! does not allocate once that buffer has grown to the size it needs. The functions take
//! the terminal modes they depend on as plain structs; [`Terminal`](crate::Terminal) has
//! thin methods that fill them from its own state.

pub mod key;
pub mod mouse;
pub mod paste;

pub use key::{KeyCode, KeyEventKind, KeyModes, KeypadKey, MediaKey, ModifierKey, encode_key};
pub use mouse::{
    MouseButton, MouseEncoding, MouseEvent, MouseEventKind, MouseTracking, encode_alt_scroll,
};
pub use paste::{encode_focus, encode_paste};

/// Modifier keys held during an input event.
///
/// The bits are the ones the kitty keyboard protocol sends (value minus one), so the
/// encoder can use them unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const SHIFT: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const CTRL: Mods = Mods(4);
    pub const SUPER: Mods = Mods(8);
    pub const HYPER: Mods = Mods(16);
    pub const META: Mods = Mods(32);
    pub const CAPS_LOCK: Mods = Mods(64);
    pub const NUM_LOCK: Mods = Mods(128);

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn from_bits(bits: u8) -> Mods {
        Mods(bits)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Mods) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn without(self, other: Mods) -> Mods {
        Mods(self.0 & !other.0)
    }

    /// The same set without caps lock and num lock.
    pub const fn without_locks(self) -> Mods {
        Mods(self.0 & 0x3f)
    }
}

impl std::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, rhs: Mods) -> Mods {
        Mods(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Mods {
    fn bitor_assign(&mut self, rhs: Mods) {
        self.0 |= rhs.0;
    }
}

/// Appends `v` in decimal.
pub(crate) fn push_dec(out: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

/// Appends `c` as UTF-8.
pub(crate) fn push_char(out: &mut Vec<u8>, c: char) {
    let mut b = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
}
