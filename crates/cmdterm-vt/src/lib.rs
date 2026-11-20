//! Terminal emulator core for scriptschnell-term.
//!
//! The crate does no I/O and uses no unsafe code. It has the byte-level [`Parser`] (the
//! DEC ANSI state machine with UTF-8 decoding in ground state), the [`Perform`] trait it
//! drives, and the [`Terminal`]: a screen of fixed cells with interned styles, rows that
//! carry a sequence number, a scrollback, and the core VT behavior. DEC modes and the input encoders
//! (keys with the kitty protocol, mouse, paste, focus) are in [`input`]. OSCs (titles,
//! palette, hyperlinks, directory, clipboard, notifications, prompt marks) and the device
//! reports are in [`osc`] and the terminal. Cells hold grapheme clusters ([`grapheme`]).
//!
//! The terminal never writes to the program. Replies collect in a buffer
//! ([`Terminal::take_replies`]) and events in a queue ([`Terminal::drain_events`]).

pub mod cell;
pub mod charset;
pub mod color;
pub mod event;
pub mod grapheme;
pub mod input;
pub mod osc;
pub mod parser;
mod reflow;
mod scan;
pub mod screen;
pub mod style;
pub mod terminal;
pub mod utf8;

pub use cell::{Cell, ClusterId, Width};
pub use charset::Charset;
pub use color::{Color, Palette, Rgb};
pub use event::Event;
pub use input::{
    KeyCode, KeyEventKind, KeyModes, KeypadKey, MediaKey, ModifierKey, Mods, MouseButton,
    MouseEncoding, MouseEvent, MouseEventKind, MouseTracking, encode_focus, encode_key,
    encode_paste,
};
pub use osc::{CurrentDir, Progress, SemanticKind, SemanticZone};
pub use parser::{
    MAX_INTERMEDIATES, MAX_PARAMS, OSC_MAX_LEN, OscEnd, Params, ParamsIter, Parser, Perform,
    osc_split,
};
pub use screen::{Region, Row, Screen};
pub use style::{Style, StyleId, StyleTable, Underline, flags};
pub use terminal::{Config, Cursor, CursorShape, Modes, Size, Terminal};
